//! The `hmac` method: `<claims-hex>.<signature-hex>`, where `signature-hex`
//! is the lowercase-hex HMAC-SHA256, keyed with the shared secret, of the
//! ASCII bytes of `claims-hex`; the claims object carries `ns`, `exp` and
//! the binding's identity claim (`docs/caller-identity#methods`).

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use sha2::Sha256;

use crate::contract::binding::HmacSpec;
use crate::contract::credential::Secret;
use crate::contract::identity::{
    ExtractedIdentity, ExtractionError, ExtractorBuildError, IdentityExtractor, RequestHead,
};

use super::claims::SignedClaims;
use super::factory::Clock;

type HmacSha256 = Hmac<Sha256>;

/// Length of a hex-encoded HMAC-SHA256.
const SIGNATURE_HEX_LEN: usize = 64;
/// Claims that are the method's own and not passed on as `claims`.
const MACHINERY: &[&str] = &["ns", "exp"];

pub(crate) struct HmacExtractor {
    namespace: String,
    claims: SignedClaims,
    /// Keyed once at construction; cloned per verification.
    mac: HmacSha256,
    clock: Clock,
}

impl HmacExtractor {
    pub(crate) fn new(
        spec: HmacSpec,
        secret: &Secret,
        clock: Clock,
    ) -> Result<Self, ExtractorBuildError> {
        if secret.is_empty() {
            return Err(ExtractorBuildError::EmptySecret);
        }
        let mac = <HmacSha256 as Mac>::new_from_slice(secret.expose())
            .map_err(|e| ExtractorBuildError::InvalidKey(e.to_string()))?;
        Ok(HmacExtractor {
            namespace: spec.namespace,
            claims: SignedClaims {
                source: spec.source,
                identity_claim: spec.identity_claim,
                machinery: MACHINERY,
                correlation: spec.correlation,
            },
            mac,
            clock,
        })
    }

    /// Signature, then namespace, then expiry.
    fn verify(&self, token: &str) -> Result<Map<String, Value>, ExtractionError> {
        let (claims_hex, signature_hex) = split(token)?;
        let signature = decode_signature(signature_hex)?;
        // `verify_slice` compares in constant time.
        self.mac
            .clone()
            .chain_update(claims_hex.as_bytes())
            .verify_slice(&signature)
            .map_err(|_| ExtractionError::Invalid("signature does not verify".into()))?;
        let claims = decode_claims(claims_hex)?;
        match claims.get("ns") {
            Some(Value::String(ns)) if *ns == self.namespace => {}
            Some(Value::String(ns)) => {
                return Err(ExtractionError::WrongNamespace(format!(
                    "token namespace {ns:?}, binding expects {:?}",
                    self.namespace
                )))
            }
            _ => {
                return Err(ExtractionError::Invalid(
                    "claims carry no string `ns`".into(),
                ))
            }
        }
        let exp = claims
            .get("exp")
            .and_then(Value::as_i64)
            .ok_or_else(|| ExtractionError::Invalid("claims carry no integer `exp`".into()))?;
        if self.clock.now().timestamp() >= exp {
            return Err(ExtractionError::Expired);
        }
        Ok(claims)
    }
}

fn split(token: &str) -> Result<(&str, &str), ExtractionError> {
    match token.split_once('.') {
        Some((claims, sig)) if !claims.is_empty() && !sig.is_empty() => Ok((claims, sig)),
        _ => Err(ExtractionError::Invalid(
            "not an hmac token: expected <claims-hex>.<signature-hex>".into(),
        )),
    }
}

/// The signature as bytes. The wire form is exactly 64 lowercase hex digits.
fn decode_signature(hex_sig: &str) -> Result<[u8; 32], ExtractionError> {
    let canonical = hex_sig.len() == SIGNATURE_HEX_LEN
        && hex_sig
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let mut out = [0u8; 32];
    if !canonical || hex::decode_to_slice(hex_sig, &mut out).is_err() {
        return Err(ExtractionError::Invalid(
            "signature is not 64 lowercase hex digits".into(),
        ));
    }
    Ok(out)
}

fn decode_claims(claims_hex: &str) -> Result<Map<String, Value>, ExtractionError> {
    let bytes = hex::decode(claims_hex)
        .map_err(|_| ExtractionError::Invalid("claims are not hex".into()))?;
    match serde_json::from_slice(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(ExtractionError::Invalid(
            "claims are not a JSON object".into(),
        )),
    }
}

#[async_trait]
impl IdentityExtractor for HmacExtractor {
    fn name(&self) -> &str {
        "hmac"
    }

    async fn extract(&self, req: &RequestHead) -> Result<ExtractedIdentity, ExtractionError> {
        let verified = self.verify(&self.claims.material(req)?)?;
        self.claims.project(verified, req)
    }

