//! Extractor binding configuration (`docs/caller-identity#binding`).
//!
//! The same block shape appears at three levels of the daemon config:
//! `[identity]` (the daemon default), `[upstream.identity]` (merged over the
//! default field by field) and a route's `identity` (replacing the resolved
//! upstream binding outright). [`IdentityBindingConfig`] is that block with
//! every field optional; [`IdentityBindingConfig::merged_over`] is the
//! field-by-field merge; [`IdentityBindingConfig::resolve`] turns a merged
//! block into a typed [`BindingSpec`] or says what is missing, contradictory
//! or misplaced.
//!
//! Inheritance, precisely (the docs state the field-by-field rule and the
//! same-method example; the method-change case is an implementation
//! decision recorded here):
//!
//! - An upstream block that leaves `method` unset, or repeats the default's
//!   method, inherits every field it does not set. Correlation is one
//!   logical field: setting `correlation_claim` or `correlation` replaces
//!   both.
//! - An upstream block that names a *different* method inherits nothing:
//!   the default's fields belong to the default's method, and carrying them
//!   over (a `correlation_claim` onto `trusted_header`, an `identity_claim`
//!   onto `jwt`) would silently configure the new method with the old one's
//!   settings.
//! - A route block is never merged.
//!
//! A field that the resolved method does not use is refused rather than
//! ignored, so a key in the wrong block is an error at startup, not a
//! setting that quietly does nothing.
//!
//! ```toml
//! [identity]
//! method            = "hmac"
//! source            = { header = "authorization", scheme = "Bearer " }
//! secret_ref        = "caller-identity"
//! namespace         = "janus-data-plane"
//! identity_claim    = "subject"
//! correlation_claim = "session"
//! ```

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use http::HeaderName;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use url::Url;

/// The `method` of a binding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Method {
    /// Signed claims blob verified locally with a shared secret.
    Hmac,
    /// JWT from an external issuer, verified against JWKS or a public key.
    Jwt,
    /// Random token looked up in the daemon's opaque-identity registry.
    Opaque,
    /// Header value taken as the identity with no cryptographic check.
    TrustedHeader,
    /// No identity at all: an open route. Not a verification method.
    None,
    /// `custom:<name>`: an extractor registered in a custom build.
    Custom(String),
}

impl Method {
    /// The method's config spelling, which is also the request log's
    /// `binding` value: `hmac`, `jwt`, `opaque`, `trusted_header`, `none` or
    /// `custom:<name>`.
    pub fn name(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Method::Hmac => f.write_str("hmac"),
            Method::Jwt => f.write_str("jwt"),
            Method::Opaque => f.write_str("opaque"),
            Method::TrustedHeader => f.write_str("trusted_header"),
            Method::None => f.write_str("none"),
            Method::Custom(name) => write!(f, "custom:{name}"),
        }
    }
}

impl FromStr for Method {
    type Err = BindingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hmac" => Ok(Method::Hmac),
            "jwt" => Ok(Method::Jwt),
            "opaque" => Ok(Method::Opaque),
            "trusted_header" => Ok(Method::TrustedHeader),
            "none" => Ok(Method::None),
            other => match other.strip_prefix("custom:") {
                Some(name) if !name.is_empty() && name.trim() == name => {
                    Ok(Method::Custom(name.to_owned()))
                }
                Some(_) => Err(BindingError::InvalidCustomName(other.to_owned())),
                None => Err(BindingError::UnknownMethod(other.to_owned())),
            },
        }
    }
}

impl Serialize for Method {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Method {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}

/// Where in the request the raw identity material sits.
///
/// In TOML: `{ header = "<name>" }` or `{ header = "<name>", scheme = "Bearer " }`
/// (the scheme is a prefix stripped before verification), `{ query = "<name>" }`,
/// or the bare string `"basic_password"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Header {
        name: HeaderName,
        /// Prefix such as `"Bearer "` that must be present and is stripped.
        scheme: Option<String>,
    },
    Query {
        name: String,
    },
    /// The password of HTTP basic auth, for upstreams whose SDKs only offer
    /// basic auth.
    BasicPassword,
}

