//! The three admin calls the keeper makes (`docs/admin-surface`), over the
//! daemon's unix socket or a loopback `http://` address with a bearer token.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http::{header, Method};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use janus_core::contract::credential::CredentialMetadata;
use serde_json::Value;
use tokio::net::UnixStream;

use crate::login::Material;

/// One admin call's bound.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub enum Address {
    Unix(PathBuf),
    Http(String),
}

impl Address {
    /// `unix:<path>` or `http(s)://...`.
    pub fn parse(text: &str) -> Result<Address, String> {
        if let Some(path) = text.strip_prefix("unix:") {
            let path = path.strip_prefix("//").unwrap_or(path);
            return Ok(Address::Unix(PathBuf::from(path)));
        }
        if text.starts_with("http://") || text.starts_with("https://") {
            return Ok(Address::Http(text.trim_end_matches('/').to_owned()));
        }
        Err(format!(
            "admin address {text:?} is neither unix:<path> nor http(s)://"
        ))
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Address::Unix(p) => write!(f, "unix:{}", p.display()),
            Address::Http(u) => f.write_str(u),
        }
    }
}

/// An admin call that did not succeed.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    /// No answer: the daemon is down or unreachable. Says nothing about
    /// the credential.
    #[error("admin surface unreachable at {address}: {detail}")]
    Unreachable { address: String, detail: String },
    /// The daemon answered with its error envelope.
    #[error("admin API answered {status} {code}: {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },
    #[error("admin API answered with an unexpected body: {0}")]
    BadResponse(String),
}

