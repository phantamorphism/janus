//! `janus plugin tls init`: bootstrap the TLS plugin transport without a PKI
//! (`docs/plugin-contract#channel-bootstrap`).
//!
//! Generates two self-signed key pairs — the daemon's, and the plugin's
//! carrying `--plugin-host` as its SAN — and writes `daemon.pem`,
//! `daemon-key.pem`, `plugin.pem` and `plugin-key.pem`. Each side pins the
//! other: no CA, no chain. Private keys are written mode `0600` (the docs
//! require it of `daemon-key.pem`; `plugin-key.pem` is a private key too),
//! certificates `0644`. Files are written to a temporary name and renamed,
//! so a rerun — which is how rotation works — replaces them whole.
//!
//! The returned [`TlsInitOutput`] carries the `[plugin.tls]` block to print
//! and the two facts the plugin's own configuration needs.

use std::fmt;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Datelike, Utc};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;

use super::tls::provider;

/// `--days` when not given.
pub const DEFAULT_DAYS: u32 = 365;

/// File names written into `--out`.
pub const DAEMON_CERT: &str = "daemon.pem";
pub const DAEMON_KEY: &str = "daemon-key.pem";
pub const PLUGIN_CERT: &str = "plugin.pem";
pub const PLUGIN_KEY: &str = "plugin-key.pem";

/// What `janus plugin tls init` was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsInitOptions {
    /// `--plugin-host`: the plugin's SAN (a DNS name or an IP address).
    pub plugin_host: String,
    /// `--out`: the directory to write into (created if missing).
    pub out_dir: PathBuf,
    /// `--days`: validity of both certificates, from today (UTC).
    pub days: u32,
}

/// What was written, and what to tell the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsInitOutput {
    pub daemon_cert: PathBuf,
    pub daemon_key: PathBuf,
    pub plugin_cert: PathBuf,
    pub plugin_key: PathBuf,
    /// The `[plugin.tls]` block that pins `plugin.pem`.
    pub config_block: String,
    /// The plugin's side: serve with `plugin.pem` / `plugin-key.pem`, and
    /// accept only clients whose certificate is `daemon.pem`.
    pub plugin_instructions: String,
}