impl Source {
    /// Where the material is presented, as the extractor reports it in
    /// `ExtractedIdentity::presented_at`.
    pub fn presented_at(&self) -> super::identity::PresentedAt {
        use super::identity::PresentedAt;
        match self {
            Source::Header { name, .. } => PresentedAt::Header(name.clone()),
            Source::Query { name } => PresentedAt::Query(name.clone()),
            Source::BasicPassword => PresentedAt::BasicPassword,
        }
    }
}

fn parse_header_name(name: &str) -> Result<HeaderName, BindingError> {
    HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| BindingError::InvalidHeaderName(name.to_owned()))
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SourceVisitor;

        impl<'de> Visitor<'de> for SourceVisitor {
            type Value = Source;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "\"basic_password\", { header = \"<name>\", scheme = \"<prefix>\" } or { query = \"<name>\" }",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Source, E> {
                match v {
                    "basic_password" => Ok(Source::BasicPassword),
                    other => Err(E::custom(format!(
                        "unknown identity source {other:?}: the only bare-string source is \"basic_password\""
                    ))),
                }
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Source, A::Error> {
                let mut header: Option<String> = None;
                let mut scheme: Option<String> = None;
                let mut query: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    let slot = match key.as_str() {
                        "header" => &mut header,
                        "scheme" => &mut scheme,
                        "query" => &mut query,
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &["header", "scheme", "query"],
                            ))
                        }
                    };
                    if slot.is_some() {
                        return Err(de::Error::custom(format!("duplicate source key {key:?}")));
                    }
                    *slot = Some(map.next_value()?);
                }
                match (header, scheme, query) {
                    (Some(h), scheme, None) => Ok(Source::Header {
                        name: parse_header_name(&h).map_err(de::Error::custom)?,
                        scheme,
                    }),
                    (None, None, Some(q)) if !q.is_empty() => Ok(Source::Query { name: q }),
                    (None, None, Some(_)) => Err(de::Error::custom(
                        "query source needs a non-empty parameter name",
                    )),
                    (None, Some(_), None) => {
                        Err(de::Error::custom("source `scheme` requires `header`"))
                    }
                    (None, _, Some(_)) => Err(de::Error::custom(
                        "source `scheme` applies only to a header source",
                    )),
                    (Some(_), _, Some(_)) => Err(de::Error::custom(
                        "a source is exactly one of `header` or `query`, not both",
                    )),
                    (None, None, None) => Err(de::Error::custom(
                        "empty source: give `header`, `query`, or \"basic_password\"",
                    )),
                }
            }
        }

        deserializer.deserialize_any(SourceVisitor)
    }
}

/// `correlation = { from_header = "x-request-id" }`: lift `correlation_id`
/// from an *unverified* passthrough header (`docs/caller-identity#correlation`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrelationConfig {
    pub from_header: String,
}

/// One binding block, every field optional so the three levels can be
/// merged. Fields that do not apply to the resolved method are refused by
/// [`IdentityBindingConfig::resolve`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityBindingConfig {
    pub method: Option<Method>,
    pub source: Option<Source>,
    /// `hmac`: name of the shared secret (resolved by the daemon).
    pub secret_ref: Option<String>,
    /// `hmac`: the claim set's namespace this binding accepts.
    pub namespace: Option<String>,
    /// `hmac`, `jwt`: which verified claim becomes `caller_identity`.
    pub identity_claim: Option<String>,
    /// Which verified claim becomes `correlation_id`. Mutually exclusive
    /// with `correlation.from_header`.
    pub correlation_claim: Option<String>,
    pub correlation: Option<CorrelationConfig>,
    /// `jwt`: expected `iss`.
    pub issuer: Option<String>,
    /// `jwt`: expected `aud`.
    pub audience: Option<String>,
    /// `jwt`: JWKS endpoint. Exactly one of `jwks_url` / `public_key_file`.
    pub jwks_url: Option<String>,
    /// `jwt`: PEM public key. Exactly one of `jwks_url` / `public_key_file`.
    pub public_key_file: Option<PathBuf>,
}

