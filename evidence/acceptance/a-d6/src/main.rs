use async_trait::async_trait;
use http::{request::Parts, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use janus_core::contract::credential::{CredentialKind, CredentialMaterial, MaterialDocument, Secret, UpstreamFeedback};
use janus_core::contract::identity::{PresentedAt, RequestHead};
use janus_core::contract::upstream::{DecodedMaterial, UpstreamError, UpstreamImpl, UpstreamKind, UsageObserver};
use janus_core::proxy::{build_outbound, OpenAiUpstream, Substitution, SubstitutionError};
use url::Url;

/// OpenAI upstream whose forwarding allowlist is deliberately misconfigured.
struct Leaky { inner: OpenAiUpstream, list: Vec<HeaderName> }
#[async_trait]
impl UpstreamImpl for Leaky {
    fn kind(&self) -> UpstreamKind { self.inner.kind() }
    fn forwarded_headers(&self) -> &[HeaderName] { &self.list }
    fn outbound_url(&self, b: &Url, r: &str, k: CredentialKind) -> Result<Url, UpstreamError> { self.inner.outbound_url(b, r, k) }
    fn apply_auth(&self, p: &mut Parts, m: &CredentialMaterial) -> Result<(), UpstreamError> { self.inner.apply_auth(p, m) }
    fn decode_material(&self, d: &MaterialDocument) -> Result<DecodedMaterial, UpstreamError> { self.inner.decode_material(d) }
    async fn refresh(&self, d: &MaterialDocument) -> Result<MaterialDocument, UpstreamError> { self.inner.refresh(d).await }
    fn usage_observer(&self) -> Box<dyn UsageObserver> { self.inner.usage_observer() }
    fn classify(&self, s: StatusCode, h: &HeaderMap, b: &[u8]) -> Option<UpstreamFeedback> { self.inner.classify(s, h, b) }
}

const TOKEN: &str = "CALLER-IDENTITY-TOKEN-xyz";
const KEY: &str = "sk-POOL-KEY-123";

fn head(hdr: &str) -> RequestHead {
    let mut h = HeaderMap::new();
    h.insert(HeaderName::from_bytes(hdr.as_bytes()).unwrap(), HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap());
    h.insert("content-type", HeaderValue::from_static("application/json"));
    RequestHead::new(Method::POST, "/openai/v1/chat".parse::<Uri>().unwrap(), h)
}
fn material() -> CredentialMaterial {
    CredentialMaterial { credential_ref: "r1".into(), upstream: UpstreamKind::OpenAi, kind: CredentialKind::ApiKey, secret: Secret::new(KEY.as_bytes().to_vec()), account_id: None }
}

fn run(name: &str, up: &dyn UpstreamImpl, hdr: &str, at: Option<PresentedAt>) -> bool {
    let base = Url::parse("http://127.0.0.1:9/").unwrap();
    let m = material();
    let h = head(hdr);
    let r = build_outbound(&h, Substitution { upstream: up, base_url: &base, rest: "/v1/chat", material: &m, presented_at: at.as_ref() });
    match &r {
        Ok(p) => {
            let dump = format!("{:?} {:?}", p.uri, p.headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("?").to_string())).collect::<Vec<_>>());
            println!("CASE {name}: OK uri={} headers={}", p.uri, dump);
            println!("  contains_caller_token={} contains_pool_key={}", dump.contains(TOKEN), dump.contains(KEY));
        }
        Err(SubstitutionError::IdentityLeak { upstream, location }) => println!("CASE {name}: ERR IdentityLeak upstream={:?} location={location}", upstream.as_str()),
        Err(e) => println!("CASE {name}: ERR other: {e}"),
    }
    match r { Ok(p) => !format!("{:?}", p.headers).contains(TOKEN) && !p.uri.to_string().contains(TOKEN), Err(_) => false }
}

fn main() {
    let hn = |s: &str| HeaderName::from_bytes(s.as_bytes()).unwrap();
    let stock = OpenAiUpstream::new().unwrap();
    let mut fails = 0;
    // A control: stock allowlist, identity in authorization -> Ok without token
    if !run("A-control-stock-allowlist", &stock, "authorization", Some(PresentedAt::Header(hn("authorization")))) { println!("A FAILED"); fails += 1; }
    // B: allowlist wrongly includes x-api-key which carries the identity -> must be Err
    let leaky = Leaky { inner: OpenAiUpstream::new().unwrap(), list: vec![hn("content-type"), hn("x-api-key")] };
    if run("B-leaky-allowlist-x-api-key", &leaky, "x-api-key", Some(PresentedAt::Header(hn("x-api-key")))) { println!("B FAILED: built a request"); fails += 1; }
    // C: allowlist includes authorization, identity in authorization
    let leaky2 = Leaky { inner: OpenAiUpstream::new().unwrap(), list: vec![hn("content-type"), hn("authorization")] };
    if run("C-leaky-allowlist-authorization", &leaky2, "authorization", Some(PresentedAt::Header(hn("authorization")))) { println!("C FAILED"); fails += 1; }
    // D: basic-password identity (lives in authorization) with authorization allowlisted
    if run("D-leaky-basic-password", &leaky2, "authorization", Some(PresentedAt::BasicPassword)) { println!("D FAILED"); fails += 1; }
    // E: same leaky allowlist but no identity presented (method none) -> Ok
    let ok_e = run("E-leaky-allowlist-but-no-identity-presented", &leaky, "x-api-key", None);
    println!("E ok_request_built={ok_e}");
    println!("FAILS={fails} E_ok={ok_e}");
    std::process::exit(if fails == 0 { 0 } else { 1 });
}
