//! The admin API client: one HTTP+JSON surface over two transports.
//!
//! - `unix://<path>` — the daemon's admin socket. Filesystem permissions are
//!   the authentication, so no token is sent.
//! - `http(s)://host:port` — the TCP admin (or UI) listener. Every request
//!   carries `Authorization: Bearer <admin token>`. For `https` the server
//!   is verified against the system roots, or — with `--ca-file` — against
//!   that file alone (a private CA, or the listener's own self-signed
//!   certificate pinned).
//!
//! Responses that are not 2xx are decoded from the daemon's error envelope
//! (`{"error":{"code","message"}}`) into [`CliError::Api`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http::{header, Method};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::net::UnixStream;
use url::Url;

use crate::error::{chain, CliError};

/// How long a connection attempt may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a whole non-streaming call may take. `POST /v1/plugin/check`
/// runs several bounded probes, and a credential refresh calls a token
/// endpoint with its own 30-second limit; both fit.
const CALL_TIMEOUT: Duration = Duration::from_secs(90);

/// Where the admin surface is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Unix(PathBuf),
    Http(Url),
}

impl Address {
    /// `unix://<path>` or `http(s)://host:port[/prefix]`.
    pub fn parse(text: &str) -> Result<Address, CliError> {
        if let Some(path) = text.strip_prefix("unix://") {
            if path.is_empty() {
                return Err(CliError::usage(
                    "admin address `unix://` names no socket path (use unix:///path/to/admin.sock)",
                ));
            }
            return Ok(Address::Unix(PathBuf::from(path)));
        }
        let url = Url::parse(text).map_err(|e| {
            CliError::usage(format!(
                "admin address {text:?} is not unix://<path> or http(s)://host:port: {e}"
            ))
        })?;
        match url.scheme() {
            "http" | "https" if url.host().is_some() => {
                if url.query().is_some() || url.fragment().is_some() {
                    return Err(CliError::usage(format!(
                        "admin address {text:?} must not carry a query or fragment"
                    )));
                }
                Ok(Address::Http(url))
            }
            _ => Err(CliError::usage(format!(
                "admin address {text:?} is not unix://<path> or http(s)://host:port"
            ))),
        }
    }

    /// The default: `<data_dir>/admin.sock`.
    pub fn default_socket(data_dir: &Path) -> Address {
        Address::Unix(janus_core::contract::paths::admin_socket_path(data_dir))
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Address::Unix(path) => write!(f, "unix://{}", path.display()),
            Address::Http(url) => f.write_str(url.as_str()),
        }
    }
}

enum Transport {
    Unix(PathBuf),
    Http {
        base: String,
        client: reqwest::Client,
        token: Option<String>,
    },
}

/// A connected-on-demand admin API client.
pub struct Admin {
    address: Address,
    transport: Transport,
}

/// A response body, read incrementally (the live tail) or whole.
pub enum Body {
    Unix(Incoming),
    Http(reqwest::Response),
}

impl Body {
    /// The next chunk; `None` at the end of the body.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, String> {
        match self {
            Body::Unix(body) => loop {
                match body.frame().await {
                    None => return Ok(None),
                    Some(Err(e)) => return Err(chain(&e)),
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            return Ok(Some(data));
                        }
                    }
                }
            },
            Body::Http(response) => response.chunk().await.map_err(|e| chain(&e)),
        }
    }

    async fn collect(mut self) -> Result<Vec<u8>, String> {
        let mut all = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            all.extend_from_slice(&chunk);
        }
        Ok(all)
    }
}

/// TLS and token settings for an `http(s)` address.
pub struct HttpOptions {
    /// `--ca-file`: the only trust anchor for an `https` admin address.
    pub ca_file: Option<PathBuf>,
    /// The admin token, already read from wherever it came from.
    pub token: Option<String>,
}