impl IdentityBindingConfig {
    /// `self` (an `[upstream.identity]` block) merged over `base` (the
    /// daemon default) field by field: every field `self` sets wins, every
    /// other field is inherited. The correlation setting is one logical
    /// field: setting either `correlation_claim` or `correlation` replaces
    /// both. When `self` names a method other than `base`'s, nothing is
    /// inherited (see the module docs).
    pub fn merged_over(&self, base: &IdentityBindingConfig) -> IdentityBindingConfig {
        if self.method.is_some() && self.method != base.method {
            return self.clone();
        }
        fn pick<T: Clone>(over: &Option<T>, base: &Option<T>) -> Option<T> {
            over.clone().or_else(|| base.clone())
        }
        let (correlation_claim, correlation) =
            if self.correlation_claim.is_some() || self.correlation.is_some() {
                (self.correlation_claim.clone(), self.correlation.clone())
            } else {
                (base.correlation_claim.clone(), base.correlation.clone())
            };
        IdentityBindingConfig {
            method: pick(&self.method, &base.method),
            source: pick(&self.source, &base.source),
            secret_ref: pick(&self.secret_ref, &base.secret_ref),
            namespace: pick(&self.namespace, &base.namespace),
            identity_claim: pick(&self.identity_claim, &base.identity_claim),
            correlation_claim,
            correlation,
            issuer: pick(&self.issuer, &base.issuer),
            audience: pick(&self.audience, &base.audience),
            jwks_url: pick(&self.jwks_url, &base.jwks_url),
            public_key_file: pick(&self.public_key_file, &base.public_key_file),
        }
    }

