//! The `jwt` method: a JWT from an external issuer, verified against a
//! configured public key or a JWKS endpoint, with issuer and audience
//! checked (`docs/caller-identity#methods`).
//!
//! The accepted algorithms come from the key, never from the token: a PEM
//! key admits its own family (RSA, EC or EdDSA), a JWK its declared `alg` or
//! else its family. HMAC algorithms and symmetric (`oct`) JWKs are never
//! accepted, so a token cannot have its public key used as an HMAC secret.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, PublicKeyUse};
use jsonwebtoken::{decode, decode_header, Algorithm, AlgorithmFamily, DecodingKey, Validation};
use serde_json::{Map, Value};
use tokio::sync::{Mutex, RwLock};
use url::Url;

use crate::contract::binding::JwtSpec;
use crate::contract::identity::{
    ExtractedIdentity, ExtractionError, ExtractorBuildError, IdentityExtractor, JwtKeyInput,
    RequestHead,
};

use super::claims::SignedClaims;
use super::factory::{Clock, JwksRefresh, JWT_NBF_SKEW};

/// Registered claims that are the method's own and not passed on.
const MACHINERY: &[&str] = &["iss", "aud", "exp", "nbf", "iat"];
/// Largest JWKS document accepted.
const MAX_JWKS_BYTES: usize = 1024 * 1024;

/// One verification key and the algorithms it admits.
#[derive(Clone)]
struct VerifyKey {
    kid: Option<String>,
    key: DecodingKey,
    algorithms: Vec<Algorithm>,
}

impl VerifyKey {
    fn admits(&self, alg: Algorithm, kid: Option<&str>) -> bool {
        self.algorithms.contains(&alg)
            && match (kid, &self.kid) {
                (Some(want), Some(have)) => want == have,
                (Some(_), None) => false,
                (None, _) => true,
            }
    }
}

/// A key set fetched from a JWKS endpoint and kept current.
struct Jwks {
    url: Url,
    client: reqwest::Client,
    refresh: JwksRefresh,
    state: RwLock<JwksState>,
    /// Serializes refetches so concurrent misses cause one fetch.
    refetch: Mutex<()>,
}

struct JwksState {
    keys: Arc<Vec<VerifyKey>>,
    fetched_at: Instant,
    attempted_at: Instant,
}

enum Keys {
    /// `public_key_file`.
    Static(Arc<Vec<VerifyKey>>),
    Jwks(Box<Jwks>),
}

pub(crate) struct JwtExtractor {
    issuer: String,
    audience: String,
    claims: SignedClaims,
    keys: Keys,
    clock: Clock,
}

impl JwtExtractor {
    pub(crate) async fn new(
        spec: JwtSpec,
        key: JwtKeyInput,
        clock: Clock,
        refresh: JwksRefresh,
    ) -> Result<Self, ExtractorBuildError> {
        let keys = match key {
            JwtKeyInput::PublicKeyPem(pem) => Keys::Static(Arc::new(vec![pem_key(&pem)?])),
            JwtKeyInput::Jwks(url) => {
                let client = reqwest::Client::builder()
                    .timeout(refresh.timeout)
                    .connect_timeout(refresh.timeout)
                    .build()
                    .map_err(|e| ExtractorBuildError::JwksUnreachable {
                        url: url.to_string(),
                        reason: format!("http client: {e}"),
                    })?;
                let keys = fetch_jwks(&client, &url).await?;
                let now = Instant::now();
                Keys::Jwks(Box::new(Jwks {
                    url,
                    client,
                    refresh,
                    state: RwLock::new(JwksState {
                        keys: Arc::new(keys),
                        fetched_at: now,
                        attempted_at: now,
                    }),
                    refetch: Mutex::new(()),
                }))
            }
        };
        Ok(JwtExtractor {
            issuer: spec.issuer,
            audience: spec.audience,
            claims: SignedClaims {
                source: spec.source,
                identity_claim: spec.identity_claim,
                machinery: MACHINERY,
                correlation: spec.correlation,
            },
            keys,
            clock,
        })
    }

