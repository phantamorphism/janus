//! Reading material documents and refreshing OAuth tokens: the pieces the
//! per-upstream `decode_material` and `refresh` share.

use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, Utc};
use http::HeaderValue;
use serde_json::Value;
use url::Url;

use crate::contract::credential::{CredentialMaterial, MaterialDocument, Secret};
use crate::contract::upstream::{UpstreamError, UpstreamKind};

/// A token endpoint answers a small JSON exchange; it gets this long.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// A plain API key document: the key as a JSON string, or `{"api_key":
/// "<key>"}`. Surrounding whitespace (a trailing newline from a file or
/// stdin) is not part of a key. `None` when `doc` is neither form.
pub(crate) fn plain_api_key(doc: &MaterialDocument) -> Option<Result<Secret, UpstreamError>> {
    let key = match doc.expose() {
        Value::String(key) => key,
        Value::Object(fields) => match fields.get("api_key") {
            Some(Value::String(key)) => key,
            _ => return None,
        },
        _ => return None,
    };
    let key = key.trim();
    Some(if key.is_empty() {
        Err(UpstreamError::InvalidMaterial(
            "the API key is empty".into(),
        ))
    } else {
        Ok(Secret::new(key))
    })
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
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    DateTime::from_timestamp(claims.get("exp")?.as_i64()?, 0)
}

/// POST a JSON refresh-token grant to `token_url` and return the JSON
/// answer. A failure names the status and the OAuth `error` code only: a
/// token endpoint's body is not echoed into diagnostics.
pub(crate) async fn refresh_grant(
    client: &reqwest::Client,
    token_url: &Url,
    grant: &Value,
) -> Result<Value, UpstreamError> {
    let response = client
        .post(token_url.clone())
        .timeout(REFRESH_TIMEOUT)
        .json(grant)
        .send()
        .await
        .map_err(|e| UpstreamError::RefreshFailed(e.without_url().to_string()))?;
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
    fn plain_api_keys_in_both_document_forms() {
        let key = |v: Value| plain_api_key(&MaterialDocument::new(v));
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
        assert!(key(json!(42)).is_none());
    }

    #[test]
    fn jwt_expiry_is_read_unverified() {
        let b64 = |v: Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap())
        };
        let token = format!(
            "{}.{}.sig",
            b64(json!({"alg": "RS256"})),
            b64(json!({"exp": 1_900_000_000}))
        );
        assert_eq!(
            jwt_expiry(&token),
            DateTime::from_timestamp(1_900_000_000, 0)
        );
        assert_eq!(jwt_expiry("opaque-token"), None);
        let no_exp = format!("h.{}.s", b64(json!({"sub": "x"})));
        assert_eq!(jwt_expiry(&no_exp), None);
    }
}