    /// Resolve a (merged) block into the typed binding it describes, or
    /// report the first missing or contradictory field. Static checks only:
    /// whether the secret exists, the JWKS endpoint answers or the custom
    /// extractor is registered is checked when extractors are constructed.
    pub fn resolve(&self) -> Result<BindingSpec, BindingError> {
        let method = self.method.clone().ok_or(BindingError::MissingMethod)?;
        self.refuse_inapplicable(&method)?;
        if method == Method::None {
            return Ok(BindingSpec::None);
        }
        let correlation = self.correlation_source()?;
        let need = |field: &'static str, v: &Option<String>| -> Result<String, BindingError> {
            match v {
                Some(s) if !s.is_empty() => Ok(s.clone()),
                _ => Err(BindingError::MissingField {
                    method: method.name(),
                    field,
                }),
            }
        };
        let source = || -> Result<Source, BindingError> {
            self.source.clone().ok_or(BindingError::MissingField {
                method: method.name(),
                field: "source",
            })
        };
        match &method {
            Method::Hmac => Ok(BindingSpec::Hmac(HmacSpec {
                source: source()?,
                secret_ref: need("secret_ref", &self.secret_ref)?,
                namespace: need("namespace", &self.namespace)?,
                identity_claim: need("identity_claim", &self.identity_claim)?,
                correlation,
            })),
            Method::Jwt => {
                let key = match (&self.jwks_url, &self.public_key_file) {
                    (Some(url), None) if !url.is_empty() => JwtKey::Jwks {
                        url: parse_jwks_url(url)?,
                    },
                    (None, Some(path)) if !path.as_os_str().is_empty() => {
                        JwtKey::PublicKeyFile(path.clone())
                    }
                    (Some(_), None) => return Err(BindingError::EmptyField("jwks_url")),
                    (None, Some(_)) => return Err(BindingError::EmptyField("public_key_file")),
                    (Some(_), Some(_)) => {
                        return Err(BindingError::Conflict {
                            a: "jwks_url",
                            b: "public_key_file",
                        })
                    }
                    _ => {
                        return Err(BindingError::MissingField {
                            method: method.name(),
                            field: "jwks_url or public_key_file",
                        })
                    }
                };
                Ok(BindingSpec::Jwt(JwtSpec {
                    source: source()?,
                    key,
                    issuer: need("issuer", &self.issuer)?,
                    audience: need("audience", &self.audience)?,
                    identity_claim: need("identity_claim", &self.identity_claim)?,
                    correlation,
                }))
            }
            Method::Opaque => Ok(BindingSpec::Opaque(OpaqueSpec {
                source: source()?,
                correlation,
            })),
            Method::TrustedHeader => {
                let (header, scheme) = match source()? {
                    Source::Header { name, scheme } => (name, scheme),
                    _ => return Err(BindingError::TrustedHeaderNeedsHeader),
                };
                if matches!(correlation, Some(Correlation::Claim(_))) {
                    return Err(BindingError::CorrelationClaimWithoutClaims(method.name()));
                }
                Ok(BindingSpec::TrustedHeader(TrustedHeaderSpec {
                    header,
                    scheme,
                    correlation,
                }))
            }
            Method::Custom(name) => Ok(BindingSpec::Custom(CustomSpec {
                name: name.clone(),
                correlation,
            })),
            Method::None => Ok(BindingSpec::None),
        }
    }

    /// Refuse every set field that `method` does not use.
    fn refuse_inapplicable(&self, method: &Method) -> Result<(), BindingError> {
        let set: [(&'static str, bool); 10] = [
            ("source", self.source.is_some()),
            ("secret_ref", self.secret_ref.is_some()),
            ("namespace", self.namespace.is_some()),
            ("identity_claim", self.identity_claim.is_some()),
            ("correlation_claim", self.correlation_claim.is_some()),
            ("correlation", self.correlation.is_some()),
            ("issuer", self.issuer.is_some()),
            ("audience", self.audience.is_some()),
            ("jwks_url", self.jwks_url.is_some()),
            ("public_key_file", self.public_key_file.is_some()),
        ];
        // `trusted_header` lists `correlation_claim` so that the more
        // specific `CorrelationClaimWithoutClaims` reports it.
        let applicable: &[&str] = match method {
            Method::None => &[],
            Method::Hmac => &[
                "source",
                "secret_ref",
                "namespace",
                "identity_claim",
                "correlation_claim",
                "correlation",
            ],
            Method::Jwt => &[
                "source",
                "identity_claim",
                "correlation_claim",
                "correlation",
                "issuer",
                "audience",
                "jwks_url",
                "public_key_file",
            ],
            Method::Opaque => &["source", "correlation_claim", "correlation"],
            Method::TrustedHeader => &["source", "correlation_claim", "correlation"],
            Method::Custom(_) => &["correlation_claim", "correlation"],
        };
        match set
            .iter()
            .find(|(field, is_set)| *is_set && !applicable.contains(field))
        {
            Some((field, _)) => Err(BindingError::NotApplicable {
                method: method.name(),
                field,
            }),
            None => Ok(()),
        }
    }

    fn correlation_source(&self) -> Result<Option<Correlation>, BindingError> {
        match (&self.correlation_claim, &self.correlation) {
            (Some(_), Some(_)) => Err(BindingError::Conflict {
                a: "correlation_claim",
                b: "correlation.from_header",
            }),
            (Some(claim), None) if !claim.is_empty() => Ok(Some(Correlation::Claim(claim.clone()))),
            (Some(_), None) => Err(BindingError::EmptyField("correlation_claim")),
            (None, Some(c)) => Ok(Some(Correlation::Header(parse_header_name(
                &c.from_header,
            )?))),
            (None, None) => Ok(None),
        }
    }
}

/// Where `correlation_id` comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Correlation {
    /// A verified claim (`correlation_claim`).
    Claim(String),
    /// An unverified, caller-controlled header (`correlation.from_header`).
    Header(HeaderName),
}

/// A resolved binding: what extractor to construct for a request type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingSpec {
    /// `method = "none"`: no extractor; the plugin is not consulted.
    None,
    Hmac(HmacSpec),
    Jwt(JwtSpec),
    Opaque(OpaqueSpec),
    TrustedHeader(TrustedHeaderSpec),
    Custom(CustomSpec),
}

