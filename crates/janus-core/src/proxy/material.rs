//! Reading material documents and refreshing OAuth tokens: the pieces the
//! per-upstream `decode_material` and `refresh` share.

use std::time::Duration;

use chrono::{DateTime, Utc};
use http::HeaderValue;
use serde_json::Value;
use url::Url;

use crate::contract::credential::{CredentialMaterial, MaterialDocument, Secret};
use crate::contract::upstream::{UpstreamError, UpstreamKind};

use super::relay::{is_dead_connection, transport_error};

/// A token endpoint answers a small JSON exchange; it gets this long.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// A plain API key document: the key as a JSON string, or exactly
/// `{"api_key": "<key>"}` (the pool's canonical stored form). Surrounding
/// whitespace (a trailing newline from a file or stdin) is not part of a
/// key. `None` when `doc` is neither form.
///
/// A key is one token of printable, non-space ASCII — what every launch
/// upstream issues. Anything else claimed as a key is an error, never a
/// secret: in particular text that starts like a JSON document, which is
/// what a credential file read mid-rewrite (`{"tokens":{"access_tok`)
/// looks like once it has failed to parse as JSON. Sending that upstream
/// would leak the file's other secrets as an auth header.
pub(crate) fn validated_api_key(doc: &MaterialDocument) -> Option<Result<Secret, UpstreamError>> {
    let key = match doc.expose() {
        Value::String(key) => key,
        Value::Object(fields) if fields.len() == 1 => match fields.get("api_key") {
            Some(Value::String(key)) => key,
            _ => return None,
        },
        _ => return None,
    };
    let key = key.trim();
    let refuse = |why: &str| Some(Err(UpstreamError::InvalidMaterial(why.into())));
    if key.is_empty() {
        return refuse("the API key is empty");
    }
    if key.starts_with(['{', '[', '"']) {
        return refuse("the API key is a fragment of a JSON document, not a key");
    }
    if !key.bytes().all(|b| b.is_ascii_graphic()) {
        return refuse("the API key contains whitespace or non-printable characters");
    }
    Some(Ok(Secret::new(key)))
}

/// Refuse material resolved for a different upstream: applying it would
/// send one upstream's secret to another.
pub(crate) fn ensure_for(
    upstream: UpstreamKind,
    material: &CredentialMaterial,
) -> Result<(), UpstreamError> {
    if material.upstream == upstream {
        Ok(())
    } else {
        Err(UpstreamError::Auth(format!(
            "credential {:?} is for {}, not {upstream}",
            material.credential_ref, material.upstream
        )))
    }
}

/// `prefix` followed by the secret, as a header value marked sensitive (so
/// HTTP/2 never indexes it and debug output redacts it).
pub(crate) fn secret_value(prefix: &str, secret: &Secret) -> Result<HeaderValue, UpstreamError> {
    let text = secret
        .expose_str()
        .ok_or_else(|| UpstreamError::Auth("the secret is not UTF-8".into()))?;
    let mut value = HeaderValue::from_str(&format!("{prefix}{text}"))
        .map_err(|_| UpstreamError::Auth("the secret is not a valid header value".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

/// A non-empty string field of a JSON object.
pub(crate) fn string_field<'v>(
    object: &'v Value,
    field: &str,
    what: &str,
) -> Result<&'v str, UpstreamError> {
    match object.get(field) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s),
        _ => Err(UpstreamError::InvalidMaterial(format!(
            "{what} has no {field:?} string"
        ))),
    }
}

/// The `exp` claim of a JWT, read without verifying it — only to know when
/// to refresh, never to trust anything. `None` when `token` is not a JWT
/// with a numeric `exp`.
pub(crate) fn jwt_expiry(token: &str) -> Option<DateTime<Utc>> {
    let data = jsonwebtoken::dangerous::insecure_decode::<Value>(token).ok()?;
    DateTime::from_timestamp(data.claims.get("exp")?.as_i64()?, 0)
}