    fn validation(&self, alg: Algorithm) -> Validation {
        let mut v = Validation::new(alg);
        v.set_issuer(&[&self.issuer]);
        v.set_audience(&[&self.audience]);
        v.set_required_spec_claims(&["exp", "iss", "aud"]);
        // Lifetimes are checked against `self.clock` below.
        v.validate_exp = false;
        v.validate_nbf = false;
        v.leeway = 0;
        v
    }

    async fn verify(&self, token: &str) -> Result<Map<String, Value>, ExtractionError> {
        let header = decode_header(token)
            .map_err(|e| ExtractionError::Invalid(format!("not a JWT: {e}")))?;
        if header.alg.family() == AlgorithmFamily::Hmac {
            return Err(ExtractionError::Invalid(format!(
                "algorithm {:?} is not accepted: only asymmetric signatures verify",
                header.alg
            )));
        }
        let kid = header.kid.as_deref();
        let candidates = match &self.keys {
            Keys::Static(keys) => matching(keys, header.alg, kid),
            Keys::Jwks(jwks) => jwks.candidates(header.alg, kid).await,
        };
        if candidates.is_empty() {
            return Err(ExtractionError::Invalid(match kid {
                Some(kid) => format!("no {:?} verification key with kid {kid:?}", header.alg),
                None => format!("no {:?} verification key", header.alg),
            }));
        }
        let validation = self.validation(header.alg);
        let mut last = ExtractionError::Invalid("signature does not verify".into());
        for key in candidates {
            use jsonwebtoken::errors::ErrorKind as K;
            let err = match decode::<Map<String, Value>>(token, &key.key, &validation) {
                Ok(data) => return self.check_lifetime(data.claims),
                Err(e) => e,
            };
            match err.kind() {
                // This key does not verify the token; another candidate may.
                K::InvalidSignature
                | K::InvalidAlgorithm
                | K::InvalidEcdsaKey
                | K::InvalidEddsaKey
                | K::InvalidRsaKey(_)
                | K::InvalidKeyFormat => {
                    last = ExtractionError::Invalid(format!("token does not verify: {err}"));
                }
                // Issuer and audience are checked only after the signature
                // verified: genuine material for another binding.
                K::InvalidIssuer => {
                    return Err(ExtractionError::WrongNamespace(format!(
                        "token issuer is not {:?}",
                        self.issuer
                    )))
                }
                K::InvalidAudience => {
                    return Err(ExtractionError::WrongNamespace(format!(
                        "token audience does not include {:?}",
                        self.audience
                    )))
                }
                _ => {
                    return Err(ExtractionError::Invalid(format!(
                        "token does not verify: {err}"
                    )))
                }
            }
        }
        Err(last)
    }

    fn check_lifetime(
        &self,
        claims: Map<String, Value>,
    ) -> Result<Map<String, Value>, ExtractionError> {
        let now = self.clock.now().timestamp() as f64;
        let exp = claims
            .get("exp")
            .and_then(Value::as_f64)
            .ok_or_else(|| ExtractionError::Invalid("`exp` is not a NumericDate".into()))?;
        if now >= exp {
            return Err(ExtractionError::Expired);
        }
        match claims.get("nbf") {
            None => {}
            Some(nbf) => {
                let nbf = nbf
                    .as_f64()
                    .ok_or_else(|| ExtractionError::Invalid("`nbf` is not a NumericDate".into()))?;
                if nbf > now + JWT_NBF_SKEW.as_secs_f64() {
                    return Err(ExtractionError::Invalid(
                        "token is not valid yet (nbf)".into(),
                    ));
                }
            }
        }
        Ok(claims)
    }
}

