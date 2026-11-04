//! The daemon's configuration file.
//!
//! One TOML file is the only source of daemon settings; its path is the only
//! thing the command line supplies (`docs/plugin-contract#plugin-config`).
//! [`DaemonConfig::from_toml_str`] / [`DaemonConfig::load`] parse it and run
//! [`DaemonConfig::validate`]: the static half of startup validation — every
//! rule the docs state that can be decided from the file alone, plus the
//! refusal of settings that would have no effect where they are written.
//! Checks that need the outside world (files exist and parse, key modes,
//! JWKS reachability, socket-path ownership, linked implementations) run
//! when the daemon constructs its components;
//! [`super::Embedded::check_against`] covers config/linkage agreement.
//!
//! Implementation decisions. The docs show the `[identity]`, `[[upstream]]`,
//! `[plugin]` and `[request_log]` sections but give no contract for the
//! following, which a working daemon needs; they are decided here and in
//! `janus_core::contract::paths`, and the CLI shares them through that
//! module:
//!
//! - top-level `data_dir`, falling back to `$JANUS_DATA_DIR`, then
//!   `$HOME/.janus`;
//! - `[data_plane] bind`, and `bind` / `assets_dir` of `[ui]`;
//! - `[secrets.<name>]` (`env` or `file`): where an `hmac` binding's
//!   `secret_ref` is resolved;
//! - `[[credential]]` (`ref`, `upstream`, `label`, `env` or `file`): the
//!   `env` and `file` credential sources the admin surface reports (pool
//!   slots are discovered from the data directory instead);
//! - upstream routing: a request belongs to the upstream whose
//!   `path_prefix` is a whole-segment prefix of its path. `/` and nested
//!   prefixes are refused, so at most one upstream matches. A route `path`
//!   is a literal path, matched against the request path with the prefix
//!   stripped, both in canonical form (`janus_core::identity`'s route
//!   matching: repeated and trailing `/` and percent-encoded unreserved
//!   characters do not change which binding applies); the docs call it a
//!   pattern without defining a syntax, so wildcard characters are refused
//!   rather than given a meaning, and a path not written canonically is a
//!   startup error.
//!
//! Sections:
//!
//! ```toml
//! data_dir = "/var/lib/janus"            # optional; see janus_core::contract::paths
//!
//! [data_plane]
//! bind = "127.0.0.1:7600"
//! max_connections = 1024                  # default; above zero
//!
//! [admin]                                 # optional; the unix socket always exists
//! bind = "127.0.0.1:7601"                 # opt-in TCP admin listener
//! # allow_non_loopback = true
//! # tls = { cert = "/etc/janus/admin.pem", key = "/etc/janus/admin-key.pem" }
//! # allow_plaintext = true
//!
//! [ui]                                    # optional; absent = no UI listener
//! bind = "127.0.0.1:7602"
//!
//! [request_log]
//! enabled = true                          # default
//! retain  = "30d"                         # default
//!
//! [secrets.caller-identity]               # named secrets bindings refer to
//! file = "/etc/janus/caller-identity.key" # or env = "VAR"
//!
//! [[credential]]                          # env/file credentials; pool slots
//! ref      = "openrouter-main"            # live in the data directory
//! upstream = "openrouter"
//! env      = "OPENROUTER_API_KEY"         # or file = "/path"
//!
//! [identity]  ...                         # docs/caller-identity#binding
//! [[upstream]] ...
//! [plugin]    ...                         # docs/plugin-contract#plugin-config
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use janus_core::contract::binding::{BindingError, BindingSpec, IdentityBindingConfig};
use janus_core::contract::credential::validate_credential_ref;
use janus_core::contract::paths;
use janus_core::contract::plugin::PluginMode;
use janus_core::contract::upstream::UpstreamKind;
use janus_core::proxy::token_url_refusal;
use serde::Deserialize;
use url::Url;

/// `plugin.timeout_ms` when unset.
pub const DEFAULT_PLUGIN_TIMEOUT_MS: u64 = 250;
/// `data_plane.max_connections` when unset.
pub const DEFAULT_DATA_PLANE_MAX_CONNECTIONS: usize = 1024;
/// `request_log.retain` when unset: 30 days.
pub const DEFAULT_REQUEST_LOG_RETAIN: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The whole config file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Data directory; resolved with [`DaemonConfig::data_dir`].
    pub data_dir: Option<PathBuf>,
    pub data_plane: DataPlaneConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    /// Absent: the UI listener does not exist.
    pub ui: Option<UiConfig>,
    /// Absent: pass-through; zero calls defined in `janus.proto`.
    pub plugin: Option<PluginConfig>,
    #[serde(default)]
    pub request_log: RequestLogConfig,
    /// The daemon default binding (`[identity]`).
    pub identity: Option<IdentityBindingConfig>,
    #[serde(default, rename = "upstream")]
    pub upstreams: Vec<UpstreamConfig>,
    #[serde(default, rename = "credential")]
    pub credentials: Vec<CredentialConfig>,
    /// Named secrets that bindings reference (`secret_ref`).
    #[serde(default)]
    pub secrets: BTreeMap<String, MaterialSource>,
}

/// `[data_plane]`: the proxy listener callers are pointed at.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataPlaneConfig {
    pub bind: SocketAddr,
    /// Open data-plane connections served at once; a connection accepted
    /// beyond this is closed unanswered and counted
    /// (`data_plane.refused_connections` in the status endpoint). An
    /// HTTP/1.1 call holds its connection for its whole life, so this bounds
    /// concurrent HTTP/1.1 calls. Must be above zero.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
}

/// `[admin]`. The unix socket at `<data_dir>/admin.sock` always exists; this
/// section only adds the opt-in TCP listener.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    /// Opt-in TCP admin listener; every request on it needs the admin token.
    pub bind: Option<SocketAddr>,
    /// Required for a non-loopback `bind`.
    #[serde(default)]
    pub allow_non_loopback: bool,
    /// Required for a non-loopback `bind` unless `allow_plaintext`.
    pub tls: Option<ListenerTls>,
    /// Something outside Janus encrypts the hop.
    #[serde(default)]
    pub allow_plaintext: bool,
}

