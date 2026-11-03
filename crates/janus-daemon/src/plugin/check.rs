//! `janus plugin check` / `POST /v1/plugin/check`: test the channel actively
//! (`docs/plugin-contract#channel-check`).
//!
//! On the network transport: a full handshake with the configured material,
//! then two the plugin must *reject* — one presenting a throwaway
//! self-signed client certificate, one presenting none. Under TLS 1.3 the
//! server's verdict on a client certificate arrives after the client's side
//! of the handshake has finished, so each probe writes the HTTP/2 client
//! preface and an empty SETTINGS frame and reads: a TLS alert (or the
//! connection closing) is a rejection, the server's SETTINGS an acceptance.
//! No probe issues a `ResolveRoute`.
//!
//! On the socket transport: the path-permission walk and a connection
//! attempt. On the plaintext transport: a connection attempt.

use std::time::Duration;

use janus_core::contract::plugin::{PluginError, PluginFailureClass, PluginMode};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::grpc::{Dialer, Target};
use super::socket::{self, PathWalk};
use super::tls::{self, ClientIdentity, TlsDialer};
use super::tls_init;
use super::transport::connect_tcp;

/// How long a probe waits for the plugin's verdict after its handshake.
pub const PROBE_WAIT: Duration = Duration::from_secs(2);

/// The HTTP/2 client connection preface followed by an empty SETTINGS
/// frame: what makes an HTTP/2 server answer with its own SETTINGS.
const H2_PROBE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00";

/// Which check a step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStepKind {
    /// Socket transport: the directory walk.
    PathPermissions,
    /// Socket and plaintext transports: a connection attempt.
    Connect,
    /// TLS: a full handshake with the configured material, accepted.
    Handshake,
    /// TLS: a throwaway self-signed client certificate, which must be
    /// rejected.
    ThrowawayCertRejected,
    /// TLS: no client certificate, which must be rejected.
    NoCertRejected,
}

/// One step's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckStep {
    pub step: CheckStepKind,
    pub passed: bool,
    /// What was observed, for the operator.
    pub detail: String,
    /// The failure class, when the step failed the way a call would.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<PluginFailureClass>,
    /// Socket transport: the directories examined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub walk: Option<PathWalk>,
}

impl CheckStep {
    fn pass(step: CheckStepKind, detail: impl Into<String>) -> Self {
        CheckStep {
            step,
            passed: true,
            detail: detail.into(),
            class: None,
            walk: None,
        }
    }

    fn fail(step: CheckStepKind, detail: impl Into<String>) -> Self {
        CheckStep {
            passed: false,
            ..CheckStep::pass(step, detail)
        }
    }

    fn failed_call(step: CheckStepKind, err: PluginError) -> Self {
        CheckStep {
            class: Some(err.class),
            ..CheckStep::fail(step, err.message)
        }
    }
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChannelCheck {
    pub mode: PluginMode,
    pub endpoint: Option<String>,
    /// Every step passed.
    pub passed: bool,
    pub steps: Vec<CheckStep>,
}

impl ChannelCheck {
    fn new(mode: PluginMode, endpoint: Option<String>, steps: Vec<CheckStep>) -> Self {
        ChannelCheck {
            mode,
            endpoint,
            passed: steps.iter().all(|s| s.passed),
            steps,
        }
    }

    /// The linked model has no channel to check.
    pub fn linked() -> Self {
        ChannelCheck::new(PluginMode::Linked, None, Vec::new())
    }