fn matching(keys: &[VerifyKey], alg: Algorithm, kid: Option<&str>) -> Vec<VerifyKey> {
    keys.iter()
        .filter(|k| k.admits(alg, kid))
        .cloned()
        .collect()
}

impl Jwks {
    /// Keys admitting `alg`/`kid`, refetching first when the set is stale
    /// or has no such key, subject to `min_interval`. A failed refetch keeps
    /// the keys already held.
    async fn candidates(&self, alg: Algorithm, kid: Option<&str>) -> Vec<VerifyKey> {
        let (found, stale) = {
            let state = self.state.read().await;
            (
                matching(&state.keys, alg, kid),
                state.fetched_at.elapsed() >= self.refresh.max_age,
            )
        };
        if !found.is_empty() && !stale {
            return found;
        }
        self.refetch_if_due().await;
        // Re-read either way: a refetch by a concurrent caller may have
        // landed while this one waited for the lock.
        matching(&self.state.read().await.keys, alg, kid)
    }

    async fn refetch_if_due(&self) {
        let _one_at_a_time = self.refetch.lock().await;
        let since_attempt = self.state.read().await.attempted_at.elapsed();
        if since_attempt < self.refresh.min_interval {
            return;
        }
        let result = fetch_jwks(&self.client, &self.url).await;
        let mut state = self.state.write().await;
        let now = Instant::now();
        state.attempted_at = now;
        match result {
            Ok(keys) => {
                state.keys = Arc::new(keys);
                state.fetched_at = now;
            }
            Err(e) => {
                tracing::warn!(url = %self.url, error = %e, "JWKS refetch failed; keeping the previous key set");
            }
        }
    }
}