impl BindingSpec {
    /// The request log's `binding` value.
    pub fn method(&self) -> Method {
        match self {
            BindingSpec::None => Method::None,
            BindingSpec::Hmac(_) => Method::Hmac,
            BindingSpec::Jwt(_) => Method::Jwt,
            BindingSpec::Opaque(_) => Method::Opaque,
            BindingSpec::TrustedHeader(_) => Method::TrustedHeader,
            BindingSpec::Custom(c) => Method::Custom(c.name.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HmacSpec {
    pub source: Source,
    pub secret_ref: String,
    pub namespace: String,
    pub identity_claim: String,
    pub correlation: Option<Correlation>,
}

/// The key a `jwt` binding names: exactly one of `jwks_url` (an http(s)
/// URL) or `public_key_file` (a PEM file path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtKey {
    Jwks { url: Url },
    PublicKeyFile(PathBuf),
}

fn parse_jwks_url(raw: &str) -> Result<Url, BindingError> {
    let url = Url::parse(raw).map_err(|e| BindingError::InvalidJwksUrl {
        url: raw.to_owned(),
        reason: e.to_string(),
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(BindingError::InvalidJwksUrl {
            url: raw.to_owned(),
            reason: "must be an http(s) URL with a host".into(),
        });
    }
    Ok(url)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JwtSpec {
    pub source: Source,
    pub key: JwtKey,
    pub issuer: String,
    pub audience: String,
    pub identity_claim: String,
    pub correlation: Option<Correlation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueSpec {
    pub source: Source,
    pub correlation: Option<Correlation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedHeaderSpec {
    pub header: HeaderName,
    pub scheme: Option<String>,
    /// Only `Correlation::Header`: a trusted header carries no claims.
    pub correlation: Option<Correlation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomSpec {
    /// The `IdentityExtractor::name` the custom build registered.
    pub name: String,
    pub correlation: Option<Correlation>,
}

/// A binding block that cannot be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindingError {
    #[error("binding has no `method` (use method = \"none\" for a route that takes no identity)")]
    MissingMethod,
    #[error("unknown identity method {0:?}")]
    UnknownMethod(String),
    #[error("invalid custom extractor reference {0:?}: expected \"custom:<name>\"")]
    InvalidCustomName(String),
    #[error("method {method:?} requires `{field}`")]
    MissingField { method: String, field: &'static str },
    #[error("`{0}` must not be empty")]
    EmptyField(&'static str),
    #[error("`{a}` and `{b}` are mutually exclusive")]
    Conflict { a: &'static str, b: &'static str },
    #[error("invalid header name {0:?}")]
    InvalidHeaderName(String),
    #[error("method \"trusted_header\" requires a header source")]
    TrustedHeaderNeedsHeader,
    #[error("method {0:?} produces no claims, so `correlation_claim` cannot apply")]
    CorrelationClaimWithoutClaims(String),
    #[error("`{field}` does not apply to method {method:?}")]
    NotApplicable { method: String, field: &'static str },
    #[error("jwks_url {url:?} is not usable: {reason}")]
    InvalidJwksUrl { url: String, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_src: &str) -> IdentityBindingConfig {
        toml::from_str(toml_src).expect("binding parses")
    }

    #[test]
    fn documented_default_binding_resolves_to_hmac() {
        let cfg = parse(
            r#"
method          = "hmac"
source          = { header = "authorization", scheme = "Bearer " }
secret_ref      = "caller-identity"
namespace       = "janus-data-plane"
identity_claim  = "subject"
correlation_claim = "session"    # optional; omit for no correlation value
"#,
        );
        let spec = cfg.resolve().expect("resolves");
        assert_eq!(
            spec,
            BindingSpec::Hmac(HmacSpec {
                source: Source::Header {
                    name: HeaderName::from_static("authorization"),
                    scheme: Some("Bearer ".into()),
                },
                secret_ref: "caller-identity".into(),
                namespace: "janus-data-plane".into(),
                identity_claim: "subject".into(),
                correlation: Some(Correlation::Claim("session".into())),
            })
        );
        assert_eq!(spec.method().name(), "hmac");
    }

    #[test]
    fn upstream_block_merges_field_by_field_over_default() {
        let default = parse(
            r#"
method = "hmac"
source = { header = "authorization", scheme = "Bearer " }
secret_ref = "caller-identity"
namespace = "janus-data-plane"
identity_claim = "subject"
correlation_claim = "session"
"#,
        );
        let upstream = parse(r#"source = { header = "x-api-key" }"#);
        let merged = upstream.merged_over(&default);
        assert_eq!(merged.method, Some(Method::Hmac));
        assert_eq!(merged.secret_ref.as_deref(), Some("caller-identity"));
        assert_eq!(
            merged.source,
            Some(Source::Header {
                name: HeaderName::from_static("x-api-key"),
                scheme: None
            })
        );
        // Correlation is one logical field: overriding with from_header
        // drops the inherited claim instead of conflicting with it.
        let with_header = parse(r#"correlation = { from_header = "x-request-id" }"#);
        let merged = with_header.merged_over(&default);
        assert_eq!(merged.correlation_claim, None);
        match merged.resolve().expect("resolves") {
            BindingSpec::Hmac(h) => assert_eq!(
                h.correlation,
                Some(Correlation::Header(HeaderName::from_static("x-request-id")))
            ),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn every_source_form_parses_and_bad_ones_are_refused() {
        #[derive(Deserialize)]
        struct S {
            source: Source,
        }
        let src = |s: &str| toml::from_str::<S>(s).map(|v| v.source);
        assert_eq!(
            src(r#"source = { query = "token" }"#).expect("query"),
            Source::Query {
                name: "token".into()
            }
        );
        assert_eq!(
            src(r#"source = "basic_password""#).expect("basic"),
            Source::BasicPassword
        );
        assert!(src(r#"source = { header = "a", query = "b" }"#).is_err());
        assert!(src(r#"source = { scheme = "Bearer " }"#).is_err());
        assert!(src(r#"source = { header = "bad header" }"#).is_err());
        assert!(src(r#"source = { cookie = "sid" }"#).is_err());
        assert!(src(r#"source = "bearer""#).is_err());
        assert_eq!(
            Source::BasicPassword.presented_at(),
            super::super::identity::PresentedAt::BasicPassword
        );
    }

    #[test]
    fn methods_parse_and_incomplete_bindings_are_refused() {
        assert_eq!(parse(r#"method = "none""#).resolve(), Ok(BindingSpec::None));
        assert_eq!(
            "custom:introspect".parse::<Method>(),
            Ok(Method::Custom("introspect".into()))
        );
        assert!(toml::from_str::<IdentityBindingConfig>(r#"method = "custom:""#).is_err());
        assert!(toml::from_str::<IdentityBindingConfig>(r#"method = "oauth""#).is_err());
        assert!(toml::from_str::<IdentityBindingConfig>(r#"metod = "hmac""#).is_err());
        assert_eq!(
            parse(r#"source = { header = "authorization" }"#).resolve(),
            Err(BindingError::MissingMethod)
        );
        assert_eq!(
            parse(r#"method = "hmac""#).resolve(),
            Err(BindingError::MissingField {
                method: "hmac".into(),
                field: "source"
            })
        );
        assert_eq!(
            parse(
                r#"
method = "trusted_header"
source = { query = "who" }
"#
            )
            .resolve(),
            Err(BindingError::TrustedHeaderNeedsHeader)
        );
        let both_keys = parse(
            r#"
method = "jwt"
source = { header = "authorization", scheme = "Bearer " }
issuer = "https://idp"
audience = "janus"
identity_claim = "sub"
jwks_url = "https://idp/jwks"
public_key_file = "/etc/janus/idp.pem"
"#,
        );
        assert_eq!(
            both_keys.resolve(),
            Err(BindingError::Conflict {
                a: "jwks_url",
                b: "public_key_file"
            })
        );
        let mut jwks_only = both_keys.clone();
        jwks_only.public_key_file = None;
        assert!(matches!(
            jwks_only.resolve(),
            Ok(BindingSpec::Jwt(JwtSpec {
                key: JwtKey::Jwks { .. },
                ..
            }))
        ));
        let custom = parse(r#"method = "custom:introspect""#)
            .resolve()
            .expect("custom");
        assert_eq!(custom.method().name(), "custom:introspect");

        let mut empty_key = both_keys.clone();
        empty_key.jwks_url = None;
        empty_key.public_key_file = Some(PathBuf::new());
        assert_eq!(
            empty_key.resolve(),
            Err(BindingError::EmptyField("public_key_file"))
        );
        let mut bad_url = both_keys.clone();
        bad_url.public_key_file = None;
        for url in ["not a url", "file:///etc/jwks.json"] {
            bad_url.jwks_url = Some(url.into());
            assert!(
                matches!(bad_url.resolve(), Err(BindingError::InvalidJwksUrl { .. })),
                "{url}"
            );
        }
    }

    #[test]
    fn correlation_rules_are_enforced() {
        assert_eq!(
            parse(
                r#"
method = "trusted_header"
source = { header = "x-user" }
correlation_claim = "session"
"#
            )
            .resolve(),
            Err(BindingError::CorrelationClaimWithoutClaims(
                "trusted_header".into()
            ))
        );
        assert_eq!(
            parse(
                r#"
method = "opaque"
source = { header = "authorization", scheme = "Bearer " }
correlation_claim = ""
"#
            )
            .resolve(),
            Err(BindingError::EmptyField("correlation_claim"))
        );
        assert_eq!(
            parse(
                r#"
method = "opaque"
source = { header = "authorization" }
correlation_claim = "s"
correlation = { from_header = "x-request-id" }
"#
            )
            .resolve(),
            Err(BindingError::Conflict {
                a: "correlation_claim",
                b: "correlation.from_header"
            })
        );
        let th = parse(
            r#"
method = "trusted_header"
source = { header = "x-user" }
correlation = { from_header = "x-request-id" }
"#,
        )
        .resolve()
        .expect("header correlation is allowed");
        assert!(matches!(
            th,
            BindingSpec::TrustedHeader(TrustedHeaderSpec {
                correlation: Some(Correlation::Header(_)),
                ..
            })
        ));
    }

    #[test]
    fn fields_the_method_does_not_use_are_refused() {
        let cases = [
            (
                "method = \"none\"\nsource = { header = \"a\" }",
                "none",
                "source",
            ),
            (
                "method = \"opaque\"\nsource = { header = \"a\" }\nsecret_ref = \"s\"",
                "opaque",
                "secret_ref",
            ),
            (
                "method = \"jwt\"\nsource = { header = \"a\" }\nnamespace = \"n\"",
                "jwt",
                "namespace",
            ),
            (
                "method = \"hmac\"\nsource = { header = \"a\" }\nissuer = \"i\"",
                "hmac",
                "issuer",
            ),
            (
                "method = \"custom:introspect\"\nsource = { header = \"a\" }",
                "custom:introspect",
                "source",
            ),
        ];
        for (src, method, field) in cases {
            assert_eq!(
                parse(src).resolve(),
                Err(BindingError::NotApplicable {
                    method: method.into(),
                    field
                }),
                "{src}"
            );
        }
    }

    #[test]
    fn switching_method_at_the_upstream_inherits_nothing() {
        let default = parse(
            r#"
method = "hmac"
source = { header = "authorization", scheme = "Bearer " }
secret_ref = "caller-identity"
namespace = "janus-data-plane"
identity_claim = "subject"
correlation_claim = "session"
"#,
        );
        // Without the reset this would inherit `correlation_claim` (refused
        // for trusted_header) and the hmac-only fields.
        let upstream = parse(
            r#"
method = "trusted_header"
source = { header = "x-user" }
"#,
        );
        let merged = upstream.merged_over(&default);
        assert_eq!(merged, upstream);
        assert_eq!(
            merged.resolve(),
            Ok(BindingSpec::TrustedHeader(TrustedHeaderSpec {
                header: HeaderName::from_static("x-user"),
                scheme: None,
                correlation: None,
            }))
        );
        // Restating the same method still inherits.
        let same = parse(r#"method = "hmac""#).merged_over(&default);
        assert_eq!(same, default);
        // And `none` at the upstream is a clean open binding.
        assert_eq!(
            parse(r#"method = "none""#).merged_over(&default).resolve(),
            Ok(BindingSpec::None)
        );
    }
}