/// `[ui]`: static assets plus the admin API, token-authenticated.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    pub bind: SocketAddr,
    /// Required for a non-loopback `bind` unless `allow_plaintext`.
    pub tls: Option<ListenerTls>,
    #[serde(default)]
    pub allow_plaintext: bool,
    /// Directory holding the built UI (`ui/dist`).
    pub assets_dir: Option<PathBuf>,
}

/// `tls = { cert, key }` on a TCP listener.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerTls {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// `[request_log]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestLogConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Rows older than this are deleted by a periodic job.
    #[serde(default = "default_retain", with = "humantime_serde")]
    pub retain: Duration,
}

impl Default for RequestLogConfig {
    fn default() -> Self {
        RequestLogConfig {
            enabled: true,
            retain: DEFAULT_REQUEST_LOG_RETAIN,
        }
    }
}

fn default_max_connections() -> usize {
    DEFAULT_DATA_PLANE_MAX_CONNECTIONS
}

fn default_true() -> bool {
    true
}

fn default_retain() -> Duration {
    DEFAULT_REQUEST_LOG_RETAIN
}

fn default_timeout_ms() -> u64 {
    DEFAULT_PLUGIN_TIMEOUT_MS
}

/// `{ env = "VAR" }` or `{ file = "/path" }`: where secret material is read
/// from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum MaterialSource {
    Env(String),
    File(PathBuf),
}

/// `[[credential]]`: a credential read from the environment or a file. Pool
/// slots (the `pool` source) are discovered from the data directory, not
/// listed here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    /// The credential's `ref`, as plugins and the admin surface name it.
    #[serde(rename = "ref")]
    pub credential_ref: String,
    pub upstream: UpstreamKind,
    pub label: Option<String>,
    /// Exactly one of `env` / `file`.
    pub env: Option<String>,
    pub file: Option<PathBuf>,
}

impl CredentialConfig {
    /// Where the material is read from. `None` only on a config that failed
    /// [`DaemonConfig::validate`].
    pub fn source(&self) -> Option<MaterialSource> {
        match (&self.env, &self.file) {
            (Some(var), None) => Some(MaterialSource::Env(var.clone())),
            (None, Some(path)) => Some(MaterialSource::File(path.clone())),
            _ => None,
        }
    }
}

/// `[[upstream]]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Which upstream implementation; also the contract's `upstream` value.
    pub name: UpstreamKind,
    /// Data-plane path prefix routed to this upstream, e.g. `/anthropic`.
    pub path_prefix: String,
    pub base_url: Url,
    /// Where this upstream's OAuth credentials are refreshed, in place of the
    /// vendor's endpoint. Only for an upstream with an OAuth refresh
    /// (`anthropic`, `openai`).
    pub token_url: Option<Url>,
    /// `[upstream.identity]`: merged over `[identity]` field by field.
    pub identity: Option<IdentityBindingConfig>,
    /// `[[upstream.route]]`: path-specific bindings.
    #[serde(default, rename = "route")]
    pub routes: Vec<RouteConfig>,
}

/// `[[upstream.route]]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Path pattern within the upstream, after its `path_prefix`.
    pub path: String,
    /// Replaces the resolved upstream binding outright.
    pub identity: IdentityBindingConfig,
}

/// `[plugin]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    /// `unix:///path`, `https://host:port` or (with `allow_plaintext`)
    /// `http://host:port`. Exactly one of `endpoint` / `linked = true`.
    pub endpoint: Option<String>,
    #[serde(default)]
    pub linked: bool,
    /// Deadline on each `ResolveRoute`.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Required next to an `http://` endpoint.
    #[serde(default)]
    pub allow_plaintext: bool,
    /// `[plugin.tls]`: required for an `https://` endpoint.
    pub tls: Option<PluginTlsConfig>,
}

/// `[plugin.tls]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginTlsConfig {
    /// Private CA. Exactly one of `ca_file` / `pinned_cert`.
    pub ca_file: Option<PathBuf>,
    /// The plugin's own certificate.
    pub pinned_cert: Option<PathBuf>,
    pub client_cert: PathBuf,
    /// Must be mode `0600` (checked at startup).
    pub client_key: PathBuf,
    /// SNI / hostname override.
    pub server_name: Option<String>,
    /// Exact URI SAN (SPIFFE) that replaces the hostname check.
    pub expected_san: Option<String>,
}

/// The single operator-named trust anchor for the plugin's certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustAnchor {
    CaFile(PathBuf),
    PinnedCert(PathBuf),
}

/// The plugin execution model a `[plugin]` block selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginTransport {
    /// Compiled in; `Embedded::plugin` must be `Some`.
    Linked,
    /// gRPC over a unix domain socket at this absolute path.
    Unix { path: PathBuf },
    /// gRPC over mutual TLS 1.3.
    Tls {
        endpoint: Url,
        trust: TrustAnchor,
        client_cert: PathBuf,
        client_key: PathBuf,
        server_name: Option<String>,
        expected_san: Option<String>,
    },
    /// gRPC over plaintext HTTP/2, by explicit opt-in.
    Plaintext { endpoint: Url },
}

impl PluginTransport {
    pub fn mode(&self) -> PluginMode {
        match self {
            PluginTransport::Linked => PluginMode::Linked,
            PluginTransport::Unix { .. } => PluginMode::Unix,
            PluginTransport::Tls { .. } => PluginMode::Tls,
            PluginTransport::Plaintext { .. } => PluginMode::Plaintext,
        }
    }
}

impl PluginConfig {
    /// The `ResolveRoute` deadline.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    /// Decide the transport, enforcing the documented `[plugin]` rules:
    /// exactly one of `endpoint` / `linked = true`; `unix://`, `https://`
    /// (with `[plugin.tls]` naming exactly one of `ca_file` / `pinned_cert`)
    /// or `http://` with `allow_plaintext = true`; no other scheme. Also
    /// refused: `timeout_ms = 0` (every call would fail closed), and
    /// `allow_plaintext` or `[plugin.tls]` where they have no effect —
    /// `allow_plaintext` anywhere but next to `http://`, and `[plugin.tls]`
    /// anywhere but next to `https://` (a `unix://`, `http://` or linked
    /// plugin) — the docs' "`[plugin.tls]` belongs to `https://` only".
    pub fn transport(&self) -> Result<PluginTransport, ConfigError> {
        if self.timeout_ms == 0 {
            return Err(invalid(
                "plugin.timeout_ms",
                "must be greater than 0; a zero deadline fails every request as plugin_timeout",
            ));
        }
        let transport = self.select_transport()?;
        if self.allow_plaintext && !matches!(transport, PluginTransport::Plaintext { .. }) {
            return Err(invalid(
                "plugin.allow_plaintext",
                "applies only to an http:// endpoint",
            ));
        }
        if self.tls.is_some() && !matches!(transport, PluginTransport::Tls { .. }) {
            return Err(invalid(
                "plugin.tls",
                "[plugin.tls] belongs to an https:// endpoint only; remove it for a unix://, http:// or linked plugin",
            ));
        }
        Ok(transport)
    }

