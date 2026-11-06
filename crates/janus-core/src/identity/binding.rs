//! Binding extractors to request types (`docs/caller-identity#binding`):
//! constructing every binding's extractor at startup and selecting one per
//! request, route first, then the upstream's binding (which the config
//! layer has already merged over the daemon default).
//!
//! Route matching. The docs call a route's `path` a pattern without giving
//! a pattern syntax, so a route path is one literal path. It is compared in
//! canonical form, on both sides: runs of `/` collapsed, a trailing `/`
//! dropped, percent-encoded unreserved characters (`A-Z a-z 0-9 - . _ ~`)
//! decoded and other escapes upper-cased. An upstream may treat
//! `/v1/messages/`, `//v1/messages` and `/v1/%6Dessages` as `/v1/messages`;
//! were they matched literally, a route stricter than its upstream would be
//! bypassed by respelling the path. Paths stay case-sensitive. A configured
//! route path must already be canonical, start with `/`, and contain no
//! query, fragment or dot segment, so that what the operator wrote is what
//! matches and what the request log reports.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use crate::contract::binding::{BindingSpec, JwtKey, Method};
use crate::contract::credential::Secret;
use crate::contract::identity::{
    ExtractedIdentity, ExtractionError, ExtractorBuildError, ExtractorFactory, ExtractorInput,
    IdentityExtractor, JwtKeyInput, RequestHead,
};
use crate::contract::registry::OpaqueRegistry;

use super::custom::CustomBinding;

/// Resolves the material a binding only names. Implemented by the daemon,
/// which owns `[secrets]`, the filesystem and the registry store.
pub trait MaterialResolver: Send + Sync {
    /// The bytes of the shared secret an `hmac` binding's `secret_ref`
    /// names. `Err`: why it cannot be had (undeclared, unreadable, ...).
    fn secret(&self, secret_ref: &str) -> Result<Secret, String>;

    /// The contents of a `jwt` binding's `public_key_file`.
    fn public_key_file(&self, path: &Path) -> Result<Vec<u8>, String>;

    /// The opaque-identity registry the `opaque` method probes; `None` when
    /// this daemon has none.
    fn opaque_registry(&self) -> Option<Arc<dyn OpaqueRegistry>>;
}

/// A config's bindings could not all be turned into extractors. The daemon
/// must not start: it could not establish who is calling.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartupError {
    /// Named material is missing or unreadable (secret, key file, registry).
    #[error("{at}: {reason}")]
    Material { at: String, reason: String },
    /// The material was resolved but the extractor cannot be built with it.
    #[error("{at}: {source}")]
    Build {
        at: String,
        #[source]
        source: ExtractorBuildError,
    },
    #[error("{at}: custom extractor {name:?} is not registered in this build")]
    UnregisteredCustom { at: String, name: String },
    #[error("extractors: two custom extractors are registered as {name:?}")]
    DuplicateCustom { name: String },
    #[error("{at}: declared more than once")]
    Duplicate { at: String },
    #[error("{at}: route of an upstream whose binding was not declared first")]
    UndeclaredUpstream { at: String },
    /// A route path that could never match, or matches under another
    /// spelling than the one written.
    #[error("{at}: {reason}")]
    InvalidRoutePath { at: String, reason: String },
}

impl StartupError {
    /// The config location the error points at, in the daemon config's
    /// `upstream[<name>].route[<path>].identity.<field>` form.
    pub fn at(&self) -> &str {
        match self {
            StartupError::Material { at, .. }
            | StartupError::Build { at, .. }
            | StartupError::UnregisteredCustom { at, .. }
            | StartupError::Duplicate { at }
            | StartupError::UndeclaredUpstream { at }
            | StartupError::InvalidRoutePath { at, .. } => at,
            StartupError::DuplicateCustom { .. } => "extractors",
        }
    }
}

/// One constructed binding: the method and, unless it is `none`, its
/// extractor.
#[derive(Clone)]
pub struct Binding {
    method: Method,
    extractor: Option<Arc<dyn IdentityExtractor>>,
}

/// An extraction failure, with what the material claimed for the request
/// log. `claimed_identity` is unverified: log it, never authorize with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub error: ExtractionError,
    pub claimed_identity: Option<String>,
}

impl Binding {
    /// The request log's `binding` value.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Run the request head through this binding. `Ok(None)`: a
    /// `method = "none"` route, which produces no identity and must not be
    /// handed to the plugin. `Err`: a 401 and nothing else.
    pub async fn identify(
        &self,
        head: &RequestHead,
    ) -> Result<Option<ExtractedIdentity>, Rejection> {
        let Some(extractor) = &self.extractor else {
            return Ok(None);
        };
        extractor
            .extract(head)
            .await
            .map(Some)
            .map_err(|error| Rejection {
                error,
                claimed_identity: extractor.claimed_identity(head),
            })
    }
}

impl fmt::Debug for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Binding")
            .field("method", &self.method.name())
            .finish()
    }
}

/// The binding that applies to a request.
#[derive(Debug, Clone, Copy)]
pub struct Selected<'a> {
    /// The matched route path, when a route binding applied (the request
    /// log's `route`).
    pub route: Option<&'a str>,
    pub binding: &'a Binding,
}

