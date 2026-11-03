//! Reading the raw identity material from where a binding's source says it
//! sits (`docs/caller-identity#methods`, "Each method is paired with a
//! source").

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use http::header::AUTHORIZATION;
use http::{HeaderName, HeaderValue};

use crate::contract::binding::Source;
use crate::contract::identity::{ExtractionError, RequestHead};

/// The raw material at `source`, the scheme prefix already stripped and
/// surrounding whitespace trimmed. Absent or empty: `NoMaterial`. Present
/// but unusable (repeated, not text, wrong scheme, undecodable basic
/// credentials): `Invalid`.
pub(crate) fn read(source: &Source, head: &RequestHead) -> Result<String, ExtractionError> {
    let raw = match source {
        Source::Header { name, scheme } => {
            let Some(value) = single_header(head, name)? else {
                return Err(ExtractionError::NoMaterial);
            };
            let text = header_text(name, value)?;
            match scheme {
                Some(scheme) => strip_scheme(text, scheme).ok_or_else(|| {
                    ExtractionError::Invalid(format!(
                        "header {name} does not carry the expected {:?} scheme",
                        scheme.trim()
                    ))
                })?,
                None => text,
            }
            .to_owned()
        }
        Source::Query { name } => query_param(head, name)?,
        Source::BasicPassword => basic_password(head)?,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ExtractionError::NoMaterial);
    }
    Ok(trimmed.to_owned())
}

/// The one value of header `name`, `None` if absent. Two or more values are
/// refused: which one carries the identity would be ambiguous.
fn single_header<'a>(
    head: &'a RequestHead,
    name: &HeaderName,
) -> Result<Option<&'a HeaderValue>, ExtractionError> {
    let mut values = head.headers.get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(ExtractionError::Invalid(format!(
            "header {name} is present more than once"
        )));
    }
    Ok(first)
}

fn header_text<'a>(name: &HeaderName, value: &'a HeaderValue) -> Result<&'a str, ExtractionError> {
    std::str::from_utf8(value.as_bytes())
        .map_err(|_| ExtractionError::Invalid(format!("header {name} is not valid UTF-8")))
}

/// `value` with the leading `scheme` removed, matched ASCII
/// case-insensitively; `None` when `value` does not start with it.
fn strip_scheme<'a>(value: &'a str, scheme: &str) -> Option<&'a str> {
    let n = scheme.len();
    let head = value.as_bytes().get(..n)?;
    if head.eq_ignore_ascii_case(scheme.as_bytes()) {
        value.get(n..)
    } else {
        None
    }
}

fn query_param(head: &RequestHead, name: &str) -> Result<String, ExtractionError> {
    let Some(query) = head.query() else {
        return Err(ExtractionError::NoMaterial);
    };
    let mut found = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned());
    let first = found.next().ok_or(ExtractionError::NoMaterial)?;
    if found.next().is_some() {
        return Err(ExtractionError::Invalid(format!(
            "query parameter {name:?} is present more than once"
        )));
    }
    Ok(first)
}

/// The password half of `Authorization: Basic base64(user:password)`.
fn basic_password(head: &RequestHead) -> Result<String, ExtractionError> {
    let Some(value) = single_header(head, &AUTHORIZATION)? else {
        return Err(ExtractionError::NoMaterial);
    };
    let text = header_text(&AUTHORIZATION, value)?;
    let encoded = strip_scheme(text, "Basic ")
        .ok_or_else(|| ExtractionError::Invalid("authorization is not HTTP basic auth".into()))?;
    let decoded = STANDARD
        .decode(encoded.trim())
        .map_err(|_| ExtractionError::Invalid("basic credentials are not valid base64".into()))?;
    let decoded = String::from_utf8(decoded)
        .map_err(|_| ExtractionError::Invalid("basic credentials are not valid UTF-8".into()))?;
    let (_user, password) = decoded.split_once(':').ok_or_else(|| {
        ExtractionError::Invalid("basic credentials have no user:password separator".into())
    })?;
    Ok(password.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::testkit::head;

    fn header_source(name: &'static str, scheme: Option<&str>) -> Source {
        Source::Header {
            name: HeaderName::from_static(name),
            scheme: scheme.map(str::to_owned),
        }
    }

    #[test]
    fn header_source_strips_scheme_case_insensitively_and_refuses_ambiguity() {
        let bearer = header_source("authorization", Some("Bearer "));
        let req = head("/x", &[("authorization", "bearer  tok-1 ")]);
        assert_eq!(read(&bearer, &req), Ok("tok-1".into()));
        assert_eq!(
            read(&bearer, &head("/x", &[])),
            Err(ExtractionError::NoMaterial)
        );
        assert_eq!(
            read(&bearer, &head("/x", &[("authorization", "Bearer   ")])),
            Err(ExtractionError::NoMaterial)
        );
        assert!(matches!(
            read(&bearer, &head("/x", &[("authorization", "Basic dTpw")])),
            Err(ExtractionError::Invalid(_))
        ));
        assert!(matches!(
            read(
                &bearer,
                &head(
                    "/x",
                    &[("authorization", "Bearer a"), ("authorization", "Bearer b")]
                )
            ),
            Err(ExtractionError::Invalid(_))
        ));
        let plain = header_source("x-api-key", None);
        assert_eq!(
            read(&plain, &head("/x", &[("x-api-key", "k-9")])),
            Ok("k-9".into())
        );
        // A scheme longer than the value, or a multibyte value, is not a panic.
        assert!(read(&bearer, &head("/x", &[("authorization", "Be")])).is_err());
    }

    #[test]
    fn query_source_decodes_and_refuses_repeats() {
        let q = Source::Query {
            name: "token".into(),
        };
        assert_eq!(
            read(&q, &head("/v1/models?a=1&token=ab%2Bc", &[])),
            Ok("ab+c".into())
        );
        assert_eq!(
            read(&q, &head("/v1/models?a=1", &[])),
            Err(ExtractionError::NoMaterial)
        );
        assert_eq!(
            read(&q, &head("/v1/models", &[])),
            Err(ExtractionError::NoMaterial)
        );
        assert!(matches!(
            read(&q, &head("/v1/models?token=a&token=b", &[])),
            Err(ExtractionError::Invalid(_))
        ));
    }

    #[test]
    fn basic_password_source_takes_the_password_half() {
        let enc = STANDARD.encode("sdk-user:secret:with:colons");
        let req = head("/x", &[("authorization", &format!("Basic {enc}"))]);
        assert_eq!(
            read(&Source::BasicPassword, &req),
            Ok("secret:with:colons".into())
        );
        let empty = STANDARD.encode("user:");
        assert_eq!(
            read(
                &Source::BasicPassword,
                &head("/x", &[("authorization", &format!("Basic {empty}"))])
            ),
            Err(ExtractionError::NoMaterial)
        );
        for bad in ["Basic !!!", "Basic dXNlcg==", "Bearer abc"] {
            assert!(
                matches!(
                    read(
                        &Source::BasicPassword,
                        &head("/x", &[("authorization", bad)])
                    ),
                    Err(ExtractionError::Invalid(_))
                ),
                "{bad}"
            );
        }
        assert_eq!(
            read(&Source::BasicPassword, &head("/x", &[])),
            Err(ExtractionError::NoMaterial)
        );
    }
}
