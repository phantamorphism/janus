//! The shared reading of an upstream's error response that each upstream's
//! `classify` builds its verdict from: the JSON error object all three
//! launch upstreams use, a back-off time, and a short diagnostic for the
//! pool's `last_error`, the request log and the Notify events.
//!
//! # When a response is a `credential_invalid` verdict
//!
//! `CredentialInvalid` takes a shared credential out of service for every
//! caller until an operator replaces its material, while the request that
//! provoked it — its path, body and the betas it declares — is the
//! caller's. So a response is that verdict only when the upstream's own
//! JSON error says the credential itself was not accepted: a 401 whose
//! error type or code is the upstream's documented "this key/token is not
//! valid" (each upstream module lists the ones it trusts). The status
//! alone never is:
//!
//! - A 403 is about the request, not the key — a restricted key's missing
//!   scope, an organisation or region policy, a moderation refusal, or an
//!   edge (Cloudflare) challenge page. It is relayed to the caller with no
//!   verdict, so one caller probing an endpoint outside the key's scopes
//!   cannot disable the credential for everyone else.
//! - A 401 without a recognised JSON error (an HTML page from an edge, an
//!   error type the module does not list, OpenAI's 401 "missing scopes"
//!   with no code) is likewise relayed without a verdict.
//! - An *expired* token is not an invalid one: expiry is the pool's to
//!   handle by refreshing, and a refresh that then fails is reported by the
//!   pool, not here.
//!
//! The cost of the rule is a credential that is revoked in a way the
//! upstream reports unrecognisably stays in rotation, failing its requests
//! visibly, instead of being benched; the reverse error (any caller
//! benching any credential) is the one the rule exists to prevent.

use chrono::{DateTime, Duration, Utc};
use http::{header, HeaderMap, StatusCode};
use serde_json::Value;

/// How much of an upstream's error message is kept in a reason.
const REASON_LIMIT: usize = 200;

/// The `error` object of an upstream error body: `{"error": {"type",
/// "code", "message", ...}}` (Anthropic additionally wraps it in
/// `{"type": "error", ...}`; an `error` that is a bare string is its
/// message).
#[derive(Debug, Default)]
pub(crate) struct ErrorBody {
    /// `error.type`.
    pub kind: Option<String>,
    /// `error.code`, which OpenAI and OpenRouter use (a string or a number).
    pub code: Option<String>,
    pub message: Option<String>,
    /// The whole `error` object, for upstream-specific fields.
    pub raw: Value,
}

impl ErrorBody {
    /// Parse `body` (possibly cut off by the relay's prefix limit, in which
    /// case nothing is recognised).
    pub fn parse(body: &[u8]) -> ErrorBody {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return ErrorBody::default();
        };
        let error = value.get("error").cloned().unwrap_or(Value::Null);
        let text = |v: Option<&Value>| match v {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        match &error {
            Value::String(message) => ErrorBody {
                message: Some(message.clone()),
                raw: error.clone(),
                ..ErrorBody::default()
            },
            Value::Object(fields) => ErrorBody {
                kind: text(fields.get("type")),
                code: text(fields.get("code")),
                message: text(fields.get("message")),
                raw: error.clone(),
            },
            _ => ErrorBody::default(),
        }
    }

    /// Whether `error.type` or `error.code` is `name`.
    pub fn is(&self, name: &str) -> bool {
        self.kind.as_deref() == Some(name) || self.code.as_deref() == Some(name)
    }

    /// Whether the message says the token has expired (rather than that it
    /// was revoked or never valid).
    pub fn says_expired(&self) -> bool {
        self.message
            .as_deref()
            .is_some_and(|m| m.to_ascii_lowercase().contains("expired"))
    }

    /// `"<status>: <type or code>: <message>"`, with the message cut to
    /// [`REASON_LIMIT`] characters.
    pub fn reason(&self, status: StatusCode) -> String {
        let mut reason = status.to_string();
        if let Some(kind) = self.kind.as_ref().or(self.code.as_ref()) {
            reason.push_str(": ");
            reason.push_str(kind);
        }
        if let Some(message) = &self.message {
            reason.push_str(": ");
            reason.extend(message.chars().take(REASON_LIMIT));
        }
        reason
    }
}

/// When a throttled credential may be tried again, from `Retry-After`
/// (delay-seconds or an HTTP-date). `None` when the upstream did not say.
pub(crate) fn retry_after(headers: &HeaderMap, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u32>() {
        return Some(now + Duration::seconds(i64::from(seconds)));
    }
    DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn error_bodies_of_each_launch_upstream_are_read() {
        let anthropic = ErrorBody::parse(
            br#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        );
        assert_eq!(anthropic.kind.as_deref(), Some("authentication_error"));
        assert_eq!(
            anthropic.reason(StatusCode::UNAUTHORIZED),
            "401 Unauthorized: authentication_error: invalid x-api-key"
        );
        let openai = ErrorBody::parse(
            br#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#,
        );
        assert!(openai.is("insufficient_quota"));
        let openrouter =
            ErrorBody::parse(br#"{"error":{"code":402,"message":"Insufficient credits"}}"#);
        assert_eq!(openrouter.code.as_deref(), Some("402"));
        let bare = ErrorBody::parse(br#"{"error":"nope"}"#);
        assert_eq!(bare.message.as_deref(), Some("nope"));
        let cut_off = ErrorBody::parse(br#"{"error":{"type":"authentic"#);
        assert_eq!(cut_off.reason(StatusCode::FORBIDDEN), "403 Forbidden");
        let long = ErrorBody::parse(
            format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(5000)).as_bytes(),
        );
        assert!(long.reason(StatusCode::UNAUTHORIZED).len() < REASON_LIMIT + 40);
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates() {
        let now = Utc.with_ymd_and_hms(2026, 11, 3, 12, 0, 0).unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after(&headers, now), None);
        headers.insert(header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(
            retry_after(&headers, now),
            Some(now + Duration::seconds(30))
        );
        headers.insert(
            header::RETRY_AFTER,
            "Tue, 03 Nov 2026 12:05:00 GMT".parse().unwrap(),
        );
        assert_eq!(
            retry_after(&headers, now),
            Some(Utc.with_ymd_and_hms(2026, 11, 3, 12, 5, 0).unwrap())
        );
        headers.insert(header::RETRY_AFTER, "soon".parse().unwrap());
        assert_eq!(retry_after(&headers, now), None);
    }
}