/// No binding resolves for the request: it is rejected. There is no implicit
/// unauthenticated tier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no identity binding applies to {upstream} path {path:?}")]
pub struct Unbound {
    /// The upstream instance name.
    pub upstream: String,
    pub path: String,
}

#[derive(Debug, Default)]
struct UpstreamEntry {
    binding: Option<Binding>,
    routes: BTreeMap<String, Binding>,
}

/// Every binding of a config, constructed.
#[derive(Debug, Default)]
pub struct BindingTable {
    upstreams: BTreeMap<String, UpstreamEntry>,
}

impl BindingTable {
    /// Start building. `custom` is the build's registered custom extractors
    /// (`Embedded::extractors`); two with one name is a startup error.
    pub fn builder<'a>(
        factory: &'a dyn ExtractorFactory,
        materials: &'a dyn MaterialResolver,
        custom: &[Arc<dyn IdentityExtractor>],
    ) -> Result<BindingTableBuilder<'a>, StartupError> {
        let mut registered = BTreeMap::new();
        for e in custom {
            if registered.insert(e.name().to_owned(), e.clone()).is_some() {
                return Err(StartupError::DuplicateCustom {
                    name: e.name().to_owned(),
                });
            }
        }
        Ok(BindingTableBuilder {
            factory,
            materials,
            custom: registered,
            built: Vec::new(),
            table: BindingTable::default(),
        })
    }

    /// The binding for `path` (the request path with the upstream's
    /// `path_prefix` stripped): the route whose path equals `path` in
    /// canonical form (see the module docs), else the upstream's binding,
    /// else [`Unbound`].
    pub fn select(&self, upstream: &str, path: &str) -> Result<Selected<'_>, Unbound> {
        let unbound = || Unbound {
            upstream: upstream.to_owned(),
            path: path.to_owned(),
        };
        let entry = self.upstreams.get(upstream).ok_or_else(unbound)?;
        if let Some((route, binding)) = entry.routes.get_key_value(&canonical_path(path)) {
            return Ok(Selected {
                route: Some(route),
                binding,
            });
        }
        entry
            .binding
            .as_ref()
            .map(|binding| Selected {
                route: None,
                binding,
            })
            .ok_or_else(unbound)
    }
}

/// `path` in the form route paths are compared in: runs of `/` collapsed,
/// a trailing `/` dropped (except for `/` itself), percent-encoded
/// unreserved characters decoded and remaining escapes upper-cased.
fn canonical_path(path: &str) -> String {
    fn hex(b: Option<&u8>) -> Option<u8> {
        (*b? as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = path.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if let (Some(hi), Some(lo)) = (hex(bytes.get(i + 1)), hex(bytes.get(i + 2))) {
                let v = hi * 16 + lo;
                if v.is_ascii_alphanumeric() || matches!(v, b'-' | b'.' | b'_' | b'~') {
                    out.push(v);
                } else {
                    out.extend_from_slice(format!("%{v:02X}").as_bytes());
                }
                i += 3;
                continue;
            }
        }
        if !(b == b'/' && out.last() == Some(&b'/')) {
            out.push(b);
        }
        i += 1;
    }
    while out.len() > 1 && out.last() == Some(&b'/') {
        out.pop();
    }
    // Only ASCII was inserted and every other byte copied whole.
    String::from_utf8_lossy(&out).into_owned()
}

/// Why `path` cannot be a route path, or `None` when it can.
fn route_path_refusal(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return Some("a route path starts with '/'".into());
    }
    if let Some(c) = path.chars().find(|c| matches!(c, '?' | '#')) {
        return Some(format!(
            "a route path has no {c:?}: it is matched against the path alone"
        ));
    }
    let canonical = canonical_path(path);
    if canonical
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Some(
            "a route path has no '.' or '..' segment: such a request is never forwarded".into(),
        );
    }
    if canonical != path {
        return Some(format!(
            "write the route path in canonical form, {canonical:?}: it is compared that way"
        ));
    }
    None
}

/// Constructs a [`BindingTable`] one binding at a time; every error is a
/// startup error.
pub struct BindingTableBuilder<'a> {
    factory: &'a dyn ExtractorFactory,
    materials: &'a dyn MaterialResolver,
    custom: BTreeMap<String, Arc<dyn IdentityExtractor>>,
    /// Extractors already constructed, so bindings that resolve to the same
    /// spec share one (and one JWKS key set).
    built: Vec<(BindingSpec, Arc<dyn IdentityExtractor>)>,
    table: BindingTable,
}