impl fmt::Display for TlsInitOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Add to the daemon's config, under [plugin]:\n")?;
        writeln!(f, "{}", self.config_block)?;
        write!(f, "{}", self.plugin_instructions)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TlsInitError {
    #[error("--plugin-host must not be empty")]
    EmptyHost,
    #[error("--plugin-host {0:?} is not a valid DNS name or IP address")]
    InvalidHost(String),
    #[error("--days must be at least 1")]
    InvalidDays,
    #[error("--days {0} puts the expiry beyond the year 9999")]
    TooManyDays(u32),
    #[error("cannot generate a certificate: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("cannot write {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> TlsInitError + '_ {
    move |source| TlsInitError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// A freshly generated self-signed certificate and its key.
struct SelfSigned {
    cert_pem: String,
    cert_der: CertificateDer<'static>,
    key: KeyPair,
}

/// Validity of `days` days starting today (UTC midnight).
fn validity(params: &mut CertificateParams, days: u32) -> Result<(), TlsInitError> {
    let today = Utc::now().date_naive();
    let until = today
        .checked_add_days(chrono::Days::new(u64::from(days)))
        .filter(|d| d.year() <= 9999)
        .ok_or(TlsInitError::TooManyDays(days))?;
    // Month and day are always in range for a valid date.
    let ymd = |d: chrono::NaiveDate| rcgen::date_time_ymd(d.year(), d.month() as u8, d.day() as u8);
    params.not_before = ymd(today);
    params.not_after = ymd(until);
    Ok(())
}

fn self_signed(
    common_name: &str,
    sans: Vec<String>,
    usage: ExtendedKeyUsagePurpose,
    days: u32,
) -> Result<SelfSigned, TlsInitError> {
    let mut params = CertificateParams::new(sans)?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    params.distinguished_name = dn;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![usage];
    validity(&mut params, days)?;
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok(SelfSigned {
        cert_pem: cert.pem(),
        cert_der: cert.der().clone(),
        key,
    })
}

/// Write `contents` to `path` with `mode`, atomically (temp file + rename).
fn write_file(path: &Path, contents: &str, mode: u32) -> Result<(), TlsInitError> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{file_name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(io_err(&tmp))?;
    // The process umask may have narrowed the mode; make it exact.
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(io_err(&tmp))?;
    file.write_all(contents.as_bytes()).map_err(io_err(&tmp))?;
    file.sync_all().map_err(io_err(&tmp))?;
    std::fs::rename(&tmp, path).map_err(io_err(path))?;
    Ok(())
}

fn toml_string(path: &Path) -> String {
    toml::Value::String(path.display().to_string()).to_string()
}

/// Generate and write the four files.
pub fn tls_init(opts: &TlsInitOptions) -> Result<TlsInitOutput, TlsInitError> {
    let host = opts.plugin_host.trim();
    if host.is_empty() {
        return Err(TlsInitError::EmptyHost);
    }
    if opts.days == 0 {
        return Err(TlsInitError::InvalidDays);
    }
    if rustls::pki_types::ServerName::try_from(host).is_err() {
        return Err(TlsInitError::InvalidHost(host.to_owned()));
    }
    let daemon = self_signed(
        "janus-daemon",
        Vec::new(),
        ExtendedKeyUsagePurpose::ClientAuth,
        opts.days,
    )?;
    let plugin = self_signed(
        host,
        vec![host.to_owned()],
        ExtendedKeyUsagePurpose::ServerAuth,
        opts.days,
    )?;

    std::fs::create_dir_all(&opts.out_dir).map_err(io_err(&opts.out_dir))?;
    let out = std::fs::canonicalize(&opts.out_dir).map_err(io_err(&opts.out_dir))?;
    let output = TlsInitOutput {
        daemon_cert: out.join(DAEMON_CERT),
        daemon_key: out.join(DAEMON_KEY),
        plugin_cert: out.join(PLUGIN_CERT),
        plugin_key: out.join(PLUGIN_KEY),
        config_block: String::new(),
        plugin_instructions: String::new(),
    };
    write_file(&output.daemon_key, &daemon.key.serialize_pem(), 0o600)?;
    write_file(&output.daemon_cert, &daemon.cert_pem, 0o644)?;
    write_file(&output.plugin_key, &plugin.key.serialize_pem(), 0o600)?;
    write_file(&output.plugin_cert, &plugin.cert_pem, 0o644)?;

    let config_block = format!(
        "[plugin.tls]\npinned_cert = {}\nclient_cert = {}\nclient_key  = {}\n",
        toml_string(&output.plugin_cert),
        toml_string(&output.daemon_cert),
        toml_string(&output.daemon_key),
    );
    let plugin_instructions = format!(
        "For the plugin:\n  - serve TLS 1.3 with certificate {} and key {}\n  - require a client certificate, and accept only {} (pin it; there is no CA)\n",
        output.plugin_cert.display(),
        output.plugin_key.display(),
        output.daemon_cert.display(),
    );
    Ok(TlsInitOutput {
        config_block,
        plugin_instructions,
        ..output
    })
}

/// A throwaway self-signed client identity, for the `plugin check` probe
/// the plugin must reject. Never written to disk.
pub(crate) fn throwaway_client_key() -> Result<Arc<CertifiedKey>, String> {
    let generated = self_signed(
        "janus-plugin-check-throwaway",
        Vec::new(),
        ExtendedKeyUsagePurpose::ClientAuth,
        1,
    )
    .map_err(|e| e.to_string())?;
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(generated.key.serialize_der()));
    CertifiedKey::from_der(vec![generated.cert_der], key, &provider())
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    use crate::contract::config::{PluginConfig, PluginTransport, TrustAnchor};

    #[test]
    fn tls_init_writes_four_files_with_documented_modes_and_san() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let out_dir = tmp.path().join("certs");
        let opts = TlsInitOptions {
            plugin_host: "policy.internal".into(),
            out_dir: out_dir.clone(),
            days: 30,
        };
        let out = tls_init(&opts).expect("generated");
        let mode = |p: &Path| std::fs::metadata(p).expect("stat").mode() & 0o777;
        assert_eq!(mode(&out.daemon_key), 0o600);
        assert_eq!(mode(&out.plugin_key), 0o600);
        assert_eq!(mode(&out.daemon_cert), 0o644);
        assert_eq!(mode(&out.plugin_cert), 0o644);

        let plugin_pem = std::fs::read(&out.plugin_cert).expect("read");
        let (_, pem) = x509_parser::pem::parse_x509_pem(&plugin_pem).expect("pem");
        let cert = pem.parse_x509().expect("x509");
        let san = cert
            .subject_alternative_name()
            .expect("ext")
            .expect("has SAN");
        assert!(san.value.general_names.iter().any(|n| matches!(
            n,
            x509_parser::extensions::GeneralName::DNSName("policy.internal")
        )));
        let lifetime =
            cert.validity().not_after.timestamp() - cert.validity().not_before.timestamp();
        assert_eq!(lifetime, 30 * 24 * 3600);
        assert_eq!(cert.issuer(), cert.subject(), "self-signed");

        // The printed block is a valid [plugin.tls] that pins plugin.pem.
        let config: toml::Table = toml::from_str(&format!(
            "[plugin]\nendpoint = \"https://policy.internal:7443\"\n{}",
            out.config_block
        ))
        .expect("toml");
        let plugin: PluginConfig = config["plugin"].clone().try_into().expect("plugin block");
        match plugin.transport().expect("transport") {
            PluginTransport::Tls {
                trust, client_key, ..
            } => {
                assert_eq!(trust, TrustAnchor::PinnedCert(out.plugin_cert.clone()));
                assert_eq!(client_key, out.daemon_key);
            }
            other => panic!("unexpected transport {other:?}"),
        }
        assert!(out
            .plugin_instructions
            .contains(&out.daemon_cert.display().to_string()));
        assert!(out.to_string().contains("[plugin.tls]"));

        // Rerunning rotates: new material replaces the old.
        let again = tls_init(&opts).expect("rerun");
        assert_ne!(
            std::fs::read(&again.plugin_cert).expect("read"),
            plugin_pem,
            "a rerun writes new certificates"
        );
        assert_eq!(mode(&again.daemon_key), 0o600);
    }

    #[test]
    fn tls_init_refuses_bad_arguments() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = TlsInitOptions {
            plugin_host: "policy.internal".into(),
            out_dir: tmp.path().to_path_buf(),
            days: 365,
        };
        let cases = [
            (
                TlsInitOptions {
                    plugin_host: " ".into(),
                    ..base.clone()
                },
                "empty",
            ),
            (
                TlsInitOptions {
                    plugin_host: "not a host!".into(),
                    ..base.clone()
                },
                "valid DNS",
            ),
            (
                TlsInitOptions {
                    days: 0,
                    ..base.clone()
                },
                "at least 1",
            ),
            (
                TlsInitOptions {
                    days: u32::MAX,
                    ..base.clone()
                },
                "9999",
            ),
        ];
        for (opts, expect) in cases {
            let err = tls_init(&opts).expect_err("refused");
            assert!(err.to_string().contains(expect), "{err}");
        }
        assert_eq!(
            std::fs::read_dir(tmp.path()).expect("ls").count(),
            0,
            "nothing written on refusal"
        );
        let ip = tls_init(&TlsInitOptions {
            plugin_host: "10.0.0.7".into(),
            ..base
        })
        .expect("IP SAN is accepted");
        assert!(ip.plugin_cert.exists());
    }

    #[test]
    fn throwaway_client_key_is_a_usable_identity() {
        let a = throwaway_client_key().expect("generated");
        let b = throwaway_client_key().expect("generated");
        assert_ne!(a.cert[0], b.cert[0], "fresh every time");
        a.keys_match().expect("key matches certificate");
    }
}