    fn claimed_identity(&self, req: &RequestHead) -> Option<String> {
        let token = self.claims.material(req).ok()?;
        let (claims_hex, _) = split(&token).ok()?;
        self.claims.claimed(&decode_claims(claims_hex).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::binding::{Correlation, Source};
    use crate::contract::identity::PresentedAt;
    use crate::identity::testkit::{clock_at, head, mint_hmac};
    use http::HeaderName;
    use serde_json::json;

    const SECRET: &[u8] = b"shared-secret-of-the-host";
    const NOW: i64 = 1_700_000_000;

    fn spec() -> HmacSpec {
        HmacSpec {
            source: Source::Header {
                name: HeaderName::from_static("authorization"),
                scheme: Some("Bearer ".into()),
            },
            secret_ref: "caller-identity".into(),
            namespace: "janus-data-plane".into(),
            identity_claim: "subject".into(),
            correlation: Some(Correlation::Claim("session".into())),
        }
    }

    fn extractor() -> HmacExtractor {
        HmacExtractor::new(spec(), &Secret::new(SECRET), clock_at(NOW)).expect("builds")
    }

    fn bearer(token: &str) -> RequestHead {
        head(
            "/v1/messages",
            &[("authorization", &format!("Bearer {token}"))],
        )
    }

    fn good_claims() -> Value {
        json!({"ns": "janus-data-plane", "exp": NOW + 60, "subject": "sess-1",
               "session": "s-77", "agent": "claude"})
    }

    #[tokio::test]
    async fn hmac_accepts_a_genuine_token_and_projects_claims() {
        let token = mint_hmac(SECRET, &good_claims());
        let id = extractor()
            .extract(&bearer(&token))
            .await
            .expect("verifies");
        assert_eq!(id.caller_identity, "sess-1");
        assert_eq!(id.correlation_id.as_deref(), Some("s-77"));
        assert_eq!(
            id.claims.into_iter().collect::<Vec<_>>(),
            vec![
                ("agent".to_owned(), "claude".to_owned()),
                ("session".to_owned(), "s-77".to_owned())
            ]
        );
        assert_eq!(
            id.presented_at,
            PresentedAt::Header(HeaderName::from_static("authorization"))
        );
    }

    #[tokio::test]
    async fn hmac_rejects_expired_token_from_the_exp_second_on() {
        let ex = extractor();
        for exp in [NOW, NOW - 1, 0] {
            let mut c = good_claims();
            c["exp"] = json!(exp);
            assert_eq!(
                ex.extract(&bearer(&mint_hmac(SECRET, &c))).await,
                Err(ExtractionError::Expired),
                "exp {exp}"
            );
        }
        let mut c = good_claims();
        c["exp"] = json!(NOW + 1);
        assert!(ex.extract(&bearer(&mint_hmac(SECRET, &c))).await.is_ok());
        for bad in [json!("soon"), json!(1.5), Value::Null] {
            let mut c = good_claims();
            c["exp"] = bad;
            assert!(matches!(
                ex.extract(&bearer(&mint_hmac(SECRET, &c))).await,
                Err(ExtractionError::Invalid(_))
            ));
        }
    }

    #[tokio::test]
    async fn hmac_rejects_wrong_namespace_even_with_a_good_signature() {
        let mut c = good_claims();
        c["ns"] = json!("host-rpc");
        let err = extractor()
            .extract(&bearer(&mint_hmac(SECRET, &c)))
            .await
            .expect_err("other namespace");
        assert!(
            matches!(&err, ExtractionError::WrongNamespace(m) if m.contains("host-rpc")),
            "{err:?}"
        );
        // Wrong namespace and expired: reported as the wrong binding.
        c["exp"] = json!(NOW - 100);
        assert!(matches!(
            extractor().extract(&bearer(&mint_hmac(SECRET, &c))).await,
            Err(ExtractionError::WrongNamespace(_))
        ));
        // No namespace at all is malformed, not "another namespace".
        let mut c = good_claims();
        c.as_object_mut().expect("object").remove("ns");
        assert!(matches!(
            extractor().extract(&bearer(&mint_hmac(SECRET, &c))).await,
            Err(ExtractionError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn hmac_rejects_bad_signature_and_tampering() {
        let ex = extractor();
        let other_key = mint_hmac(b"some-other-secret", &good_claims());
        assert!(matches!(
            ex.extract(&bearer(&other_key)).await,
            Err(ExtractionError::Invalid(_))
        ));
        // Tamper with the claims but keep the signature: identity swap.
        let token = mint_hmac(SECRET, &good_claims());
        let (_, sig) = token.split_once('.').expect("two parts");
        let mut forged = good_claims();
        forged["subject"] = json!("someone-else");
        let forged_hex = hex::encode(serde_json::to_vec(&forged).expect("json"));
        assert!(matches!(
            ex.extract(&bearer(&format!("{forged_hex}.{sig}"))).await,
            Err(ExtractionError::Invalid(_))
        ));
        // Same MAC, but not the canonical wire form.
        let (claims_hex, sig) = token.split_once('.').expect("two parts");
        let upper = format!("{claims_hex}.{}", sig.to_uppercase());
        let truncated = format!("{claims_hex}.{}", &sig[..62]);
        for bad in [
            upper.as_str(),
            truncated.as_str(),
            "nodot",
            ".abc",
            "abc.",
            "zz.00",
        ] {
            assert!(
                matches!(
                    ex.extract(&bearer(bad)).await,
                    Err(ExtractionError::Invalid(_))
                ),
                "{bad}"
            );
        }
        assert_eq!(
            ex.extract(&head("/v1/messages", &[])).await,
            Err(ExtractionError::NoMaterial)
        );
    }

    #[tokio::test]
    async fn hmac_claimed_identity_is_decoded_without_verifying() {
        let forged = mint_hmac(b"attacker-key", &good_claims());
        let ex = extractor();
        let req = bearer(&forged);
        assert!(ex.extract(&req).await.is_err());
        assert_eq!(ex.claimed_identity(&req).as_deref(), Some("sess-1"));
        assert_eq!(ex.claimed_identity(&bearer("garbage")), None);
    }

    #[test]
    fn hmac_refuses_an_empty_secret() {
        assert!(matches!(
            HmacExtractor::new(spec(), &Secret::new(Vec::new()), clock_at(NOW)),
            Err(ExtractorBuildError::EmptySecret)
        ));
    }
}