impl BindingTableBuilder<'_> {
    /// Declare `upstream` with its resolved upstream-level binding (`None`:
    /// neither `[upstream.identity]` nor `[identity]` exists, so only its
    /// routes are bound).
    pub async fn upstream(
        &mut self,
        upstream: &str,
        binding: Option<&BindingSpec>,
    ) -> Result<(), StartupError> {
        let at = format!("upstream[{upstream}].identity");
        if self.table.upstreams.contains_key(upstream) {
            return Err(StartupError::Duplicate {
                at: format!("upstream[{upstream}]"),
            });
        }
        let binding = match binding {
            Some(spec) => Some(self.construct(&at, spec).await?),
            None => None,
        };
        self.table.upstreams.insert(
            upstream.to_owned(),
            UpstreamEntry {
                binding,
                routes: BTreeMap::new(),
            },
        );
        Ok(())
    }

    /// Declare a route of an already-declared upstream. `path` is one
    /// literal path in canonical form (see the module docs); anything else
    /// is refused.
    pub async fn route(
        &mut self,
        upstream: &str,
        path: &str,
        binding: &BindingSpec,
    ) -> Result<(), StartupError> {
        let at = format!("upstream[{upstream}].route[{path}].identity");
        if let Some(reason) = route_path_refusal(path) {
            return Err(StartupError::InvalidRoutePath {
                at: format!("upstream[{upstream}].route[{path}].path"),
                reason,
            });
        }
        match self.table.upstreams.get(upstream) {
            None => return Err(StartupError::UndeclaredUpstream { at }),
            Some(entry) if entry.routes.contains_key(path) => {
                return Err(StartupError::Duplicate {
                    at: format!("upstream[{upstream}].route[{path}]"),
                })
            }
            Some(_) => {}
        }
        let constructed = self.construct(&at, binding).await?;
        if let Some(entry) = self.table.upstreams.get_mut(upstream) {
            entry.routes.insert(path.to_owned(), constructed);
        }
        Ok(())
    }

    pub fn finish(self) -> BindingTable {
        self.table
    }

    async fn construct(&mut self, at: &str, spec: &BindingSpec) -> Result<Binding, StartupError> {
        let method = spec.method();
        if let Some((_, existing)) = self.built.iter().find(|(s, _)| s == spec) {
            return Ok(Binding {
                method,
                extractor: Some(existing.clone()),
            });
        }
        let material = |field: &str, reason: String| StartupError::Material {
            at: format!("{at}.{field}"),
            reason,
        };
        // The factory input, and the config field its material came from
        // (where a build failure points).
        let (input, field) = match spec {
            BindingSpec::None => {
                return Ok(Binding {
                    method,
                    extractor: None,
                })
            }
            BindingSpec::Custom(c) => {
                let inner =
                    self.custom
                        .get(&c.name)
                        .ok_or_else(|| StartupError::UnregisteredCustom {
                            at: format!("{at}.method"),
                            name: c.name.clone(),
                        })?;
                return Ok(Binding {
                    method,
                    extractor: Some(Arc::new(CustomBinding::new(
                        inner.clone(),
                        c.correlation.clone(),
                    ))),
                });
            }
            BindingSpec::Hmac(h) => {
                let secret = self
                    .materials
                    .secret(&h.secret_ref)
                    .map_err(|r| material("secret_ref", r))?;
                (
                    ExtractorInput::Hmac {
                        spec: h.clone(),
                        secret,
                    },
                    "secret_ref",
                )
            }
            BindingSpec::Jwt(j) => {
                let (key, field) = match &j.key {
                    JwtKey::Jwks { url } => (JwtKeyInput::Jwks(url.clone()), "jwks_url"),
                    JwtKey::PublicKeyFile(path) => (
                        JwtKeyInput::PublicKeyPem(
                            self.materials
                                .public_key_file(path)
                                .map_err(|r| material("public_key_file", r))?,
                        ),
                        "public_key_file",
                    ),
                };
                (
                    ExtractorInput::Jwt {
                        spec: j.clone(),
                        key,
                    },
                    field,
                )
            }
            BindingSpec::Opaque(o) => {
                let registry = self.materials.opaque_registry().ok_or_else(|| {
                    material(
                        "method",
                        "the opaque method needs the daemon's opaque-identity registry, and none is available"
                            .into(),
                    )
                })?;
                (
                    ExtractorInput::Opaque {
                        spec: o.clone(),
                        registry,
                    },
                    "method",
                )
            }
            BindingSpec::TrustedHeader(t) => (ExtractorInput::TrustedHeader(t.clone()), "source"),
        };
        let built = self
            .factory
            .build(input)
            .await
            .map_err(|source| StartupError::Build {
                at: format!("{at}.{field}"),
                source,
            })?;
        self.built.push((spec.clone(), built.clone()));
        Ok(Binding {
            method,
            extractor: Some(built),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::binding::{
        Correlation, CustomSpec, HmacSpec, JwtSpec, OpaqueSpec, Source, TrustedHeaderSpec,
    };
    use crate::contract::identity::PresentedAt;
    use crate::contract::upstream::UpstreamKind;
    use crate::identity::testkit::{
        clock_at, closed_port_url, head, mint_hmac, record, FakeRegistry, JwksServer, ED_PUBLIC,
        ED_X,
    };
    use crate::identity::IdentityFactory;
    use async_trait::async_trait;
    use http::HeaderName;
    use serde_json::json;
    use std::path::PathBuf;

    const NOW: i64 = 1_700_000_000;
    const SECRET: &[u8] = b"caller-identity-secret";

    /// The daemon's side: declared secrets, readable key files, a registry.
    #[derive(Default)]
    struct Materials {
        secrets: BTreeMap<String, Secret>,
        files: BTreeMap<PathBuf, Vec<u8>>,
        registry: Option<Arc<dyn OpaqueRegistry>>,
    }

    impl Materials {
        fn standard() -> Self {
            Materials {
                secrets: BTreeMap::from([("caller-identity".to_owned(), Secret::new(SECRET))]),
                files: BTreeMap::from([(
                    PathBuf::from("/etc/janus/idp.pem"),
                    ED_PUBLIC.as_bytes().to_vec(),
                )]),
                registry: Some(Arc::new(FakeRegistry::default())),
            }
        }
    }

    impl MaterialResolver for Materials {
        fn secret(&self, secret_ref: &str) -> Result<Secret, String> {
            self.secrets
                .get(secret_ref)
                .cloned()
                .ok_or_else(|| format!("secret {secret_ref:?} is not declared under [secrets]"))
        }

        fn public_key_file(&self, path: &Path) -> Result<Vec<u8>, String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| format!("cannot read {}", path.display()))
        }

        fn opaque_registry(&self) -> Option<Arc<dyn OpaqueRegistry>> {
            self.registry.clone()
        }
    }

    /// A custom build's introspection extractor.
    struct Introspect;

    #[async_trait]
    impl IdentityExtractor for Introspect {
        fn name(&self) -> &str {
            "introspect"
        }

        async fn extract(&self, req: &RequestHead) -> Result<ExtractedIdentity, ExtractionError> {
            let token = req
                .headers
                .get("x-ext-token")
                .and_then(|v| v.to_str().ok())
                .ok_or(ExtractionError::NoMaterial)?;
            let (identity, tenant) = token
                .split_once('@')
                .ok_or_else(|| ExtractionError::Invalid("introspection said no".into()))?;
            Ok(ExtractedIdentity {
                caller_identity: identity.to_owned(),
                claims: BTreeMap::from([("tenant".to_owned(), tenant.to_owned())]),
                correlation_id: Some("from-extractor".into()),
                presented_at: PresentedAt::Header(HeaderName::from_static("x-ext-token")),
            })
        }

        fn claimed_identity(&self, req: &RequestHead) -> Option<String> {
            req.headers
                .get("x-ext-token")
                .and_then(|v| v.to_str().ok())
                .map(|t| format!("claimed:{t}"))
        }
    }

    fn factory() -> IdentityFactory {
        IdentityFactory::new().with_clock(clock_at(NOW))
    }

    fn hmac_spec(header: &'static str) -> BindingSpec {
        BindingSpec::Hmac(HmacSpec {
            source: Source::Header {
                name: HeaderName::from_static(header),
                scheme: None,
            },
            secret_ref: "caller-identity".into(),
            namespace: "janus-data-plane".into(),
            identity_claim: "subject".into(),
            correlation: None,
        })
    }

    fn token(subject: &str) -> String {
        mint_hmac(
            SECRET,
            &json!({"ns": "janus-data-plane", "exp": NOW + 60, "subject": subject}),
        )
    }

    fn trusted() -> BindingSpec {
        BindingSpec::TrustedHeader(TrustedHeaderSpec {
            header: HeaderName::from_static("x-mesh-user"),
            scheme: None,
            correlation: None,
        })
    }

    fn custom(correlation: Option<Correlation>) -> BindingSpec {
        BindingSpec::Custom(CustomSpec {
            name: "introspect".into(),
            correlation,
        })
    }

    async fn build_one(
        materials: &Materials,
        custom_extractors: &[Arc<dyn IdentityExtractor>],
        spec: &BindingSpec,
    ) -> Result<BindingTable, StartupError> {
        let f = factory();
        let mut b = BindingTable::builder(&f, materials, custom_extractors)?;
        b.upstream(UpstreamKind::Anthropic.as_str(), Some(spec))
            .await?;
        Ok(b.finish())
    }

    #[tokio::test]
    async fn binding_route_overrides_upstream_and_upstream_covers_the_rest() {
        let f = factory();
        let m = Materials::standard();
        let mut b = BindingTable::builder(&f, &m, &[]).expect("builder");
        b.upstream(UpstreamKind::OpenAi.as_str(), Some(&hmac_spec("x-api-key")))
            .await
            .expect("upstream");
        b.route(
            UpstreamKind::OpenAi.as_str(),
            "/v1/models",
            &BindingSpec::None,
        )
        .await
        .expect("open route");
        b.route(UpstreamKind::OpenAi.as_str(), "/v1/mesh", &trusted())
            .await
            .expect("mesh route");
        let table = b.finish();

        let models = table
            .select(UpstreamKind::OpenAi.as_str(), "/v1/models")
            .expect("bound");
        assert_eq!(models.route, Some("/v1/models"));
        assert_eq!(models.binding.method(), &Method::None);

        let mesh = table
            .select(UpstreamKind::OpenAi.as_str(), "/v1/mesh")
            .expect("bound");
        assert_eq!(mesh.route, Some("/v1/mesh"));
        let id = mesh
            .binding
            .identify(&head("/v1/mesh", &[("x-mesh-user", "svc-a")]))
            .await
            .expect("verifies")
            .expect("identity");
        assert_eq!(id.caller_identity, "svc-a");
        // The route replaced the upstream binding: an hmac token is not looked at.
        let err = mesh
            .binding
            .identify(&head("/v1/mesh", &[("x-api-key", &token("s-1"))]))
            .await
            .expect_err("trusted_header needs its header");
        assert_eq!(err.error, ExtractionError::NoMaterial);

        // Respellings of a route path an upstream may treat as the same
        // resource take the route binding.
        for path in [
            "/v1/models/",
            "//v1/models",
            "/v1//models//",
            "/v1/%6dodels",
        ] {
            let sel = table
                .select(UpstreamKind::OpenAi.as_str(), path)
                .expect("bound");
            assert_eq!(sel.route, Some("/v1/models"), "{path}");
        }
        // Everything else under the upstream takes the upstream binding,
        // including near-misses of a route path.
        for path in [
            "/v1/chat/completions",
            "/v1/Models",
            "/v1/models/gpt",
            "/v1/models%2F",
        ] {
            let sel = table
                .select(UpstreamKind::OpenAi.as_str(), path)
                .expect("bound");
            assert_eq!(sel.route, None, "{path}");
            assert_eq!(sel.binding.method(), &Method::Hmac, "{path}");
        }
        let chat = table
            .select(UpstreamKind::OpenAi.as_str(), "/v1/chat/completions")
            .expect("bound");
        let id = chat
            .binding
            .identify(&head(
                "/v1/chat/completions",
                &[("x-api-key", &token("s-1"))],
            ))
            .await
            .expect("verifies")
            .expect("identity");
        assert_eq!(id.caller_identity, "s-1");
        assert_eq!(
            id.presented_at,
            PresentedAt::Header(HeaderName::from_static("x-api-key"))
        );
    }

    #[tokio::test]
    async fn a_route_stricter_than_its_upstream_cannot_be_bypassed_by_respelling() {
        let f = factory();
        let m = Materials::standard();
        let mut b = BindingTable::builder(&f, &m, &[]).expect("builder");
        b.upstream(UpstreamKind::Anthropic.as_str(), Some(&BindingSpec::None))
            .await
            .expect("open upstream");
        b.route(
            UpstreamKind::Anthropic.as_str(),
            "/v1/messages",
            &hmac_spec("x-api-key"),
        )
        .await
        .expect("guarded route");
        let table = b.finish();
        for path in [
            "/v1/messages",
            "/v1/messages/",
            "//v1/messages",
            "/v1//messages",
            "/v1/%6Dessages",
            "/%76%31/messages/",
        ] {
            let sel = table
                .select(UpstreamKind::Anthropic.as_str(), path)
                .expect("bound");
            assert_eq!(sel.binding.method(), &Method::Hmac, "{path}");
            assert_eq!(sel.route, Some("/v1/messages"), "{path}");
        }
        assert_eq!(
            table
                .select(UpstreamKind::Anthropic.as_str(), "/v1/models")
                .expect("bound")
                .binding
                .method(),
            &Method::None
        );
    }

    #[tokio::test]
    async fn startup_validation_rejects_route_paths_that_cannot_match_as_written() {
        let f = factory();
        let m = Materials::standard();
        let mut b = BindingTable::builder(&f, &m, &[]).expect("builder");
        b.upstream(UpstreamKind::OpenAi.as_str(), None)
            .await
            .expect("upstream");
        for bad in [
            "",
            "v1/models",
            "/v1/models?x=1",
            "/v1/models#a",
            "/v1/models/",
            "//v1/models",
            "/v1/%6Dodels",
            "/v1/%2fmodels",
            "/v1/../models",
            "/v1/%2e/models",
        ] {
            let err = b
                .route(UpstreamKind::OpenAi.as_str(), bad, &BindingSpec::None)
                .await
                .expect_err(bad);
            assert!(
                matches!(err, StartupError::InvalidRoutePath { .. }),
                "{bad}: {err:?}"
            );
            assert_eq!(err.at(), format!("upstream[openai].route[{bad}].path"));
        }
        for good in ["/", "/v1/models", "/v1/a%2Fb", "/v1/~x"] {
            b.route(UpstreamKind::OpenAi.as_str(), good, &BindingSpec::None)
                .await
                .expect(good);
        }
        let table = b.finish();
        assert_eq!(
            table
                .select(UpstreamKind::OpenAi.as_str(), "/v1/a%2fb")
                .expect("bound")
                .route,
            Some("/v1/a%2Fb")
        );
        assert_eq!(
            table
                .select(UpstreamKind::OpenAi.as_str(), "/v1/%7Ex")
                .expect("bound")
                .route,
            Some("/v1/~x")
        );
    }

    #[tokio::test]
    async fn hmac_bindings_with_different_namespaces_or_secrets_do_not_share() {
        let spec = |namespace: &str, secret_ref: &str| {
            BindingSpec::Hmac(HmacSpec {
                source: Source::Header {
                    name: HeaderName::from_static("x-api-key"),
                    scheme: None,
                },
                secret_ref: secret_ref.into(),
                namespace: namespace.into(),
                identity_claim: "subject".into(),
                correlation: None,
            })
        };
        let m = Materials {
            secrets: BTreeMap::from([
                ("caller-identity".to_owned(), Secret::new(SECRET)),
                ("other".to_owned(), Secret::new(b"another-secret".to_vec())),
            ]),
            ..Materials::standard()
        };
        let f = factory();
        let mut b = BindingTable::builder(&f, &m, &[]).expect("builder");
        b.upstream(
            UpstreamKind::Anthropic.as_str(),
            Some(&spec("janus-data-plane", "caller-identity")),
        )
        .await
        .expect("a");
        b.upstream(
            UpstreamKind::OpenAi.as_str(),
            Some(&spec("host-rpc", "caller-identity")),
        )
        .await
        .expect("b");
        b.upstream(
            UpstreamKind::OpenRouter.as_str(),
            Some(&spec("janus-data-plane", "other")),
        )
        .await
        .expect("c");
        let table = b.finish();
        let req = head("/x", &[("x-api-key", &token("s-1"))]);
        let id = |up: UpstreamKind| {
            let binding = table
                .select(up.as_str(), "/x")
                .expect("bound")
                .binding
                .clone();
            let req = req.clone();
            async move { binding.identify(&req).await }
        };
        assert!(id(UpstreamKind::Anthropic).await.is_ok());
        assert!(matches!(
            id(UpstreamKind::OpenAi).await,
            Err(Rejection {
                error: ExtractionError::WrongNamespace(_),
                ..
            })
        ));
        assert!(matches!(
            id(UpstreamKind::OpenRouter).await,
            Err(Rejection {
                error: ExtractionError::Invalid(_),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn none_method_yields_no_identity_even_when_material_is_present() {
        let m = Materials::standard();
        let table = build_one(&m, &[], &BindingSpec::None)
            .await
            .expect("builds");
        let sel = table
            .select(UpstreamKind::Anthropic.as_str(), "/v1/models")
            .expect("bound");
        assert_eq!(sel.binding.method().name(), "none");
        assert_eq!(sel.route, None);
        for req in [
            head("/v1/models", &[]),
            head(
                "/v1/models",
                &[("authorization", &format!("Bearer {}", token("s-1")))],
            ),
        ] {
            assert_eq!(sel.binding.identify(&req).await, Ok(None));
        }
    }

    #[tokio::test]
    async fn unbound_requests_are_rejected_not_waved_through() {
        let f = factory();
        let m = Materials::standard();
        let mut b = BindingTable::builder(&f, &m, &[]).expect("builder");
        // Neither [upstream.identity] nor [identity]: only the route is bound.
        b.upstream(UpstreamKind::Anthropic.as_str(), None)
            .await
            .expect("upstream");
        b.route(
            UpstreamKind::Anthropic.as_str(),
            "/v1/models",
            &BindingSpec::None,
        )
        .await
        .expect("route");
        let table = b.finish();
        assert!(table
            .select(UpstreamKind::Anthropic.as_str(), "/v1/models")
            .is_ok());
        assert_eq!(
            table
                .select(UpstreamKind::Anthropic.as_str(), "/v1/messages")
                .map(|s| s.route),
            Err(Unbound {
                upstream: "anthropic".into(),
                path: "/v1/messages".into()
            })
        );
        assert!(table
            .select(UpstreamKind::OpenRouter.as_str(), "/v1/models")
            .is_err());
        assert!(BindingTable::default()
            .select(UpstreamKind::OpenAi.as_str(), "/")
            .is_err());
    }

    #[tokio::test]
    async fn rejection_carries_the_unverified_claimed_identity() {
        let m = Materials::standard();
        let table = build_one(&m, &[], &hmac_spec("x-api-key"))
            .await
            .expect("builds");
        let sel = table
            .select(UpstreamKind::Anthropic.as_str(), "/v1/messages")
            .expect("bound");
        let forged = mint_hmac(
            b"not-the-secret",
            &json!({"ns": "janus-data-plane", "exp": NOW + 60, "subject": "victim"}),
        );
        let rej = sel
            .binding
            .identify(&head("/v1/messages", &[("x-api-key", &forged)]))
            .await
            .expect_err("forged");
        assert!(matches!(rej.error, ExtractionError::Invalid(_)));
        assert_eq!(rej.error.kind().as_str(), "invalid");
        assert_eq!(rej.claimed_identity.as_deref(), Some("victim"));
        let none = sel
            .binding
            .identify(&head("/v1/messages", &[]))
            .await
            .expect_err("nothing presented");
        assert_eq!(none.error, ExtractionError::NoMaterial);
        assert_eq!(none.claimed_identity, None);
    }

    #[tokio::test]
    async fn opaque_binding_probes_the_daemon_registry() {
        let reg = Arc::new(FakeRegistry::default());
        reg.insert("tok-1", record("sandbox-1", None, None));
        reg.insert(
            "tok-revoked",
            record(
                "sandbox-2",
                None,
                Some(crate::identity::testkit::at(NOW - 5)),
            ),
        );
        let m = Materials {
            registry: Some(reg),
            ..Materials::standard()
        };
        let spec = BindingSpec::Opaque(OpaqueSpec {
            source: Source::Header {
                name: HeaderName::from_static("authorization"),
                scheme: Some("Bearer ".into()),
            },
            correlation: None,
        });
        let table = build_one(&m, &[], &spec).await.expect("builds");
        let b = table
            .select(UpstreamKind::Anthropic.as_str(), "/x")
            .expect("bound")
            .binding;
        let id = b
            .identify(&head("/x", &[("authorization", "Bearer tok-1")]))
            .await
            .expect("live")
            .expect("identity");
        assert_eq!(id.caller_identity, "sandbox-1");
        let rej = b
            .identify(&head("/x", &[("authorization", "Bearer tok-revoked")]))
            .await
            .expect_err("revoked");
        assert_eq!(rej.error, ExtractionError::Revoked);
    }

    #[tokio::test]
    async fn custom_extractor_is_registered_by_name_and_correlation_applies() {
        let m = Materials::standard();
        let registered: Vec<Arc<dyn IdentityExtractor>> = vec![Arc::new(Introspect)];
        let req = head(
            "/x",
            &[("x-ext-token", "agent-5@acme"), ("x-request-id", "trace-9")],
        );

        let plain = build_one(&m, &registered, &custom(None))
            .await
            .expect("builds");
        let b = plain
            .select(UpstreamKind::Anthropic.as_str(), "/x")
            .expect("bound")
            .binding;
        assert_eq!(b.method().name(), "custom:introspect");
        let id = b.identify(&req).await.expect("ok").expect("identity");
        assert_eq!(id.caller_identity, "agent-5");
        assert_eq!(id.correlation_id.as_deref(), Some("from-extractor"));

        let by_header = Correlation::Header(HeaderName::from_static("x-request-id"));
        let t = build_one(&m, &registered, &custom(Some(by_header)))
            .await
            .expect("builds");
        let id = t
            .select(UpstreamKind::Anthropic.as_str(), "/x")
            .expect("bound")
            .binding
            .identify(&req)
            .await
            .expect("ok")
            .expect("identity");
        assert_eq!(id.correlation_id.as_deref(), Some("trace-9"));

        let by_claim = Correlation::Claim("tenant".into());
        let t = build_one(&m, &registered, &custom(Some(by_claim)))
            .await
            .expect("builds");
        let sel = t
            .select(UpstreamKind::Anthropic.as_str(), "/x")
            .expect("bound");
        let id = sel
            .binding
            .identify(&req)
            .await
            .expect("ok")
            .expect("identity");
        assert_eq!(id.correlation_id.as_deref(), Some("acme"));

        // Its rejections and log-only decode pass through; an empty identity
        // is refused.
        let rej = sel
            .binding
            .identify(&head("/x", &[("x-ext-token", "@acme")]))
            .await
            .expect_err("empty identity");
        assert!(matches!(rej.error, ExtractionError::Invalid(_)));
        assert_eq!(rej.claimed_identity.as_deref(), Some("claimed:@acme"));
        let rej = sel
            .binding
            .identify(&head("/x", &[("x-ext-token", "nope")]))
            .await
            .expect_err("introspection refused");
        assert_eq!(
            rej.error,
            ExtractionError::Invalid("introspection said no".into())
        );
    }

    #[tokio::test]
    async fn startup_validation_rejects_missing_or_unusable_material() {
        // Undeclared secret.
        let m = Materials {
            secrets: BTreeMap::new(),
            ..Materials::standard()
        };
        let err = build_one(&m, &[], &hmac_spec("x-api-key"))
            .await
            .expect_err("no secret");
        assert!(matches!(err, StartupError::Material { .. }), "{err:?}");
        assert_eq!(err.at(), "upstream[anthropic].identity.secret_ref");

        // Empty secret.
        let m = Materials {
            secrets: BTreeMap::from([("caller-identity".to_owned(), Secret::new(Vec::new()))]),
            ..Materials::standard()
        };
        let err = build_one(&m, &[], &hmac_spec("x-api-key"))
            .await
            .expect_err("empty");
        assert_eq!(
            err,
            StartupError::Build {
                at: "upstream[anthropic].identity.secret_ref".into(),
                source: ExtractorBuildError::EmptySecret
            }
        );

        // Key file unreadable, then unusable.
        let jwt = |key: JwtKey| {
            BindingSpec::Jwt(JwtSpec {
                source: Source::Header {
                    name: HeaderName::from_static("authorization"),
                    scheme: Some("Bearer ".into()),
                },
                key,
                issuer: "https://idp".into(),
                audience: "janus".into(),
                identity_claim: "sub".into(),
                correlation: None,
            })
        };
        let missing = jwt(JwtKey::PublicKeyFile("/nowhere.pem".into()));
        let err = build_one(&Materials::standard(), &[], &missing)
            .await
            .expect_err("no file");
        assert_eq!(err.at(), "upstream[anthropic].identity.public_key_file");
        assert!(matches!(err, StartupError::Material { .. }));
        let garbage = Materials {
            files: BTreeMap::from([(PathBuf::from("/bad.pem"), b"garbage".to_vec())]),
            ..Materials::standard()
        };
        let err = build_one(
            &garbage,
            &[],
            &jwt(JwtKey::PublicKeyFile("/bad.pem".into())),
        )
        .await
        .expect_err("bad key");
        assert!(matches!(
            err,
            StartupError::Build {
                source: ExtractorBuildError::InvalidKey(_),
                ..
            }
        ));

        // Opaque with no registry.
        let m = Materials {
            registry: None,
            ..Materials::standard()
        };
        let opaque = BindingSpec::Opaque(OpaqueSpec {
            source: Source::Query { name: "k".into() },
            correlation: None,
        });
        let err = build_one(&m, &[], &opaque).await.expect_err("no registry");
        assert_eq!(err.at(), "upstream[anthropic].identity.method");
    }

    #[tokio::test]
    async fn startup_validation_rejects_an_unreachable_jwks_endpoint() {
        let url = closed_port_url().await;
        let spec = BindingSpec::Jwt(JwtSpec {
            source: Source::Header {
                name: HeaderName::from_static("authorization"),
                scheme: Some("Bearer ".into()),
            },
            key: JwtKey::Jwks { url },
            issuer: "https://idp".into(),
            audience: "janus".into(),
            identity_claim: "sub".into(),
            correlation: None,
        });
        let err = build_one(&Materials::standard(), &[], &spec)
            .await
            .expect_err("unreachable");
        assert_eq!(err.at(), "upstream[anthropic].identity.jwks_url");
        assert!(matches!(
            err,
            StartupError::Build {
                source: ExtractorBuildError::JwksUnreachable { .. },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn startup_validation_rejects_unregistered_or_duplicate_custom_extractors() {
        let m = Materials::standard();
        let err = build_one(&m, &[], &custom(None))
            .await
            .expect_err("unregistered");
        assert_eq!(
            err,
            StartupError::UnregisteredCustom {
                at: "upstream[anthropic].identity.method".into(),
                name: "introspect".into()
            }
        );
        let twice: Vec<Arc<dyn IdentityExtractor>> =
            vec![Arc::new(Introspect), Arc::new(Introspect)];
        let f = factory();
        assert_eq!(
            BindingTable::builder(&f, &m, &twice).map(|_| ()),
            Err(StartupError::DuplicateCustom {
                name: "introspect".into()
            })
        );
    }

    #[tokio::test]
    async fn startup_validation_rejects_duplicate_and_orphan_declarations() {
        let f = factory();
        let m = Materials::standard();
        let mut b = BindingTable::builder(&f, &m, &[]).expect("builder");
        let err = b
            .route(
                UpstreamKind::OpenAi.as_str(),
                "/v1/models",
                &BindingSpec::None,
            )
            .await
            .expect_err("route before upstream");
        assert_eq!(err.at(), "upstream[openai].route[/v1/models].identity");
        b.upstream(UpstreamKind::OpenAi.as_str(), None)
            .await
            .expect("upstream");
        assert_eq!(
            b.upstream(UpstreamKind::OpenAi.as_str(), None).await,
            Err(StartupError::Duplicate {
                at: "upstream[openai]".into()
            })
        );
        b.route(
            UpstreamKind::OpenAi.as_str(),
            "/v1/models",
            &BindingSpec::None,
        )
        .await
        .expect("route");
        assert!(matches!(
            b.route(UpstreamKind::OpenAi.as_str(), "/v1/models", &trusted())
                .await,
            Err(StartupError::Duplicate { .. })
        ));
        // Route-level failures point at the route.
        let err = b
            .route(UpstreamKind::OpenAi.as_str(), "/v1/x", &custom(None))
            .await
            .expect_err("unregistered");
        assert_eq!(err.at(), "upstream[openai].route[/v1/x].identity.method");
    }

    #[tokio::test]
    async fn identical_bindings_share_one_extractor_and_one_jwks_fetch() {
        let server = JwksServer::start(json!({"keys": [
            {"kty": "OKP", "crv": "Ed25519", "kid": "k1", "x": ED_X}
        ]}))
        .await;
        let spec = BindingSpec::Jwt(JwtSpec {
            source: Source::Header {
                name: HeaderName::from_static("authorization"),
                scheme: Some("Bearer ".into()),
            },
            key: JwtKey::Jwks { url: server.url() },
            issuer: "https://idp".into(),
            audience: "janus".into(),
            identity_claim: "sub".into(),
            correlation: None,
        });
        let f = factory();
        let m = Materials::standard();
        let mut b = BindingTable::builder(&f, &m, &[]).expect("builder");
        for up in UpstreamKind::ALL {
            b.upstream(up.as_str(), Some(&spec))
                .await
                .expect("upstream");
        }
        b.route(UpstreamKind::OpenAi.as_str(), "/v1/responses", &spec)
            .await
            .expect("route");
        let table = b.finish();
        assert_eq!(server.fetches(), 1);
        let a = table
            .select(UpstreamKind::Anthropic.as_str(), "/x")
            .expect("bound")
            .binding;
        let r = table
            .select(UpstreamKind::OpenAi.as_str(), "/v1/responses")
            .expect("bound")
            .binding;
        assert!(Arc::ptr_eq(
            a.extractor.as_ref().expect("extractor"),
            r.extractor.as_ref().expect("extractor")
        ));
    }
}