    /// The step of kind `step`, if it ran.
    pub fn step(&self, step: CheckStepKind) -> Option<&CheckStep> {
        self.steps.iter().find(|s| s.step == step)
    }
}

pub(crate) async fn run(dialer: &Dialer, mode: PluginMode, endpoint: &str) -> ChannelCheck {
    let steps = match &dialer.target {
        Target::Unix(path) => check_socket(path).await,
        Target::Tls(tls) => check_tls(tls).await,
        Target::Plaintext(authority) => vec![match connect_tcp(authority.as_str(), authority).await {
            Ok(_) => CheckStep::pass(
                CheckStepKind::Connect,
                format!("connected to {authority}; plaintext: the channel is not authenticated by Janus"),
            ),
            Err(e) => CheckStep::failed_call(CheckStepKind::Connect, e),
        }],
    };
    ChannelCheck::new(mode, Some(endpoint.to_owned()), steps)
}

async fn check_socket(path: &std::path::Path) -> Vec<CheckStep> {
    let walk = match socket::check_full(path) {
        Ok(walk) => walk,
        Err(e) => {
            return vec![
                CheckStep::failed_call(
                    CheckStepKind::PathPermissions,
                    PluginError::new(PluginFailureClass::Unreachable, e.to_string()),
                ),
                CheckStep::fail(
                    CheckStepKind::Connect,
                    "not attempted: the socket path failed the permission walk",
                ),
            ]
        }
    };
    let connect = match walk.connect().await {
        Ok(_) => CheckStep::pass(
            CheckStepKind::Connect,
            format!(
                "connected to {}, served by an allowed uid",
                walk.socket.display()
            ),
        ),
        Err(e) => CheckStep::failed_call(CheckStepKind::Connect, e),
    };
    let mut perms = CheckStep::pass(
        CheckStepKind::PathPermissions,
        format!(
            "{} directories checked: none writable by group or others, all owned by root, the daemon's user or the socket directory's owner",
            walk.dirs.len()
        ),
    );
    perms.walk = Some(walk);
    vec![perms, connect]
}

/// What the plugin did after a completed handshake.
enum Verdict {
    /// It answered with HTTP/2 SETTINGS.
    Accepted,
    /// It refused the client certificate with a TLS alert.
    Rejected(String),
    /// It closed the connection without a verdict.
    Closed,
    /// Something else went wrong, or nothing arrived in time.
    Failed(PluginError),
}

async fn observe(mut hs: tls::Handshake) -> Verdict {
    // The write can fail if the plugin already refused and closed; the
    // read below is what reports the verdict.
    let _ = hs.stream.write_all(H2_PROBE).await;
    let _ = hs.stream.flush().await;
    let mut buf = [0u8; 64];
    match tokio::time::timeout(PROBE_WAIT, hs.stream.read(&mut buf)).await {
        Err(_) => Verdict::Failed(PluginError::new(
            PluginFailureClass::Timeout,
            format!(
                "no response from the plugin within {} ms of the handshake",
                PROBE_WAIT.as_millis()
            ),
        )),
        Ok(Ok(0)) => Verdict::Closed,
        Ok(Ok(_)) => Verdict::Accepted,
        Ok(Err(e)) => {
            let err = tls::classify_post_handshake(&e);
            match err.class {
                PluginFailureClass::RejectedClientCert => Verdict::Rejected(err.message),
                _ => Verdict::Failed(err),
            }
        }
    }
}

async fn configured_handshake(tls: &TlsDialer) -> CheckStep {
    let step = CheckStepKind::Handshake;
    let hs = match tls.handshake(&ClientIdentity::Configured).await {
        Ok(hs) => hs,
        Err(e) => return CheckStep::failed_call(step, e),
    };
    if !hs.client_auth_requested {
        return CheckStep::failed_call(
            step,
            PluginError::new(
                PluginFailureClass::NoClientAuth,
                "the plugin did not request a client certificate; it must verify the daemon",
            ),
        );
    }
    match observe(hs).await {
        Verdict::Accepted => CheckStep::pass(
            step,
            "TLS 1.3 handshake completed: the plugin's certificate verified, and it requested and accepted the daemon's",
        ),
        Verdict::Rejected(message) => CheckStep::failed_call(
            step,
            PluginError::new(PluginFailureClass::RejectedClientCert, message),
        ),
        Verdict::Closed => CheckStep::failed_call(
            step,
            PluginError::new(
                PluginFailureClass::Unreachable,
                "the plugin closed the connection after the handshake",
            ),
        ),
        Verdict::Failed(e) => CheckStep::failed_call(step, e),
    }
}

/// A handshake the plugin must refuse.
async fn must_reject(tls: &TlsDialer, step: CheckStepKind, presented: ClientIdentity) -> CheckStep {
    let what = match presented {
        ClientIdentity::Absent => "no client certificate",
        _ => "a throwaway self-signed client certificate",
    };
    let hs = match tls.handshake(&presented).await {
        Ok(hs) => hs,
        Err(e) => {
            return CheckStep::failed_call(
                step,
                PluginError::new(e.class, format!("inconclusive: {}", e.message)),
            )
        }
    };
    let requested = hs.client_auth_requested;
    match observe(hs).await {
        Verdict::Rejected(message) => {
            CheckStep::pass(step, format!("the plugin refused {what}: {message}"))
        }
        Verdict::Closed => CheckStep::pass(
            step,
            format!("the plugin closed the connection presenting {what}"),
        ),
        Verdict::Accepted if !requested => CheckStep::fail(
            step,
            format!("the plugin accepted {what}: it does not request a client certificate at all"),
        ),
        Verdict::Accepted => CheckStep::fail(
            step,
            match presented {
                ClientIdentity::Absent => format!(
                    "the plugin accepted {what}: it requests a certificate without requiring one"
                ),
                _ => {
                    format!("the plugin accepted {what}: its client verification is misconfigured")
                }
            },
        ),
        Verdict::Failed(e) => CheckStep::failed_call(
            step,
            PluginError::new(e.class, format!("inconclusive: {}", e.message)),
        ),
    }
}

async fn check_tls(tls: &TlsDialer) -> Vec<CheckStep> {
    let handshake = configured_handshake(tls).await;
    let throwaway = match tls_init::throwaway_client_key() {
        Ok(key) => {
            must_reject(
                tls,
                CheckStepKind::ThrowawayCertRejected,
                ClientIdentity::Other(key),
            )
            .await
        }
        Err(e) => CheckStep::fail(
            CheckStepKind::ThrowawayCertRejected,
            format!("cannot generate the throwaway certificate: {e}"),
        ),
    };
    let no_cert = must_reject(tls, CheckStepKind::NoCertRejected, ClientIdentity::Absent).await;
    vec![handshake, throwaway, no_cert]
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use super::*;
    use crate::contract::config::PluginTransport;
    use crate::plugin::grpc::GrpcPluginClient;
    use crate::plugin::testutil::{
        closed_port, serve_unix, silent_listener, socket_dir, ClientAuth, FakePlugin, TlsFixture,
    };
    use crate::plugin::transport::CONNECT_TIMEOUT;

    /// Run the check against a TLS plugin with `client_auth`.
    async fn check_tls_plugin(client_auth: impl FnOnce(&TlsFixture) -> ClientAuth) -> ChannelCheck {
        let fx = TlsFixture::new();
        let plugin = FakePlugin::credential("c");
        let (_server, addr) = fx.serve(&plugin, client_auth(&fx)).await;
        let check = fx.client(addr).check().await;
        assert_eq!(plugin.request_count(), 0, "no probe issues a ResolveRoute");
        check
    }

    fn passed(check: &ChannelCheck, kind: CheckStepKind) -> bool {
        check.step(kind).expect("step ran").passed
    }

    #[tokio::test]
    async fn check_passes_against_a_strict_plugin() {
        let check = check_tls_plugin(TlsFixture::strict).await;
        assert!(check.passed, "{check:#?}");
        assert_eq!(check.mode, PluginMode::Tls);
        assert_eq!(check.steps.len(), 3);
        let json = serde_json::to_value(&check).expect("json");
        assert_eq!(json["steps"][1]["step"], "throwaway_cert_rejected");
        assert_eq!(json["steps"][2]["step"], "no_cert_rejected");
    }

    #[tokio::test]
    async fn check_flags_a_plugin_accepting_any_certificate() {
        let check = check_tls_plugin(|_| ClientAuth::AcceptAny).await;
        assert!(!check.passed);
        assert!(passed(&check, CheckStepKind::Handshake));
        assert!(!passed(&check, CheckStepKind::ThrowawayCertRejected));
        assert!(passed(&check, CheckStepKind::NoCertRejected));
    }

    #[tokio::test]
    async fn check_flags_optional_client_auth() {
        let check = check_tls_plugin(|fx| ClientAuth::Optional(vec![fx.ca.der()])).await;
        assert!(passed(&check, CheckStepKind::Handshake));
        assert!(passed(&check, CheckStepKind::ThrowawayCertRejected));
        let no_cert = check.step(CheckStepKind::NoCertRejected).expect("ran");
        assert!(!no_cert.passed);
        assert!(no_cert.detail.contains("without requiring"), "{no_cert:?}");
    }

    #[tokio::test]
    async fn check_flags_a_plugin_without_client_auth() {
        let check = check_tls_plugin(|_| ClientAuth::Off).await;
        let handshake = check.step(CheckStepKind::Handshake).expect("ran");
        assert_eq!(handshake.class, Some(PluginFailureClass::NoClientAuth));
        for kind in [
            CheckStepKind::ThrowawayCertRejected,
            CheckStepKind::NoCertRejected,
        ] {
            let step = check.step(kind).expect("ran");
            assert!(!step.passed);
            assert!(step.detail.contains("does not request"), "{step:?}");
        }
    }

    #[tokio::test]
    async fn check_is_bounded_against_a_plugin_that_never_answers() {
        // Accepts TCP, then never speaks: every probe's handshake stalls.
        let (_listener, addr) = silent_listener().await;
        let fx = TlsFixture::new();
        let check = tokio::time::timeout(
            CONNECT_TIMEOUT * 3 + Duration::from_secs(3),
            fx.client(addr).check(),
        )
        .await
        .expect("plugin check must not hang");
        assert!(!check.passed);
        let handshake = check.step(CheckStepKind::Handshake).expect("ran");
        assert_eq!(
            handshake.class,
            Some(PluginFailureClass::TlsError),
            "{handshake:?}"
        );
        assert!(
            handshake.detail.contains("did not complete within"),
            "{handshake:?}"
        );
        for kind in [
            CheckStepKind::ThrowawayCertRejected,
            CheckStepKind::NoCertRejected,
        ] {
            let step = check.step(kind).expect("ran");
            assert!(
                !step.passed && step.detail.contains("inconclusive"),
                "{step:?}"
            );
        }
    }

    #[tokio::test]
    async fn check_socket_walks_then_connects_and_plaintext_connects() {
        let (dir, path) = socket_dir();
        let plugin = FakePlugin::credential("c");
        let _server = serve_unix(&plugin, &path);
        let client = GrpcPluginClient::new(
            &PluginTransport::Unix { path: path.clone() },
            Duration::from_secs(1),
        )
        .expect("client");
        let check = client.check().await;
        assert!(check.passed, "{check:#?}");
        let walk = check.step(CheckStepKind::PathPermissions).expect("ran");
        assert!(walk.walk.as_ref().is_some_and(|w| !w.dirs.is_empty()));

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o770))
            .expect("chmod");
        let check = client.check().await;
        assert!(!passed(&check, CheckStepKind::PathPermissions));
        let connect = check.step(CheckStepKind::Connect).expect("ran");
        assert!(connect.detail.contains("not attempted"), "{connect:?}");

        let endpoint = url::Url::parse(&format!("http://{}", closed_port().await)).expect("url");
        let client = GrpcPluginClient::new(
            &PluginTransport::Plaintext { endpoint },
            Duration::from_secs(1),
        )
        .expect("client");
        let check = client.check().await;
        let connect = check.step(CheckStepKind::Connect).expect("ran");
        assert_eq!(connect.class, Some(PluginFailureClass::Unreachable));
        assert!(ChannelCheck::linked().passed);
    }
}