impl Admin {
    pub fn new(address: Address, options: HttpOptions) -> Result<Admin, CliError> {
        let transport = match &address {
            Address::Unix(path) => Transport::Unix(path.clone()),
            Address::Http(url) => {
                let mut builder = reqwest::Client::builder()
                    .connect_timeout(CONNECT_TIMEOUT)
                    .https_only(url.scheme() == "https");
                if let Some(ca) = &options.ca_file {
                    if url.scheme() != "https" {
                        return Err(CliError::usage(
                            "--ca-file applies only to an https:// admin address",
                        ));
                    }
                    let pem = std::fs::read(ca).map_err(|e| {
                        CliError::usage(format!("cannot read --ca-file {}: {e}", ca.display()))
                    })?;
                    let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| {
                        CliError::usage(format!(
                            "--ca-file {} is not a PEM certificate: {}",
                            ca.display(),
                            chain(&e)
                        ))
                    })?;
                    if certs.is_empty() {
                        return Err(CliError::usage(format!(
                            "--ca-file {} holds no certificate",
                            ca.display()
                        )));
                    }
                    builder = builder.tls_built_in_root_certs(false);
                    for cert in certs {
                        builder = builder.add_root_certificate(cert);
                    }
                }
                let client = builder.build().map_err(|e| {
                    CliError::usage(format!("cannot build the HTTP client: {}", chain(&e)))
                })?;
                Transport::Http {
                    base: url.as_str().trim_end_matches('/').to_owned(),
                    client,
                    token: options.token,
                }
            }
        };
        Ok(Admin { address, transport })
    }

    /// Send one request; the body is not read yet.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Body), CliError> {
        let payload = body.map(|v| Bytes::from(v.to_string()));
        match &self.transport {
            Transport::Unix(socket) => {
                let stream = UnixStream::connect(socket)
                    .await
                    .map_err(|e| self.unreachable(&e.to_string()))?;
                let (mut sender, connection) =
                    hyper::client::conn::http1::handshake(TokioIo::new(stream))
                        .await
                        .map_err(|e| self.unreachable(&chain(&e)))?;
                tokio::spawn(async move {
                    // The connection's own error surfaces on the request.
                    let _ = connection.await;
                });
                let mut request = http::Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json, text/event-stream");
                if payload.is_some() {
                    request = request.header(header::CONTENT_TYPE, "application/json");
                }
                let request = request
                    .body(Full::new(payload.unwrap_or_default()))
                    .map_err(|e| CliError::usage(format!("invalid request {path}: {e}")))?;
                let response = sender
                    .send_request(request)
                    .await
                    .map_err(|e| self.unreachable(&chain(&e)))?;
                Ok((response.status().as_u16(), Body::Unix(response.into_body())))
            }
            Transport::Http {
                base,
                client,
                token,
            } => {
                let mut request = client.request(method, format!("{base}{path}"));
                if let Some(token) = token {
                    request = request.bearer_auth(token);
                }
                if let Some(payload) = payload {
                    request = request
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(payload);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|e| self.unreachable(&chain(&e)))?;
                Ok((response.status().as_u16(), Body::Http(response)))
            }
        }
    }

    fn unreachable(&self, detail: &str) -> CliError {
        let hint = match &self.address {
            Address::Unix(_) => {
                "is janus-daemon running over this data directory? Point the CLI elsewhere with \
                 --admin, JANUS_ADMIN, --config or JANUS_DATA_DIR"
            }
            Address::Http(_) => "is the daemon's admin listener (admin.bind) up at this address?",
        };
        CliError::Unreachable(format!(
            "cannot reach the admin surface at {}: {detail} ({hint})",
            self.address
        ))
    }

    /// One call, read whole. `Ok(None)` for `204 No Content`.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Option<Value>, CliError> {
        let call = async {
            let (status, body) = self.send(method, path, body).await?;
            let bytes = body.collect().await.map_err(|e| self.unreachable(&e))?;
            Ok::<_, CliError>((status, bytes))
        };
        let (status, bytes) = tokio::time::timeout(CALL_TIMEOUT, call)
            .await
            .map_err(|_| {
                self.unreachable(&format!(
                    "no answer within {}",
                    humantime::format_duration(CALL_TIMEOUT)
                ))
            })??;
        if !(200..300).contains(&status) {
            return Err(self.api_error(status, &bytes));
        }
        if status == 204 || bytes.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&bytes).map(Some).map_err(|e| {
            CliError::failed(
                "bad_response",
                format!("the daemon answered {path} with a body that is not JSON: {e}"),
            )
        })
    }

    /// A call whose 2xx answer must be a JSON document of type `T`.
    pub async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(Value, T), CliError> {
        let value = self.call(method, path, body).await?.ok_or_else(|| {
            CliError::failed(
                "bad_response",
                format!("the daemon answered {path} with no body"),
            )
        })?;
        let typed = decode(path, &value)?;
        Ok((value, typed))
    }

    /// Open a streaming response (the live tail): a 2xx status and its body.
    pub async fn stream(&self, path: &str) -> Result<Body, CliError> {
        let (status, body) = tokio::time::timeout(CALL_TIMEOUT, self.send(Method::GET, path, None))
            .await
            .map_err(|_| self.unreachable("no answer to the stream request"))??;
        if !(200..300).contains(&status) {
            let bytes = body.collect().await.unwrap_or_default();
            return Err(self.api_error(status, &bytes));
        }
        Ok(body)
    }

    fn api_error(&self, status: u16, bytes: &[u8]) -> CliError {
        let envelope: Option<(String, String)> =
            serde_json::from_slice::<Value>(bytes).ok().and_then(|v| {
                let e = v.get("error")?;
                Some((
                    e.get("code")?.as_str()?.to_owned(),
                    e.get("message")?.as_str()?.to_owned(),
                ))
            });
        let (code, mut message) = envelope.unwrap_or_else(|| {
            (
                format!("http_{status}"),
                format!("the daemon answered HTTP {status} without an error document"),
            )
        });
        if status == 401 {
            message.push_str(
                "; this listener requires the admin token: pass --token-file <path> (or set \
                 JANUS_TOKEN_FILE or JANUS_ADMIN_TOKEN), or run on the daemon's host where \
                 <data_dir>/admin-token is read",
            );
        }
        CliError::Api {
            status,
            code,
            message,
        }
    }
}

