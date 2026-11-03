//! Test fixtures for the identity module: request heads, a fixed clock,
//! real HMAC minting and JWT signing (Janus itself mints nothing), an
//! in-memory opaque registry, and a local JWKS endpoint.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use hmac::{Hmac, Mac};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri};
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::Value;
use sha2::Sha256;
use tokio::net::TcpListener;
use url::Url;

use crate::contract::identity::RequestHead;
use crate::contract::registry::{IdentityRecord, OpaqueRegistry, TokenHash};

use super::factory::Clock;

/// Ed25519 key pair "k1" (test fixture only).
pub(crate) const ED_PRIVATE: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIA6PCxRZnkBAvbVM9nxSAAHq+GM9OqwDda00g6MGp75B
-----END PRIVATE KEY-----
";
pub(crate) const ED_PUBLIC: &str = "-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAfyP7MMJLuaTkfemj9sISz8q02zMmLfQYZ34/s/VW31I=
-----END PUBLIC KEY-----
";
/// `x` of [`ED_PUBLIC`] as a JWK.
pub(crate) const ED_X: &str = "fyP7MMJLuaTkfemj9sISz8q02zMmLfQYZ34_s_VW31I";

/// A second Ed25519 key pair "k2".
pub(crate) const ED2_PRIVATE: &str = "-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIDhrB+bPdtqPi5Q0u/Xzg2bITC0LZ5AUHrRi4xxkDWxU
-----END PRIVATE KEY-----
";
pub(crate) const ED2_X: &str = "VddmuTCiux6sUGFfemKgjkzNicrQdkLSUrgGYw0IyBY";

/// A P-256 key pair.
pub(crate) const EC_PRIVATE: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgelvdzY7W1C4Ycq+e
kfZz4hq14L55ydc3t70cSFpsdXmhRANCAARJM1LUYdwwRPrYS05ApK18+Vz38f/q
HsZ7xze+NTn92gsvuCsnNuzGgMXJXtv0ZASSVWPApabcMaP/euGsinL8
-----END PRIVATE KEY-----
";
pub(crate) const EC_PUBLIC: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAESTNS1GHcMET62EtOQKStfPlc9/H/
6h7Ge8c3vjU5/doLL7grJzbsxoDFyV7b9GQEklVjwKWm3DGj/3rhrIpy/A==
-----END PUBLIC KEY-----
";

pub(crate) fn head(uri: &str, headers: &[(&str, &str)]) -> RequestHead {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    RequestHead::new(Method::POST, uri.parse::<Uri>().expect("uri"), map)
}

pub(crate) fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).single().expect("timestamp")
}

pub(crate) fn clock_at(secs: i64) -> Clock {
    Clock::from_fn(move || at(secs))
}

/// `<claims-hex>.<signature-hex>` exactly as a host would mint it.
pub(crate) fn mint_hmac(secret: &[u8], claims: &Value) -> String {
    let claims_hex = hex::encode(serde_json::to_vec(claims).expect("claims json"));
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("hmac key");
    mac.update(claims_hex.as_bytes());
    format!("{claims_hex}.{}", hex::encode(mac.finalize().into_bytes()))
}

pub(crate) fn sign_jwt(
    alg: Algorithm,
    private_pem: &str,
    kid: Option<&str>,
    claims: &Value,
) -> String {
    let key = match alg {
        Algorithm::EdDSA => EncodingKey::from_ed_pem(private_pem.as_bytes()),
        _ => EncodingKey::from_ec_pem(private_pem.as_bytes()),
    }
    .expect("signing key");
    let mut header = Header::new(alg);
    header.kid = kid.map(str::to_owned);
    jsonwebtoken::encode(&header, claims, &key).expect("signs")
}

pub(crate) fn record(
    identity: &str,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
) -> IdentityRecord {
    IdentityRecord {
        id: format!("id-{identity}"),
        identity: identity.into(),
        claims: BTreeMap::from([("tenant".to_owned(), "acme".to_owned())]),
        label: None,
        created_at: at(1_000),
        expires_at,
        revoked_at,
        last_seen_at: None,
    }
}

/// The daemon store's in-memory index, as far as the extractor can tell:
/// entries keyed by token hash only.
#[derive(Default)]
pub(crate) struct FakeRegistry {
    entries: Mutex<HashMap<TokenHash, IdentityRecord>>,
}

impl FakeRegistry {
    pub(crate) fn insert(&self, token: &str, rec: IdentityRecord) {
        self.entries
            .lock()
            .expect("lock")
            .insert(TokenHash::of(token.as_bytes()), rec);
    }

    pub(crate) fn revoke(&self, token: &str, when: DateTime<Utc>) {
        if let Some(rec) = self
            .entries
            .lock()
            .expect("lock")
            .get_mut(&TokenHash::of(token.as_bytes()))
        {
            rec.revoked_at = Some(when);
        }
    }

    pub(crate) fn last_seen(&self, token: &str) -> Option<DateTime<Utc>> {
        self.entries
            .lock()
            .expect("lock")
            .get(&TokenHash::of(token.as_bytes()))
            .and_then(|r| r.last_seen_at)
    }
}

impl OpaqueRegistry for FakeRegistry {
    fn lookup(&self, token_hash: &TokenHash, now: DateTime<Utc>) -> Option<IdentityRecord> {
        let mut entries = self.entries.lock().expect("lock");
        let rec = entries.get_mut(token_hash)?;
        rec.last_seen_at = Some(now);
        Some(rec.clone())
    }
}

struct JwksState {
    document: Value,
    failing: bool,
}

/// A JWKS endpoint on a loopback port that counts its requests.
pub(crate) struct JwksServer {
    url: Url,
    state: Arc<Mutex<JwksState>>,
    fetches: Arc<AtomicUsize>,
}

impl JwksServer {
    pub(crate) async fn start(document: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let state = Arc::new(Mutex::new(JwksState {
            document,
            failing: false,
        }));
        let fetches = Arc::new(AtomicUsize::new(0));
        let (st, count) = (state.clone(), fetches.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (st, count) = (st.clone(), count.clone());
                tokio::spawn(async move {
                    let service = service_fn(move |_req| {
                        count.fetch_add(1, Ordering::SeqCst);
                        let s = st.lock().expect("lock");
                        let (status, body) = if s.failing {
                            (StatusCode::INTERNAL_SERVER_ERROR, "down".to_owned())
                        } else {
                            (StatusCode::OK, s.document.to_string())
                        };
                        let resp = Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(Full::new(Bytes::from(body)))
                            .expect("response");
                        async move { Ok::<_, Infallible>(resp) }
                    });
                    // A connection error only ends that connection.
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        JwksServer {
            url: Url::parse(&format!("http://{addr}/.well-known/jwks.json")).expect("url"),
            state,
            fetches,
        }
    }

    pub(crate) fn url(&self) -> Url {
        self.url.clone()
    }

    pub(crate) fn set(&self, document: Value) {
        self.state.lock().expect("lock").document = document;
    }

    pub(crate) fn fail(&self) {
        self.state.lock().expect("lock").failing = true;
    }

    pub(crate) fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

/// A URL on a loopback port nothing listens on.
pub(crate) async fn closed_port_url() -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    Url::parse(&format!("http://{addr}/jwks.json")).expect("url")
}