impl AdminError {
    /// A `502 refresh_failed` in which the token endpoint itself refused
    /// the grant (a 4xx answer, `invalid_grant`): the refresh token is dead
    /// and only a new login helps. A transport failure is not this.
    pub fn is_refresh_refused(&self) -> bool {
        match self {
            AdminError::Api { code, message, .. } if code == "refresh_failed" => {
                message.contains("invalid_grant")
                    || ["answered 400", "answered 401", "answered 403"]
                        .iter()
                        .any(|s| message.contains(s))
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Admin {
    address: Address,
    token: Option<String>,
    http: reqwest::Client,
}

impl Admin {
    pub fn new(address: Address, token: Option<String>) -> Admin {
        Admin {
            address,
            token,
            http: reqwest::Client::new(),
        }
    }

    /// From the config's `admin` / `admin_token_file`, defaulting as the
    /// `janus` CLI does.
    pub fn from_config(admin: Option<&str>, token_file: Option<&Path>) -> Result<Admin, String> {
        let data_dir = janus_core::contract::paths::resolve_data_dir(None);
        let address = match admin {
            Some(text) => Address::parse(text)?,
            None => Address::Unix(janus_core::contract::paths::admin_socket_path(
                data_dir
                    .as_deref()
                    .ok_or("no admin address and no data directory")?,
            )),
        };
        let token = match &address {
            Address::Unix(_) => None,
            Address::Http(_) => {
                let path = match token_file {
                    Some(p) => p.to_path_buf(),
                    None => janus_core::contract::paths::admin_token_path(
                        data_dir
                            .as_deref()
                            .ok_or("no admin_token_file and no data directory")?,
                    ),
                };
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read admin token {}: {e}", path.display()))?;
                Some(text.trim().to_owned())
            }
        };
        Ok(Admin::new(address, token))
    }

    pub fn address(&self) -> &Address {
        &self.address
    }

    /// `GET /v1/credentials/{ref}`.
    pub async fn show(&self, credential_ref: &str) -> Result<CredentialMetadata, AdminError> {
        self.metadata(Method::GET, &credential_path(credential_ref, ""), None)
            .await
    }

    /// `POST /v1/credentials/{ref}/refresh`.
    pub async fn refresh(&self, credential_ref: &str) -> Result<CredentialMetadata, AdminError> {
        self.metadata(
            Method::POST,
            &credential_path(credential_ref, "/refresh"),
            None,
        )
        .await
    }

    /// `PUT /v1/credentials/{ref}/material`: replaces the material and
    /// clears `invalid`.
    pub async fn rotate(
        &self,
        credential_ref: &str,
        material: &Material,
    ) -> Result<CredentialMetadata, AdminError> {
        let body = serde_json::json!({ "material": material.document() });
        self.metadata(
            Method::PUT,
            &credential_path(credential_ref, "/material"),
            Some(body),
        )
        .await
    }

    async fn metadata(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<CredentialMetadata, AdminError> {
        let (status, bytes) = tokio::time::timeout(CALL_TIMEOUT, self.send(method, path, body))
            .await
            .map_err(|_| self.unreachable("no answer within 30s".into()))??;
        if !(200..300).contains(&status) {
            let envelope: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let field = |k: &str| envelope["error"][k].as_str().unwrap_or_default().to_owned();
            return Err(AdminError::Api {
                status,
                code: field("code"),
                message: field("message"),
            });
        }
        serde_json::from_slice(&bytes).map_err(|e| AdminError::BadResponse(e.to_string()))
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Bytes), AdminError> {
        let payload = body.map(|v| Bytes::from(v.to_string()));
        match &self.address {
            Address::Unix(socket) => {
                let stream = UnixStream::connect(socket)
                    .await
                    .map_err(|e| self.unreachable(e.to_string()))?;
                let (mut sender, connection) =
                    hyper::client::conn::http1::handshake(TokioIo::new(stream))
                        .await
                        .map_err(|e| self.unreachable(e.to_string()))?;
                tokio::spawn(async move {
                    // The connection's own error surfaces on the request.
                    let _ = connection.await;
                });
                let mut request = http::Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json");
                if payload.is_some() {
                    request = request.header(header::CONTENT_TYPE, "application/json");
                }
                let request = request
                    .body(Full::new(payload.unwrap_or_default()))
                    .map_err(|e| AdminError::BadResponse(e.to_string()))?;
                let response = sender
                    .send_request(request)
                    .await
                    .map_err(|e| self.unreachable(e.to_string()))?;
                let status = response.status().as_u16();
                let bytes = response
                    .into_body()
                    .collect()
                    .await
                    .map_err(|e| self.unreachable(e.to_string()))?
                    .to_bytes();
                Ok((status, bytes))
            }
            Address::Http(base) => {
                let mut request = self.http.request(method, format!("{base}{path}"));
                if let Some(token) = &self.token {
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
                    .map_err(|e| self.unreachable(e.to_string()))?;
                let status = response.status().as_u16();
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| self.unreachable(e.to_string()))?;
                Ok((status, bytes))
            }
        }
    }

    fn unreachable(&self, detail: String) -> AdminError {
        AdminError::Unreachable {
            address: self.address.to_string(),
            detail,
        }
    }
}

/// `/v1/credentials/{ref}{suffix}`. Refs are `[A-Za-z0-9._-]`
/// (`validate_credential_ref`), so they need no escaping; anything else is
/// percent-encoded byte by byte anyway.
fn credential_path(credential_ref: &str, suffix: &str) -> String {
    let mut encoded = String::new();
    for b in credential_ref.bytes() {
        if b.is_ascii_alphanumeric() || b"._-".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("/v1/credentials/{encoded}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse() {
        assert!(
            matches!(Address::parse("unix:/a/admin.sock"), Ok(Address::Unix(p)) if p == Path::new("/a/admin.sock"))
        );
        assert!(
            matches!(Address::parse("unix:///a/admin.sock"), Ok(Address::Unix(p)) if p == Path::new("/a/admin.sock"))
        );
        assert!(
            matches!(Address::parse("http://127.0.0.1:1/"), Ok(Address::Http(u)) if u == "http://127.0.0.1:1")
        );
        assert!(Address::parse("/a/admin.sock").is_err());
    }

    #[test]
    fn only_a_vendor_refusal_counts_as_a_dead_refresh_token() {
        let api = |code: &str, message: &str| AdminError::Api {
            status: 502,
            code: code.into(),
            message: message.into(),
        };
        assert!(api("refresh_failed", "refreshing credential \"c\" failed: token endpoint answered 400 Bad Request: invalid_grant").is_refresh_refused());
        assert!(
            api("refresh_failed", "token endpoint answered 401 Unauthorized").is_refresh_refused()
        );
        assert!(!api(
            "refresh_failed",
            "token endpoint answered 503 Service Unavailable"
        )
        .is_refresh_refused());
        assert!(!api(
            "refresh_failed",
            "error sending request: connection refused"
        )
        .is_refresh_refused());
        assert!(!api("not_found", "invalid_grant").is_refresh_refused());
    }

    #[test]
    fn refs_are_path_safe() {
        assert_eq!(
            credential_path("claude-sub.1", "/material"),
            "/v1/credentials/claude-sub.1/material"
        );
        assert_eq!(credential_path("a/b", ""), "/v1/credentials/a%2Fb");
    }
}