/// POST a JSON refresh-token grant to `token_url` and return the JSON
/// answer. A failure names the status and the OAuth `error` code only: a
/// token endpoint's body is not echoed into diagnostics.
pub(crate) async fn refresh_grant(
    client: &reqwest::Client,
    token_url: &Url,
    grant: &Value,
) -> Result<Value, UpstreamError> {
    let send = || {
        client
            .post(token_url.clone())
            .timeout(REFRESH_TIMEOUT)
            .json(grant)
            .send()
    };
    // A keep-alive connection the endpoint closed since the last grant is
    // only found out by writing to it; the request then fails without an
    // answer. That is the pool's staleness, not the endpoint's verdict, so
    // the grant is sent once more (on a fresh connection). Only that
    // signature is retried (the forwarder's predicate): any other failure
    // may mean the endpoint processed the grant, and a rotating refresh
    // token must not be spent twice.
    let response = match send().await {
        Err(e) if is_dead_connection(&e) => {
            tracing::debug!(error = %transport_error(e), "token endpoint request failed; retrying once");
            send().await
        }
        first => first,
    }
    .map_err(|e| UpstreamError::RefreshFailed(transport_error(e)))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let code = body.get("error").and_then(|e| match e {
            Value::String(code) => Some(code.as_str()),
            Value::Object(fields) => fields.get("code").or(fields.get("type"))?.as_str(),
            _ => None,
        });
        return Err(UpstreamError::RefreshFailed(match code {
            Some(code) => format!("token endpoint answered {status}: {code}"),
            None => format!("token endpoint answered {status}"),
        }));
    }
    if !body.is_object() {
        return Err(UpstreamError::RefreshFailed(
            "token endpoint answered with no JSON object".into(),
        ));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validated_api_keys_in_both_document_forms() {
        let key = |v: Value| validated_api_key(&MaterialDocument::new(v));
        assert_eq!(key(json!("sk-1\n")).unwrap().unwrap(), Secret::new("sk-1"));
        assert_eq!(
            key(json!({"api_key": " sk-2 "})).unwrap().unwrap(),
            Secret::new("sk-2")
        );
        assert!(matches!(
            key(json!("  ")),
            Some(Err(UpstreamError::InvalidMaterial(_)))
        ));
        assert!(key(json!({"tokens": {}})).is_none());
        assert!(key(json!({"api_key": "sk", "tokens": {}})).is_none());
        assert!(key(json!(42)).is_none());
    }

    #[test]
    fn a_truncated_json_document_is_an_error_never_a_key() {
        // What a config `file` credential read mid-rewrite becomes once it
        // fails to parse as JSON and is taken as text, in either form.
        let fragment = r#"{"tokens":{"access_token":"at","refresh_token":"rt""#;
        for doc in [
            json!(fragment),
            json!({ "api_key": fragment }),
            json!("[1,"),
            json!("\"sk-quoted"),
            json!("sk one"),
            json!("sk\u{7f}"),
        ] {
            assert!(
                matches!(
                    validated_api_key(&MaterialDocument::new(doc.clone())),
                    Some(Err(UpstreamError::InvalidMaterial(_)))
                ),
                "{doc}"
            );
        }
    }

    /// A token endpoint on a raw socket: `script[n]` is what the n-th
    /// request on any connection gets. `None` closes the connection having
    /// read the request and answered nothing (what a stale pooled
    /// connection looks like); `Some((status, body))` answers and keeps the
    /// connection open for reuse.
    async fn token_endpoint(
        script: Vec<Option<(u16, &'static str)>>,
    ) -> (Url, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap();
        let seen = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (script, counter) = (script.clone(), std::sync::Arc::clone(&counter));
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => {}
                        }
                        let n = counter.fetch_add(1, Ordering::SeqCst);
                        match script.get(n).copied().flatten() {
                            None => return,
                            Some((status, body)) => {
                                let reply = format!(
                                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                                    body.len()
                                );
                                if stream.write_all(reply.as_bytes()).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
        });
        (url, seen)
    }

    #[tokio::test]
    async fn a_grant_sent_on_a_stale_pooled_connection_is_retried_once() {
        use std::sync::atomic::Ordering;
        // 400, then the endpoint drops the (reused) connection with no
        // answer, then 200 on the fresh one.
        let (url, seen) = token_endpoint(vec![
            Some((400, r#"{"error":"invalid_grant"}"#)),
            None,
            Some((200, r#"{"access_token":"a"}"#)),
        ])
        .await;
        let client = crate::proxy::http_client().unwrap();
        let first = refresh_grant(&client, &url, &json!({})).await.unwrap_err();
        assert!(
            first.to_string().contains("400 Bad Request: invalid_grant"),
            "{first}"
        );
        let second = refresh_grant(&client, &url, &json!({})).await.unwrap();
        assert_eq!(second["access_token"], "a");
        assert_eq!(seen.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_second_transport_failure_is_reported_with_its_cause() {
        use std::sync::atomic::Ordering;
        let (url, seen) = token_endpoint(vec![None, None, None]).await;
        let client = crate::proxy::http_client().unwrap();
        let e = refresh_grant(&client, &url, &json!({})).await.unwrap_err();
        assert_eq!(seen.load(Ordering::SeqCst), 2, "one retry, no more");
        let text = e.to_string();
        assert!(
            text.contains("error sending request: "),
            "the cause is named: {text}"
        );
        assert!(!text.contains("127.0.0.1"), "no URL in diagnostics: {text}");
    }

    #[test]
    fn jwt_expiry_is_read_unverified() {
        let token = crate::proxy::testkit::jwt(json!({"exp": 1_900_000_000}));
        assert_eq!(
            jwt_expiry(&token),
            DateTime::from_timestamp(1_900_000_000, 0)
        );
        assert_eq!(jwt_expiry("opaque-token"), None);
        let no_exp = crate::proxy::testkit::jwt(json!({"sub": "x"}));
        assert_eq!(jwt_expiry(&no_exp), None);
    }
}
