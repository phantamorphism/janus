// Scratch custom daemon build for blind QA: a linked plugin that panics for identity "crash"
// and appends every notify() it receives to $CD_NOTIFY_LOG.
use std::io::Write;
use std::sync::Arc;
use janus_core::contract::plugin::{JanusPlugin, NotifyEvent, PluginError, ResolveRoute, RouteOutcome};
use janus_daemon::contract::config::DaemonConfig;
use janus_daemon::contract::embedded::Embedded;

struct P;
#[async_trait::async_trait]
impl JanusPlugin for P {
    async fn resolve_route(&self, req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
        if req.caller_identity == "crash" {
            panic!("linked plugin deliberately panics");
        }
        Ok(match req.caller_identity.as_str() {
            "badkey" => RouteOutcome::Credential("acct-bad".to_string()),
            _ => RouteOutcome::UseDefault,
        })
    }
    async fn notify(&self, event: NotifyEvent) {
        let path = std::env::var("CD_NOTIFY_LOG").unwrap();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(f, "{}\t{}\t{}", event.event_type, event.request_id.unwrap_or_default(), event.payload_json).unwrap();
    }
}

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(2).expect("usage: cd-linked --config <path>");
    let config = DaemonConfig::load(std::path::Path::new(&path)).expect("config");
    let embedded = Embedded { plugin: Some(Arc::new(P)), extractors: vec![] };
    if let Err(e) = janus_daemon::run(config, embedded).await {
        eprintln!("cd-linked: {e}");
        std::process::exit(1);
    }
}
