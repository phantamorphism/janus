use std::{collections::BTreeMap, path::Path, sync::Arc};
use janus_core::contract::identity::{ExtractedIdentity, ExtractionError, IdentityExtractor, PresentedAt, RequestHead};
use janus_daemon::{DaemonConfig, Embedded};

struct Introspect;
#[async_trait::async_trait]
impl IdentityExtractor for Introspect {
    fn name(&self) -> &str { "introspect" }
    async fn extract(&self, req: &RequestHead) -> Result<ExtractedIdentity, ExtractionError> {
        let h = match req.headers.get("x-introspect") { Some(h) => h, None => return Err(ExtractionError::NoMaterial) };
        let v = h.to_str().unwrap_or("");
        match v.strip_prefix("good-") {
            Some(id) => Ok(ExtractedIdentity {
                caller_identity: id.to_string(),
                claims: BTreeMap::new(),
                correlation_id: None,
                presented_at: PresentedAt::Header("x-introspect".parse().unwrap()),
            }),
            None => Err(ExtractionError::Invalid("not active".into())),
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cfg_path = args.iter().position(|a| a == "--config").map(|i| args[i + 1].clone()).expect("--config");
    let embedded = Embedded { plugin: None, extractors: vec![Arc::new(Introspect)] };
    if args.iter().any(|a| a == "check-config") {
        match janus_daemon::check_config(Path::new(&cfg_path), embedded).await { Ok(_) => println!("config OK"), Err(e) => { eprintln!("{e}"); std::process::exit(1) } }
        return;
    }
    let cfg = DaemonConfig::load(Path::new(&cfg_path)).unwrap_or_else(|e| { eprintln!("{e}"); std::process::exit(1) });
    if let Err(e) = janus_daemon::run(cfg, embedded).await { eprintln!("{e}"); std::process::exit(1) }
}