    fn select_transport(&self) -> Result<PluginTransport, ConfigError> {
        let endpoint = match (&self.endpoint, self.linked) {
            (Some(_), true) => {
                return Err(invalid(
                    "plugin",
                    "set exactly one of `endpoint` or `linked = true`, not both",
                ))
            }
            (None, false) => {
                return Err(invalid(
                    "plugin",
                    "a [plugin] block needs exactly one of `endpoint` or `linked = true`",
                ))
            }
            (None, true) => return Ok(PluginTransport::Linked),
            (Some(e), false) => e,
        };
        let url = Url::parse(endpoint)
            .map_err(|e| invalid("plugin.endpoint", format!("{endpoint:?} is not a URI: {e}")))?;
        let has_host = url.host_str().is_some_and(|h| !h.is_empty());
        match url.scheme() {
            "unix" => {
                if has_host {
                    return Err(invalid(
                        "plugin.endpoint",
                        "a unix endpoint is unix:///absolute/path (no host)",
                    ));
                }
                let decoded = percent_encoding::percent_decode_str(url.path())
                    .decode_utf8()
                    .map_err(|_| invalid("plugin.endpoint", "socket path is not valid UTF-8"))?;
                let path = PathBuf::from(decoded.as_ref());
                if !path.is_absolute() || decoded.len() <= 1 {
                    return Err(invalid(
                        "plugin.endpoint",
                        "a unix endpoint needs an absolute socket path",
                    ));
                }
                Ok(PluginTransport::Unix { path })
            }
            "https" => {
                if !has_host {
                    return Err(invalid("plugin.endpoint", "https endpoint has no host"));
                }
                let tls = self.tls.as_ref().ok_or_else(|| {
                    invalid("plugin.tls", "an https:// endpoint requires [plugin.tls]")
                })?;
                let trust = match (&tls.ca_file, &tls.pinned_cert) {
                    (Some(ca), None) => TrustAnchor::CaFile(ca.clone()),
                    (None, Some(pin)) => TrustAnchor::PinnedCert(pin.clone()),
                    (Some(_), Some(_)) => {
                        return Err(invalid(
                            "plugin.tls",
                            "set exactly one of `ca_file` or `pinned_cert`, not both",
                        ))
                    }
                    (None, None) => {
                        return Err(invalid(
                            "plugin.tls",
                            "set exactly one of `ca_file` or `pinned_cert`; system roots are never used",
                        ))
                    }
                };
                Ok(PluginTransport::Tls {
                    endpoint: url,
                    trust,
                    client_cert: tls.client_cert.clone(),
                    client_key: tls.client_key.clone(),
                    server_name: tls.server_name.clone(),
                    expected_san: tls.expected_san.clone(),
                })
            }
            "http" => {
                if !self.allow_plaintext {
                    return Err(invalid(
                        "plugin.endpoint",
                        "an http:// endpoint is refused unless `allow_plaintext = true` is set",
                    ));
                }
                if !has_host {
                    return Err(invalid("plugin.endpoint", "http endpoint has no host"));
                }
                Ok(PluginTransport::Plaintext { endpoint: url })
            }
            other => Err(invalid(
                "plugin.endpoint",
                format!("scheme {other:?} is not accepted: use unix://, https:// or http://"),
            )),
        }
    }
}

/// The resolved identity bindings of one upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamBindings {
    pub upstream: UpstreamKind,
    /// The upstream-level binding (upstream block merged over the default,
    /// or the default alone). `None`: no binding resolves, so every request
    /// not matched by a route is rejected.
    pub binding: Option<BindingSpec>,
    /// Route bindings, in config order.
    pub routes: Vec<RouteBinding>,
}

impl UpstreamBindings {
    /// Every resolved binding of this upstream paired with its config
    /// location: `upstream[<name>].identity` for the upstream-level binding,
    /// `upstream[<name>].route[<path>].identity` for each route.
    pub fn located(&self) -> Vec<(String, &BindingSpec)> {
        let upstream = self
            .binding
            .iter()
            .map(|b| (format!("upstream[{}].identity", self.upstream), b));
        let routes = self.routes.iter().map(|r| {
            (
                format!("upstream[{}].route[{}].identity", self.upstream, r.path),
                &r.binding,
            )
        });
        upstream.chain(routes).collect()
    }
}

/// One `[[upstream.route]]`, resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteBinding {
    pub path: String,
    pub binding: BindingSpec,
}

/// A config that cannot be used. The daemon must not start on one.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config does not parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("{at}: {reason}")]
    Invalid { at: String, reason: String },
    #[error("{at}: {source}")]
    Binding {
        at: String,
        #[source]
        source: BindingError,
    },
}

impl ConfigError {
    /// The config location a validation error points at (`plugin.tls`,
    /// `upstream[openai].route[/v1/models].identity`, ...).
    pub fn at(&self) -> Option<&str> {
        match self {
            ConfigError::Invalid { at, .. } | ConfigError::Binding { at, .. } => Some(at),
            ConfigError::Io { .. } | ConfigError::Parse(_) => None,
        }
    }
}

pub(crate) fn invalid(at: impl Into<String>, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        at: at.into(),
        reason: reason.into(),
    }
}

impl DaemonConfig {
    /// Read, parse and validate the config file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// Parse and validate config text.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: DaemonConfig = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// The data directory: `data_dir`, else `$JANUS_DATA_DIR`, else
    /// `$HOME/.janus` (`janus_core::contract::paths::resolve_data_dir`).
    pub fn data_dir(&self) -> Option<PathBuf> {
        paths::resolve_data_dir(self.data_dir.as_deref())
    }