/// Decode an admin API document into the type the CLI renders it as.
pub fn decode<T: DeserializeOwned>(what: &str, value: &Value) -> Result<T, CliError> {
    T::deserialize(value).map_err(|e| {
        CliError::failed(
            "bad_response",
            format!("unexpected document from {what}: {e}"),
        )
    })
}

/// An API path from literal and user-supplied segments, each
/// percent-encoded, plus the query parameters that are set.
pub fn api_path(segments: &[&str], query: &[(&str, Option<String>)]) -> Result<String, CliError> {
    let mut url =
        Url::parse("http://admin.invalid/").map_err(|e| CliError::usage(e.to_string()))?;
    for segment in segments {
        // The URL parser would drop `.` and `..` and collapse an empty
        // segment, addressing a different endpoint than the one named.
        if matches!(*segment, "" | "." | "..") {
            return Err(CliError::usage(format!(
                "{segment:?} is not a valid identifier"
            )));
        }
    }
    url.path_segments_mut()
        .map_err(|()| CliError::usage("cannot build the request path"))?
        .pop_if_empty()
        .extend(segments);
    let pairs: Vec<(&str, &str)> = query
        .iter()
        .filter_map(|(k, v)| v.as_deref().map(|v| (*k, v)))
        .collect();
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
    Ok(url[url::Position::BeforePath..].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_forms_parse_and_bad_ones_are_usage_errors() {
        assert_eq!(
            Address::parse("unix:///var/lib/janus/admin.sock").unwrap(),
            Address::Unix(PathBuf::from("/var/lib/janus/admin.sock"))
        );
        assert!(matches!(
            Address::parse("https://127.0.0.1:7071").unwrap(),
            Address::Http(u) if u.port() == Some(7071)
        ));
        for bad in [
            "unix://",
            "ftp://host",
            "localhost:7071",
            "http://h/?x=1",
            "",
        ] {
            let err = Address::parse(bad).expect_err(bad);
            assert_eq!(err.exit_code(), crate::error::EXIT_USAGE, "{bad}");
        }
    }

    #[test]
    fn api_path_encodes_segments_and_skips_unset_query() {
        let p = api_path(
            &["v1", "credentials", "a b/c"],
            &[("upstream", Some("openrouter".into())), ("x", None)],
        )
        .unwrap();
        assert_eq!(p, "/v1/credentials/a%20b%2Fc?upstream=openrouter");
        assert_eq!(api_path(&["v1", "status"], &[]).unwrap(), "/v1/status");
        assert!(api_path(&["v1", "identities", ".."], &[]).is_err());
        assert!(api_path(&["v1", "identities", ""], &[]).is_err());
    }
}