/// Fetch and parse a JWKS document. Transport failure or a non-success
/// status: unreachable. A document with no usable signature key: invalid.
async fn fetch_jwks(
    client: &reqwest::Client,
    url: &Url,
) -> Result<Vec<VerifyKey>, ExtractorBuildError> {
    let unreachable = |reason: String| ExtractorBuildError::JwksUnreachable {
        url: url.to_string(),
        reason,
    };
    let mut resp = client
        .get(url.clone())
        .header(http::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| unreachable(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(unreachable(format!("HTTP {}", resp.status())));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| unreachable(e.to_string()))? {
        if body.len() + chunk.len() > MAX_JWKS_BYTES {
            return Err(ExtractorBuildError::InvalidKey(format!(
                "JWKS document at {url} exceeds {MAX_JWKS_BYTES} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    parse_jwks(&body)
        .map_err(|reason| ExtractorBuildError::InvalidKey(format!("JWKS at {url}: {reason}")))
}

/// The usable signature-verification keys of a JWKS document. Keys of an
/// unsupported type, marked for encryption, symmetric, or declaring an
/// algorithm outside their family are skipped; none usable is an error.
fn parse_jwks(body: &[u8]) -> Result<Vec<VerifyKey>, String> {
    #[derive(serde::Deserialize)]
    struct Document {
        keys: Vec<Value>,
    }
    let doc: Document =
        serde_json::from_slice(body).map_err(|e| format!("not a JWKS document: {e}"))?;
    let keys: Vec<VerifyKey> = doc
        .keys
        .into_iter()
        .filter_map(|raw| serde_json::from_value::<Jwk>(raw).ok())
        .filter_map(|jwk| jwk_key(&jwk))
        .collect();
    if keys.is_empty() {
        return Err("no usable signature-verification key".into());
    }
    Ok(keys)
}

fn jwk_key(jwk: &Jwk) -> Option<VerifyKey> {
    if matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption)) {
        return None;
    }
    let family_algs: &[Algorithm] = match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => AlgorithmFamily::Rsa.algorithms(),
        AlgorithmParameters::EllipticCurve(ec) => match ec.curve {
            EllipticCurve::P256 => &[Algorithm::ES256],
            EllipticCurve::P384 => &[Algorithm::ES384],
            EllipticCurve::P521 | EllipticCurve::Ed25519 => return None,
        },
        AlgorithmParameters::OctetKeyPair(okp) => match okp.curve {
            EllipticCurve::Ed25519 => &[Algorithm::EdDSA],
            _ => return None,
        },
        AlgorithmParameters::OctetKey(_) => return None,
    };
    let algorithms = match jwk.common.key_algorithm {
        Some(declared) => {
            let alg = Algorithm::from_str(&declared.to_string()).ok()?;
            if !family_algs.contains(&alg) {
                return None;
            }
            vec![alg]
        }
        None => family_algs.to_vec(),
    };
    Some(VerifyKey {
        kid: jwk.common.key_id.clone(),
        key: DecodingKey::from_jwk(jwk).ok()?,
        algorithms,
    })
}

/// A PEM public key: RSA, EC or Ed25519.
fn pem_key(pem: &[u8]) -> Result<VerifyKey, ExtractorBuildError> {
    let (key, algorithms) = if let Ok(k) = DecodingKey::from_rsa_pem(pem) {
        (k, AlgorithmFamily::Rsa.algorithms().to_vec())
    } else if let Ok(k) = DecodingKey::from_ec_pem(pem) {
        (k, AlgorithmFamily::Ec.algorithms().to_vec())
    } else if let Ok(k) = DecodingKey::from_ed_pem(pem) {
        (k, vec![Algorithm::EdDSA])
    } else {
        return Err(ExtractorBuildError::InvalidKey(
            "public_key_file is not a PEM-encoded RSA, EC or Ed25519 public key".into(),
        ));
    };
    Ok(VerifyKey {
        kid: None,
        key,
        algorithms,
    })
}

#[async_trait]
impl IdentityExtractor for JwtExtractor {
    fn name(&self) -> &str {
        "jwt"
    }

    async fn extract(&self, req: &RequestHead) -> Result<ExtractedIdentity, ExtractionError> {
        let verified = self.verify(&self.claims.material(req)?).await?;
        self.claims.project(verified, req)
    }

    fn claimed_identity(&self, req: &RequestHead) -> Option<String> {
        let token = self.claims.material(req).ok()?;
        let data = jsonwebtoken::dangerous::insecure_decode::<Map<String, Value>>(&token).ok()?;
        self.claims.claimed(&data.claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::binding::{Correlation, JwtKey, Source};
    use crate::identity::testkit::{
        clock_at, head, sign_jwt, JwksServer, EC_PRIVATE, EC_PUBLIC, ED2_PRIVATE, ED_PRIVATE,
        ED_PUBLIC,
    };
    use http::HeaderName;
    use serde_json::json;
    use std::time::Duration;

    const NOW: i64 = 1_700_000_000;
    const ISS: &str = "https://idp.example";
    const AUD: &str = "janus";

    fn spec() -> JwtSpec {
        JwtSpec {
            source: Source::Header {
                name: HeaderName::from_static("authorization"),
                scheme: Some("Bearer ".into()),
            },
            key: JwtKey::PublicKeyFile("/etc/janus/idp.pem".into()),
            issuer: ISS.into(),
            audience: AUD.into(),
            identity_claim: "sub".into(),
            correlation: Some(Correlation::Claim("sid".into())),
        }
    }

    async fn pem_extractor(pem: &str) -> JwtExtractor {
        JwtExtractor::new(
            spec(),
            JwtKeyInput::PublicKeyPem(pem.as_bytes().to_vec()),
            clock_at(NOW),
            JwksRefresh::default(),
        )
        .await
        .expect("builds")
    }

    fn claims() -> Value {
        json!({"iss": ISS, "aud": AUD, "sub": "user-9", "exp": NOW + 300,
               "iat": NOW - 5, "sid": "sess-3", "tenant": "acme"})
    }

    fn bearer(token: &str) -> RequestHead {
        head("/v1/chat", &[("authorization", &format!("Bearer {token}"))])
    }

    #[tokio::test]
    async fn jwt_verifies_eddsa_and_es256_against_a_pem_key() {
        let ed = pem_extractor(ED_PUBLIC).await;
        let id = ed
            .extract(&bearer(&sign_jwt(
                Algorithm::EdDSA,
                ED_PRIVATE,
                None,
                &claims(),
            )))
            .await
            .expect("verifies");
        assert_eq!(id.caller_identity, "user-9");
        assert_eq!(id.correlation_id.as_deref(), Some("sess-3"));
        assert_eq!(
            id.claims.keys().map(String::as_str).collect::<Vec<_>>(),
            ["sid", "tenant"]
        );
        let ec = pem_extractor(EC_PUBLIC).await;
        assert!(ec
            .extract(&bearer(&sign_jwt(
                Algorithm::ES256,
                EC_PRIVATE,
                None,
                &claims()
            )))
            .await
            .is_ok());
        // An EC key does not verify an EdDSA token, and vice versa.
        assert!(matches!(
            ec.extract(&bearer(&sign_jwt(
                Algorithm::EdDSA,
                ED_PRIVATE,
                None,
                &claims()
            )))
            .await,
            Err(ExtractionError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn jwt_rejects_expired_tokens_and_future_nbf() {
        let ex = pem_extractor(ED_PUBLIC).await;
        for exp in [NOW, NOW - 1] {
            let mut c = claims();
            c["exp"] = json!(exp);
            assert_eq!(
                ex.extract(&bearer(&sign_jwt(Algorithm::EdDSA, ED_PRIVATE, None, &c)))
                    .await,
                Err(ExtractionError::Expired),
                "exp {exp}"
            );
        }
        let mut c = claims();
        c.as_object_mut().expect("object").remove("exp");
        assert!(matches!(
            ex.extract(&bearer(&sign_jwt(Algorithm::EdDSA, ED_PRIVATE, None, &c)))
                .await,
            Err(ExtractionError::Invalid(_))
        ));
        let mut c = claims();
        c["nbf"] = json!(NOW + 61);
        assert!(matches!(
            ex.extract(&bearer(&sign_jwt(Algorithm::EdDSA, ED_PRIVATE, None, &c)))
                .await,
            Err(ExtractionError::Invalid(_))
        ));
        c["nbf"] = json!(NOW + 60);
        assert!(ex
            .extract(&bearer(&sign_jwt(Algorithm::EdDSA, ED_PRIVATE, None, &c)))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn jwt_rejects_wrong_issuer_or_audience_as_wrong_namespace() {
        let ex = pem_extractor(ED_PUBLIC).await;
        let mut c = claims();
        c["iss"] = json!("https://other-idp.example");
        assert!(matches!(
            ex.extract(&bearer(&sign_jwt(Algorithm::EdDSA, ED_PRIVATE, None, &c)))
                .await,
            Err(ExtractionError::WrongNamespace(_))
        ));
        let mut c = claims();
        c["aud"] = json!(["billing", "reports"]);
        assert!(matches!(
            ex.extract(&bearer(&sign_jwt(Algorithm::EdDSA, ED_PRIVATE, None, &c)))
                .await,
            Err(ExtractionError::WrongNamespace(_))
        ));
        c["aud"] = json!(["billing", AUD]);
        assert!(ex
            .extract(&bearer(&sign_jwt(Algorithm::EdDSA, ED_PRIVATE, None, &c)))
            .await
            .is_ok());
        // Missing issuer is malformed, not "another issuer".
        let mut c = claims();
        c.as_object_mut().expect("object").remove("iss");
        assert!(matches!(
            ex.extract(&bearer(&sign_jwt(Algorithm::EdDSA, ED_PRIVATE, None, &c)))
                .await,
            Err(ExtractionError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn jwt_rejects_bad_signature_and_algorithm_confusion() {
        let ex = pem_extractor(ED_PUBLIC).await;
        // Signed by a different Ed25519 key.
        assert!(matches!(
            ex.extract(&bearer(&sign_jwt(
                Algorithm::EdDSA,
                ED2_PRIVATE,
                None,
                &claims()
            )))
            .await,
            Err(ExtractionError::Invalid(_))
        ));
        // HS256 keyed with the public key's PEM text: the classic confusion.
        let confused = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &claims(),
            &jsonwebtoken::EncodingKey::from_secret(ED_PUBLIC.as_bytes()),
        )
        .expect("hs256");
        assert!(matches!(
            ex.extract(&bearer(&confused)).await,
            Err(ExtractionError::Invalid(m)) if m.contains("not accepted")
        ));
        // Payload swapped under a genuine signature.
        let good = sign_jwt(Algorithm::EdDSA, ED_PRIVATE, None, &claims());
        let parts: Vec<&str> = good.split('.').collect();
        let mut forged = claims();
        forged["sub"] = json!("admin");
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            serde_json::to_vec(&forged).expect("json"),
        );
        let swapped = format!("{}.{payload}.{}", parts[0], parts[2]);
        assert!(matches!(
            ex.extract(&bearer(&swapped)).await,
            Err(ExtractionError::Invalid(_))
        ));
        // The log-only decode still reports what the forgery claimed.
        assert_eq!(
            ex.claimed_identity(&bearer(&swapped)).as_deref(),
            Some("admin")
        );
        assert_eq!(
            ex.extract(&head("/v1/chat", &[])).await,
            Err(ExtractionError::NoMaterial)
        );
    }

    #[tokio::test]
    async fn jwt_refuses_unusable_keys_at_construction() {
        for pem in [ED_PRIVATE, "not a key", ""] {
            assert!(
                matches!(
                    JwtExtractor::new(
                        spec(),
                        JwtKeyInput::PublicKeyPem(pem.as_bytes().to_vec()),
                        clock_at(NOW),
                        JwksRefresh::default()
                    )
                    .await,
                    Err(ExtractorBuildError::InvalidKey(_))
                ),
                "{pem}"
            );
        }
    }

    #[tokio::test]
    async fn jwt_jwks_selects_by_kid_and_refetches_on_rotation() {
        let server = JwksServer::start(json!({"keys": [
            {"kty": "OKP", "crv": "Ed25519", "kid": "k1", "x": crate::identity::testkit::ED_X},
            {"kty": "oct", "kid": "sym", "k": "c2VjcmV0"},
            {"kty": "XYZ", "kid": "weird"}
        ]}))
        .await;
        let refresh = JwksRefresh {
            max_age: Duration::from_secs(3600),
            min_interval: Duration::ZERO,
            timeout: Duration::from_secs(5),
        };
        let ex = JwtExtractor::new(
            spec(),
            JwtKeyInput::Jwks(server.url()),
            clock_at(NOW),
            refresh,
        )
        .await
        .expect("fetches");
        assert_eq!(server.fetches(), 1);
        let t1 = sign_jwt(Algorithm::EdDSA, ED_PRIVATE, Some("k1"), &claims());
        assert!(ex.extract(&bearer(&t1)).await.is_ok());
        assert_eq!(server.fetches(), 1, "a known kid needs no fetch");

        // The issuer rotates to k2; a k2 token triggers one refetch.
        server.set(json!({"keys": [
            {"kty": "OKP", "crv": "Ed25519", "kid": "k2", "x": crate::identity::testkit::ED2_X}
        ]}));
        let t2 = sign_jwt(Algorithm::EdDSA, ED2_PRIVATE, Some("k2"), &claims());
        assert!(ex.extract(&bearer(&t2)).await.is_ok());
        assert_eq!(server.fetches(), 2);
        // k1 is gone from the issuer's set now.
        assert!(matches!(
            ex.extract(&bearer(&t1)).await,
            Err(ExtractionError::Invalid(_))
        ));
        // A kid claiming the right key but signed by another is refused.
        let lying = sign_jwt(Algorithm::EdDSA, ED_PRIVATE, Some("k2"), &claims());
        assert!(matches!(
            ex.extract(&bearer(&lying)).await,
            Err(ExtractionError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn jwt_jwks_refetch_is_throttled_and_failure_keeps_keys() {
        let server = JwksServer::start(json!({"keys": [
            {"kty": "OKP", "crv": "Ed25519", "kid": "k1", "x": crate::identity::testkit::ED_X}
        ]}))
        .await;
        let ex = JwtExtractor::new(
            spec(),
            JwtKeyInput::Jwks(server.url()),
            clock_at(NOW),
            JwksRefresh {
                max_age: Duration::from_secs(3600),
                min_interval: Duration::from_secs(3600),
                timeout: Duration::from_secs(5),
            },
        )
        .await
        .expect("fetches");
        for _ in 0..5 {
            let unknown = sign_jwt(Algorithm::EdDSA, ED_PRIVATE, Some("nope"), &claims());
            assert!(ex.extract(&bearer(&unknown)).await.is_err());
        }
        assert_eq!(server.fetches(), 1, "unknown kids cannot force refetches");

        // Stale set, endpoint now failing: the held keys keep verifying.
        let stale = JwtExtractor::new(
            spec(),
            JwtKeyInput::Jwks(server.url()),
            clock_at(NOW),
            JwksRefresh {
                max_age: Duration::ZERO,
                min_interval: Duration::ZERO,
                timeout: Duration::from_secs(5),
            },
        )
        .await
        .expect("fetches");
        server.fail();
        let t1 = sign_jwt(Algorithm::EdDSA, ED_PRIVATE, Some("k1"), &claims());
        assert!(stale.extract(&bearer(&t1)).await.is_ok());
        assert_eq!(server.fetches(), 3, "stale set was refetched (and failed)");
    }

    #[tokio::test]
    async fn jwt_jwks_construction_fails_on_unreachable_or_keyless_endpoint() {
        let closed = crate::identity::testkit::closed_port_url().await;
        assert!(matches!(
            JwtExtractor::new(
                spec(),
                JwtKeyInput::Jwks(closed),
                clock_at(NOW),
                JwksRefresh::default()
            )
            .await,
            Err(ExtractorBuildError::JwksUnreachable { .. })
        ));
        let server = JwksServer::start(json!({"keys": [{"kty": "oct", "k": "c2VjcmV0"}]})).await;
        assert!(matches!(
            JwtExtractor::new(
                spec(),
                JwtKeyInput::Jwks(server.url()),
                clock_at(NOW),
                JwksRefresh::default()
            )
            .await,
            Err(ExtractorBuildError::InvalidKey(_))
        ));
        server.fail();
        assert!(matches!(
            JwtExtractor::new(
                spec(),
                JwtKeyInput::Jwks(server.url()),
                clock_at(NOW),
                JwksRefresh::default()
            )
            .await,
            Err(ExtractorBuildError::JwksUnreachable { .. })
        ));
    }

    #[test]
    fn jwk_declared_algorithm_must_fit_the_key_family() {
        let ed = |alg: &str| {
            serde_json::from_value::<Jwk>(json!({"kty": "OKP", "crv": "Ed25519",
                "x": crate::identity::testkit::ED_X, "alg": alg}))
            .expect("jwk")
        };
        assert_eq!(
            jwk_key(&ed("EdDSA")).map(|k| k.algorithms),
            Some(vec![Algorithm::EdDSA])
        );
        assert!(jwk_key(&ed("RS256")).is_none());
        let enc: Jwk = serde_json::from_value(json!({"kty": "OKP", "crv": "Ed25519",
            "x": crate::identity::testkit::ED_X, "use": "enc"}))
        .expect("jwk");
        assert!(jwk_key(&enc).is_none());
    }
}