    /// The admin surface's `plugin.mode` for this config.
    pub fn plugin_mode(&self) -> Result<PluginMode, ConfigError> {
        match &self.plugin {
            None => Ok(PluginMode::None),
            Some(p) => Ok(p.transport()?.mode()),
        }
    }

    /// The binding block that applies to `upstream` as a whole: its
    /// `[upstream.identity]` merged over `[identity]`, whichever one exists
    /// when only one does, or `None` when neither exists.
    pub fn upstream_binding(&self, upstream: &UpstreamConfig) -> Option<IdentityBindingConfig> {
        match (&upstream.identity, &self.identity) {
            (Some(own), Some(default)) => Some(own.merged_over(default)),
            (Some(own), None) => Some(own.clone()),
            (None, Some(default)) => Some(default.clone()),
            (None, None) => None,
        }
    }

    /// Resolve every upstream's and route's binding into a [`BindingSpec`].
    pub fn resolve_bindings(&self) -> Result<Vec<UpstreamBindings>, ConfigError> {
        self.upstreams
            .iter()
            .map(|up| {
                let at = format!("upstream[{}]", up.name);
                let binding = self
                    .upstream_binding(up)
                    .map(|b| {
                        b.resolve().map_err(|source| ConfigError::Binding {
                            at: format!("{at}.identity"),
                            source,
                        })
                    })
                    .transpose()?;
                let routes = up
                    .routes
                    .iter()
                    .map(|r| {
                        r.identity
                            .resolve()
                            .map(|binding| RouteBinding {
                                path: r.path.clone(),
                                binding,
                            })
                            .map_err(|source| ConfigError::Binding {
                                at: format!("{at}.route[{}].identity", r.path),
                                source,
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(UpstreamBindings {
                    upstream: up.name,
                    binding,
                    routes,
                })
            })
            .collect()
    }

    /// Every rule decidable from the file alone. See the module docs for
    /// what is checked later and where.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_listeners()?;
        if let Some(plugin) = &self.plugin {
            plugin.transport()?;
        }
        self.validate_secrets()?;
        self.validate_upstreams()?;
        self.validate_credentials()?;
        Ok(())
    }

    fn validate_listeners(&self) -> Result<(), ConfigError> {
        if self.data_plane.max_connections == 0 {
            return Err(invalid(
                "data_plane.max_connections",
                "must be above zero (the data plane would refuse every connection)",
            ));
        }
        let admin = &self.admin;
        if admin.bind.is_none() {
            let stray = [
                ("admin.tls", admin.tls.is_some()),
                ("admin.allow_non_loopback", admin.allow_non_loopback),
                ("admin.allow_plaintext", admin.allow_plaintext),
            ];
            if let Some((at, _)) = stray.iter().find(|(_, set)| *set) {
                return Err(invalid(
                    *at,
                    "applies only to the TCP admin listener, and admin.bind is not set",
                ));
            }
        }
        if admin.tls.is_some() && admin.allow_plaintext {
            return Err(invalid(
                "admin.allow_plaintext",
                "admin.tls and admin.allow_plaintext are mutually exclusive",
            ));
        }
        if let Some(ui) = &self.ui {
            if ui.tls.is_some() && ui.allow_plaintext {
                return Err(invalid(
                    "ui.allow_plaintext",
                    "ui.tls and ui.allow_plaintext are mutually exclusive",
                ));
            }
        }
        if let Some(bind) = self.admin.bind {
            if !bind.ip().is_loopback() {
                if !self.admin.allow_non_loopback {
                    return Err(invalid(
                        "admin.bind",
                        format!("{bind} is not a loopback address; set admin.allow_non_loopback = true to allow it"),
                    ));
                }
                if self.admin.tls.is_none() && !self.admin.allow_plaintext {
                    return Err(invalid(
                        "admin.tls",
                        format!("non-loopback admin bind {bind} requires admin.tls = {{ cert, key }} unless admin.allow_plaintext = true"),
                    ));
                }
            }
        }
        if let Some(ui) = &self.ui {
            if !ui.bind.ip().is_loopback() && ui.tls.is_none() && !ui.allow_plaintext {
                return Err(invalid(
                    "ui.tls",
                    format!("non-loopback UI bind {} requires ui.tls = {{ cert, key }} unless ui.allow_plaintext = true", ui.bind),
                ));
            }
        }
        // The three TCP listeners are distinct (port 0 asks the OS for a
        // fresh port each time, so it never collides). An unspecified
        // address (`0.0.0.0`, `[::]`) takes the port on every interface, so
        // it collides with any address on the same port.
        let mut seen: Vec<(&str, SocketAddr)> = vec![("data_plane.bind", self.data_plane.bind)];
        if let Some(bind) = self.admin.bind {
            seen.push(("admin.bind", bind));
        }
        if let Some(ui) = &self.ui {
            seen.push(("ui.bind", ui.bind));
        }
        for (i, (name, addr)) in seen.iter().enumerate() {
            if addr.port() == 0 {
                continue;
            }
            let collides = |a: &SocketAddr| {
                a.port() == addr.port()
                    && (a.ip() == addr.ip()
                        || a.ip().is_unspecified()
                        || addr.ip().is_unspecified())
            };
            if let Some((other, _)) = seen[..i].iter().find(|(_, a)| collides(a)) {
                return Err(invalid(
                    *name,
                    format!(
                        "{addr} is already used by {other}; each listener needs its own address"
                    ),
                ));
            }
        }
        Ok(())
    }

    fn validate_secrets(&self) -> Result<(), ConfigError> {
        for (name, source) in &self.secrets {
            let at = format!("secrets.{name}");
            if name.is_empty() {
                return Err(invalid("secrets", "secret names must not be empty"));
            }
            match source {
                MaterialSource::Env(var) if var.is_empty() => {
                    return Err(invalid(at, "`env` names no variable"))
                }
                MaterialSource::File(path) if path.as_os_str().is_empty() => {
                    return Err(invalid(at, "`file` names no path"))
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn validate_upstreams(&self) -> Result<(), ConfigError> {
        let mut names = BTreeSet::new();
        let mut prefixes = BTreeSet::new();
        for up in &self.upstreams {
            let at = format!("upstream[{}]", up.name);
            if !names.insert(up.name) {
                return Err(invalid(at, "upstream configured more than once"));
            }
            let p = up.path_prefix.as_str();
            if !p.starts_with('/') || p.ends_with('/') {
                return Err(invalid(
                    format!("{at}.path_prefix"),
                    format!("{p:?} must start with '/', not end with '/', and not be '/' alone"),
                ));
            }
            if let Some(other) = prefixes.iter().find(|q: &&&str| overlaps(p, q)) {
                return Err(invalid(
                    format!("{at}.path_prefix"),
                    format!("{p:?} overlaps another upstream's prefix {other:?}; a request must match exactly one upstream"),
                ));
            }
            prefixes.insert(p);
            let has_host = up.base_url.host_str().is_some_and(|h| !h.is_empty());
            if !matches!(up.base_url.scheme(), "http" | "https")
                || !has_host
                || up.base_url.query().is_some()
                || up.base_url.fragment().is_some()
            {
                return Err(invalid(
                    format!("{at}.base_url"),
                    format!(
                        "{} must be an http(s) URL with a host and no query or fragment",
                        up.base_url
                    ),
                ));
            }
            if let Some(token_url) = &up.token_url {
                let at = format!("{at}.token_url");
                if up.name == UpstreamKind::OpenRouter {
                    return Err(invalid(
                        at,
                        "openrouter has no OAuth refresh; remove `token_url`",
                    ));
                }
                if let Some(reason) = token_url_refusal(token_url) {
                    return Err(invalid(at, format!("{token_url}: {reason}")));
                }
            }
            let mut paths = BTreeSet::new();
            for route in &up.routes {
                if !route.path.starts_with('/') {
                    return Err(invalid(
                        format!("{at}.route[{}].path", route.path),
                        "route paths start with '/'",
                    ));
                }
                if let Some(c) = route
                    .path
                    .chars()
                    .find(|c| matches!(c, '*' | '?' | '#' | '{' | '}'))
                {
                    return Err(invalid(
                        format!("{at}.route[{}].path", route.path),
                        format!("route paths are literal; {c:?} has no defined meaning"),
                    ));
                }
                if !paths.insert(route.path.as_str()) {
                    return Err(invalid(
                        format!("{at}.route[{}]", route.path),
                        "route declared more than once",
                    ));
                }
            }
        }
        for up in self.resolve_bindings()? {
            for (at, spec) in up.located() {
                if let BindingSpec::Hmac(h) = spec {
                    if !self.secrets.contains_key(&h.secret_ref) {
                        return Err(invalid(
                            format!("{at}.secret_ref"),
                            format!("secret {:?} is not declared under [secrets]", h.secret_ref),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_credentials(&self) -> Result<(), ConfigError> {
        let mut refs = BTreeSet::new();
        for cred in &self.credentials {
            let r = cred.credential_ref.as_str();
            let at = format!("credential[{r}]");
            validate_credential_ref(r).map_err(|e| invalid(format!("{at}.ref"), e.to_string()))?;
            if !refs.insert(r) {
                return Err(invalid(at, "credential ref declared more than once"));
            }
            match (&cred.env, &cred.file) {
                (Some(var), None) if !var.is_empty() => {}
                (None, Some(path)) if !path.as_os_str().is_empty() => {}
                _ => return Err(invalid(at, "set exactly one non-empty `env` or `file`")),
            }
        }
        Ok(())
    }
}

/// Whether two path prefixes could both match one request: equal, or one is
/// a whole-segment prefix of the other (`/openai` and `/openai/v2`, but not
/// `/openai` and `/openai2`).
fn overlaps(a: &str, b: &str) -> bool {
    let nested = |short: &str, long: &str| {
        long.strip_prefix(short)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    };
    nested(a, b) || nested(b, a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use janus_core::contract::binding::{Correlation, HmacSpec, Source};

    /// Sections the documented fragments leave out; prepended so each
    /// documented example is parsed verbatim.
    const BASE: &str = r#"
[data_plane]
bind = "127.0.0.1:7600"

[secrets.caller-identity]
file = "/etc/janus/caller-identity.key"
"#;

    /// docs/caller-identity#binding, verbatim.
    const DOC_BINDING_EXAMPLE: &str = r#"
# Daemon default: one identity scheme for every upstream that doesn't
# override it.
[identity]
method          = "hmac"
source          = { header = "authorization", scheme = "Bearer " }
secret_ref      = "caller-identity"
namespace       = "janus-data-plane"
identity_claim  = "subject"
correlation_claim = "session"    # optional; omit for no correlation value

[[upstream]]
name       = "anthropic"
path_prefix = "/anthropic"
base_url   = "https://api.anthropic.com"
# No [upstream.identity] — inherits [identity] wholesale.

[[upstream]]
name       = "openai"
path_prefix = "/openai"
base_url   = "https://api.openai.com"

  # This upstream's callers are SDKs that only send an API key, which lands
  # in a different header. Same secret, same namespace, different source.
  [upstream.identity]
  source = { header = "x-api-key" }

  # Narrower than the upstream: a route that takes no identity at all.
  [[upstream.route]]
  path     = "/v1/models"
  identity = { method = "none" }
"#;

    /// docs/plugin-contract#plugin-config, verbatim (first example).
    const DOC_UNIX_PLUGIN_EXAMPLE: &str = r#"
# A plugin on a local socket.
[plugin]
endpoint   = "unix:///run/janus/policy.sock"
timeout_ms = 250                               # ResolveRoute deadline; default
"#;

    /// docs/plugin-contract#plugin-config, verbatim (second example).
    const DOC_HTTPS_PLUGIN_EXAMPLE: &str = r#"
# A plugin across the network, over mutual TLS. See "Securing the channel".
[plugin]
endpoint = "https://policy.internal:7443"

  [plugin.tls]
  ca_file     = "/etc/janus/plugin-ca.pem"     # exactly one of ca_file / pinned_cert
  client_cert = "/etc/janus/daemon.pem"
  client_key  = "/etc/janus/daemon-key.pem"    # must be mode 0600
  # server_name  = "policy.internal"           # SNI / hostname override
  # expected_san = "spiffe://example.org/policy"  # replaces the hostname check
"#;

    /// docs/plugin-contract#plugin-config, verbatim (third example).
    const DOC_LINKED_EXAMPLE: &str = r#"
# A custom build with a linked plugin says so, and names no endpoint.
[plugin]
linked = true
"#;

    fn with_base(fragment: &str) -> Result<DaemonConfig, ConfigError> {
        DaemonConfig::from_toml_str(&format!("{BASE}\n{fragment}"))
    }

    fn err_at(fragment: &str) -> String {
        let err = with_base(fragment).expect_err("config must be refused");
        err.at().unwrap_or("<parse>").to_owned()
    }

    #[test]
    fn documented_binding_example_parses_and_resolves_per_docs() {
        let cfg = with_base(DOC_BINDING_EXAMPLE).expect("documented example is valid");
        assert_eq!(cfg.upstreams.len(), 2);
        assert_eq!(cfg.upstreams[0].name, UpstreamKind::Anthropic);
        assert_eq!(
            cfg.upstreams[0].base_url.host_str(),
            Some("api.anthropic.com")
        );
        let bindings = cfg.resolve_bindings().expect("resolves");

        let default_hmac = HmacSpec {
            source: Source::Header {
                name: http::HeaderName::from_static("authorization"),
                scheme: Some("Bearer ".into()),
            },
            secret_ref: "caller-identity".into(),
            namespace: "janus-data-plane".into(),
            identity_claim: "subject".into(),
            correlation: Some(Correlation::Claim("session".into())),
        };
        // anthropic inherits [identity] wholesale.
        assert_eq!(
            bindings[0].binding,
            Some(BindingSpec::Hmac(default_hmac.clone()))
        );
        assert!(bindings[0].routes.is_empty());
        // openai overrides only the source.
        let openai = HmacSpec {
            source: Source::Header {
                name: http::HeaderName::from_static("x-api-key"),
                scheme: None,
            },
            ..default_hmac
        };
        assert_eq!(bindings[1].binding, Some(BindingSpec::Hmac(openai)));
        // the route replaces the binding outright with none.
        assert_eq!(
            bindings[1].routes,
            vec![RouteBinding {
                path: "/v1/models".into(),
                binding: BindingSpec::None
            }]
        );
        // Defaults for everything the example leaves out.
        assert!(cfg.request_log.enabled);
        assert_eq!(cfg.request_log.retain, DEFAULT_REQUEST_LOG_RETAIN);
        assert!(cfg.plugin.is_none());
        assert_eq!(cfg.plugin_mode().expect("mode"), PluginMode::None);
        assert!(cfg.ui.is_none());
        assert_eq!(cfg.admin, AdminConfig::default());
    }

    #[test]
    fn documented_plugin_examples_parse_to_their_transports() {
        let unix = with_base(DOC_UNIX_PLUGIN_EXAMPLE).expect("documented unix example");
        let plugin = unix.plugin.as_ref().expect("plugin block");
        assert_eq!(plugin.timeout(), Duration::from_millis(250));
        assert_eq!(
            plugin.transport().expect("transport"),
            PluginTransport::Unix {
                path: "/run/janus/policy.sock".into()
            }
        );

        let https = with_base(DOC_HTTPS_PLUGIN_EXAMPLE).expect("documented https example");
        match https
            .plugin
            .as_ref()
            .expect("plugin")
            .transport()
            .expect("tls")
        {
            PluginTransport::Tls {
                endpoint,
                trust,
                client_key,
                ..
            } => {
                assert_eq!(endpoint.host_str(), Some("policy.internal"));
                assert_eq!(endpoint.port(), Some(7443));
                assert_eq!(
                    trust,
                    TrustAnchor::CaFile("/etc/janus/plugin-ca.pem".into())
                );
                assert_eq!(client_key, PathBuf::from("/etc/janus/daemon-key.pem"));
            }
            other => panic!("expected TLS transport, got {other:?}"),
        }
        assert_eq!(https.plugin_mode().expect("mode"), PluginMode::Tls);

        let linked = with_base(DOC_LINKED_EXAMPLE).expect("documented linked example");
        let plugin = linked.plugin.as_ref().expect("plugin block");
        assert_eq!(plugin.transport().expect("linked"), PluginTransport::Linked);
        assert_eq!(plugin.timeout_ms, DEFAULT_PLUGIN_TIMEOUT_MS);

        let plaintext =
            with_base("[plugin]\nendpoint = \"http://policy.mesh:8080\"\nallow_plaintext = true\n")
                .expect("explicit plaintext opt-in");
        assert_eq!(
            plaintext.plugin_mode().expect("mode"),
            PluginMode::Plaintext
        );
    }

    #[test]
    fn plugin_blocks_the_docs_refuse_are_refused() {
        let tls = "[plugin.tls]\nclient_cert = \"/c.pem\"\nclient_key = \"/k.pem\"\n";
        let cases = [
            // Both endpoint and linked.
            ("[plugin]\nendpoint = \"unix:///run/p.sock\"\nlinked = true\n".to_owned(), "plugin"),
            // Neither, inside a present block.
            ("[plugin]\ntimeout_ms = 100\n".to_owned(), "plugin"),
            // http:// without allow_plaintext.
            ("[plugin]\nendpoint = \"http://policy:8080\"\n".to_owned(), "plugin.endpoint"),
            // https:// without [plugin.tls].
            ("[plugin]\nendpoint = \"https://policy:7443\"\n".to_owned(), "plugin.tls"),
            // ca_file and pinned_cert together.
            (
                format!("[plugin]\nendpoint = \"https://policy:7443\"\n{tls}ca_file = \"/ca.pem\"\npinned_cert = \"/p.pem\"\n"),
                "plugin.tls",
            ),
            // Neither ca_file nor pinned_cert: no fallback to system roots.
            (format!("[plugin]\nendpoint = \"https://policy:7443\"\n{tls}"), "plugin.tls"),
            // No other scheme is accepted.
            ("[plugin]\nendpoint = \"grpc://policy:7443\"\n".to_owned(), "plugin.endpoint"),
            ("[plugin]\nendpoint = \"unix://host/run/p.sock\"\n".to_owned(), "plugin.endpoint"),
            ("[plugin]\nendpoint = \"unix:///\"\n".to_owned(), "plugin.endpoint"),
        ];
        for (fragment, at) in cases {
            assert_eq!(err_at(&fragment), at, "for:\n{fragment}");
        }
        // Plugin settings come from the file and nowhere else; unknown keys
        // are typos, not silently ignored.
        assert_eq!(err_at("[plugin]\nlinked = true\ntimeout = 5\n"), "<parse>");
    }

    #[test]
    fn plugin_tls_beside_a_unix_endpoint_is_a_startup_refusal() {
        // The pre-split doc example: a unix:// endpoint with [plugin.tls].
        let combined = DOC_HTTPS_PLUGIN_EXAMPLE.replace(
            "\"https://policy.internal:7443\"",
            "\"unix:///run/janus/policy.sock\"",
        );
        assert!(combined.contains("unix:///run/janus/policy.sock"));
        let err = with_base(&combined).expect_err("[plugin.tls] beside unix:// is refused");
        assert_eq!(err.at(), Some("plugin.tls"));
        let message = err.to_string();
        assert!(message.contains("plugin.tls"), "{message}");
        assert!(message.contains("https:// endpoint only"), "{message}");
    }

    #[test]
    fn plugin_settings_without_effect_are_refused() {
        let tls = "[plugin.tls]\nca_file = \"/ca.pem\"\nclient_cert = \"/c.pem\"\nclient_key = \"/k.pem\"\n";
        let cases = [
            (
                "[plugin]\nlinked = true\ntimeout_ms = 0\n".to_owned(),
                "plugin.timeout_ms",
            ),
            (
                "[plugin]\nendpoint = \"unix:///run/p.sock\"\nallow_plaintext = true\n".to_owned(),
                "plugin.allow_plaintext",
            ),
            (
                format!(
                    "[plugin]\nendpoint = \"https://policy:7443\"\nallow_plaintext = true\n{tls}"
                ),
                "plugin.allow_plaintext",
            ),
            (
                "[plugin]\nlinked = true\nallow_plaintext = true\n".to_owned(),
                "plugin.allow_plaintext",
            ),
            (format!("[plugin]\nlinked = true\n{tls}"), "plugin.tls"),
            (
                format!("[plugin]\nendpoint = \"unix:///run/p.sock\"\n{tls}"),
                "plugin.tls",
            ),
            (
                format!(
                    "[plugin]\nendpoint = \"http://policy:8080\"\nallow_plaintext = true\n{tls}"
                ),
                "plugin.tls",
            ),
        ];
        for (fragment, at) in cases {
            assert_eq!(err_at(&fragment), at, "for:\n{fragment}");
        }
        with_base("[plugin]\nlinked = true\ntimeout_ms = 1\n").expect("1ms is a deadline");
    }

    #[test]
    fn listener_rules_the_docs_state_are_enforced() {
        assert_eq!(err_at("[admin]\nbind = \"0.0.0.0:7601\"\n"), "admin.bind");
        assert_eq!(
            err_at("[admin]\nbind = \"0.0.0.0:7601\"\nallow_non_loopback = true\n"),
            "admin.tls"
        );
        with_base(
            "[admin]\nbind = \"0.0.0.0:7601\"\nallow_non_loopback = true\ntls = { cert = \"/a.pem\", key = \"/a-key.pem\" }\n",
        )
        .expect("non-loopback admin with TLS");
        with_base(
            "[admin]\nbind = \"0.0.0.0:7601\"\nallow_non_loopback = true\nallow_plaintext = true\n",
        )
        .expect("non-loopback admin with explicit plaintext");
        with_base("[admin]\nbind = \"127.0.0.1:7601\"\n").expect("loopback admin needs nothing");
        assert_eq!(err_at("[ui]\nbind = \"10.0.0.5:7602\"\n"), "ui.tls");
        with_base("[ui]\nbind = \"[::1]:7602\"\n").expect("loopback UI");
        assert_eq!(err_at("[ui]\nbind = \"127.0.0.1:7600\"\n"), "ui.bind");
        // An unspecified address holds the port on every interface.
        assert_eq!(
            err_at("[ui]\nbind = \"0.0.0.0:7600\"\nallow_plaintext = true\n"),
            "ui.bind"
        );
        with_base("[ui]\nbind = \"127.0.0.1:7602\"\n").expect("distinct port");
        // Admin listener settings without an admin listener.
        assert_eq!(
            err_at("[admin]\nallow_non_loopback = true\n"),
            "admin.allow_non_loopback"
        );
        assert_eq!(
            err_at("[admin]\nallow_plaintext = true\n"),
            "admin.allow_plaintext"
        );
        assert_eq!(
            err_at("[admin]\ntls = { cert = \"/a.pem\", key = \"/k.pem\" }\n"),
            "admin.tls"
        );
        // TLS and an explicit plaintext opt-in contradict each other.
        assert_eq!(
            err_at("[admin]\nbind = \"127.0.0.1:7601\"\ntls = { cert = \"/a.pem\", key = \"/k.pem\" }\nallow_plaintext = true\n"),
            "admin.allow_plaintext"
        );
        assert_eq!(
            err_at("[ui]\nbind = \"127.0.0.1:7602\"\ntls = { cert = \"/a.pem\", key = \"/k.pem\" }\nallow_plaintext = true\n"),
            "ui.allow_plaintext"
        );
    }

    #[test]
    fn upstream_prefixes_and_route_paths_must_be_unambiguous() {
        let up = |name: &str, prefix: &str| {
            format!("[[upstream]]\nname = \"{name}\"\npath_prefix = \"{prefix}\"\nbase_url = \"https://example.com\"\n")
        };
        assert_eq!(err_at(&up("openai", "/")), "upstream[openai].path_prefix");
        assert_eq!(
            err_at(&format!(
                "{}{}",
                up("openai", "/openai"),
                up("openrouter", "/openai/v2")
            )),
            "upstream[openrouter].path_prefix"
        );
        assert_eq!(
            err_at(&format!(
                "{}{}",
                up("openai", "/a/b"),
                up("openrouter", "/a")
            )),
            "upstream[openrouter].path_prefix"
        );
        assert_eq!(
            err_at(&format!(
                "{}{}",
                up("openai", "/same"),
                up("openrouter", "/same")
            )),
            "upstream[openrouter].path_prefix"
        );
        // A shared string prefix that is not a shared segment is fine.
        with_base(&format!(
            "{}{}",
            up("openai", "/openai"),
            up("openrouter", "/openai2")
        ))
        .expect("segment-distinct prefixes");
        assert_eq!(
            err_at(&format!(
                "{}[[upstream.route]]\npath = \"/v1/*\"\nidentity = {{ method = \"none\" }}\n",
                up("openai", "/openai")
            )),
            "upstream[openai].route[/v1/*].path"
        );
    }

    #[test]
    fn token_url_is_validated() {
        let up = |name: &str, line: &str| {
            format!("[[upstream]]\nname = \"{name}\"\npath_prefix = \"/u\"\nbase_url = \"https://example.com\"\n{line}\n")
        };
        for ok in [
            "token_url = \"https://auth.example.com/oauth/token\"",
            "token_url = \"http://127.0.0.1:9000/token\"",
            "token_url = \"http://[::1]:9000/token\"",
        ] {
            let cfg = with_base(&up("anthropic", ok)).expect(ok);
            assert!(cfg.upstreams[0].token_url.is_some());
        }
        assert!(with_base(&up("openai", "")).expect("default").upstreams[0]
            .token_url
            .is_none());
        for bad in [
            "token_url = \"http://auth.example.com/token\"",
            "token_url = \"http://localhost:9000/token\"",
            "token_url = \"ftp://127.0.0.1/token\"",
        ] {
            assert_eq!(err_at(&up("openai", bad)), "upstream[openai].token_url");
        }
        // Not absolute: refused at parse.
        assert_eq!(
            err_at(&up("anthropic", "token_url = \"/oauth/token\"")),
            "<parse>"
        );
        // No OAuth refresh on openrouter.
        assert_eq!(
            err_at(&up(
                "openrouter",
                "token_url = \"https://auth.example.com/t\""
            )),
            "upstream[openrouter].token_url"
        );
    }

    #[test]
    fn upstream_binding_and_credential_errors_are_refused() {
        let up = |extra: &str| {
            format!("[[upstream]]\nname = \"openrouter\"\npath_prefix = \"/openrouter\"\nbase_url = \"https://openrouter.ai/api\"\n{extra}")
        };
        // Unknown upstream implementation.
        assert_eq!(
            err_at(
                "[[upstream]]\nname = \"gemini\"\npath_prefix = \"/g\"\nbase_url = \"https://g\"\n"
            ),
            "<parse>"
        );
        // A binding without a method is not an implicit unauthenticated tier.
        assert_eq!(
            err_at(&up(
                "[upstream.identity]\nsource = { header = \"authorization\" }\n"
            )),
            "upstream[openrouter].identity"
        );
        // A route binding is resolved on its own, not merged.
        assert_eq!(
            err_at(&up("[[upstream.route]]\npath = \"/v1/x\"\nidentity = { source = { header = \"a\" } }\n")),
            "upstream[openrouter].route[/v1/x].identity"
        );
        // An hmac binding naming an undeclared secret.
        assert_eq!(
            err_at(&up("[upstream.identity]\nmethod = \"hmac\"\nsource = { header = \"authorization\" }\nsecret_ref = \"nope\"\nnamespace = \"n\"\nidentity_claim = \"sub\"\n")),
            "upstream[openrouter].identity.secret_ref"
        );
        // Duplicate upstream.
        assert_eq!(
            err_at(&format!("{}\n{}", up(""), up(""))),
            "upstream[openrouter]"
        );
        // Bad prefix.
        assert_eq!(
            err_at("[[upstream]]\nname = \"openai\"\npath_prefix = \"openai/\"\nbase_url = \"https://api.openai.com\"\n"),
            "upstream[openai].path_prefix"
        );
        // Credentials: exactly one of env/file; ref reserved and unique.
        let cred = |body: &str| format!("[[credential]]\nupstream = \"openrouter\"\n{body}");
        assert_eq!(
            err_at(&cred("ref = \"k\"\nenv = \"A\"\nfile = \"/k\"\n")),
            "credential[k]"
        );
        assert_eq!(err_at(&cred("ref = \"k\"\n")), "credential[k]");
        assert_eq!(
            err_at(&cred("ref = \"default\"\nenv = \"A\"\n")),
            "credential[default].ref"
        );
        assert_eq!(
            err_at(&cred("ref = \"../x\"\nenv = \"A\"\n")),
            "credential[../x].ref"
        );
        let ok = with_base(&cred("ref = \"main\"\nenv = \"OPENROUTER_API_KEY\"\n")).expect("ok");
        assert_eq!(
            ok.credentials[0].source(),
            Some(MaterialSource::Env("OPENROUTER_API_KEY".into()))
        );
    }

    #[test]
    fn data_plane_max_connections_defaults_parses_and_must_be_above_zero() {
        let default = with_base("").expect("valid");
        assert_eq!(
            default.data_plane.max_connections,
            DEFAULT_DATA_PLANE_MAX_CONNECTIONS
        );
        let text = BASE.replacen("[data_plane]\n", "[data_plane]\nmax_connections = 7\n", 1);
        assert!(
            text.contains("max_connections = 7"),
            "BASE has [data_plane]"
        );
        let cfg = DaemonConfig::from_toml_str(&text).expect("valid");
        assert_eq!(cfg.data_plane.max_connections, 7);
        let zero = BASE.replacen("[data_plane]\n", "[data_plane]\nmax_connections = 0\n", 1);
        let err = DaemonConfig::from_toml_str(&zero).expect_err("zero is refused");
        assert_eq!(err.at(), Some("data_plane.max_connections"));
    }

    #[test]
    fn request_log_and_data_dir_settings_parse() {
        let cfg = DaemonConfig::from_toml_str(&format!(
            "data_dir = \"/var/lib/janus\"\n{BASE}\n[request_log]\nenabled = false\nretain = \"7d\"\n"
        ))
        .expect("parses");
        assert!(!cfg.request_log.enabled);
        assert_eq!(cfg.request_log.retain, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(cfg.data_dir(), Some(PathBuf::from("/var/lib/janus")));
        assert!(with_base("[request_log]\nretain = \"forever\"\n").is_err());
    }
}
