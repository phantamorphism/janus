use std::sync::Arc;
use std::io::Write;
use async_trait::async_trait;
use janus_core::contract::plugin::{JanusPlugin, NotifyEvent, PluginError, ResolveRoute, RouteOutcome};
use janus_daemon::{run, DaemonConfig, Embedded};

struct Denier;

#[async_trait]
impl JanusPlugin for Denier {
    async fn resolve_route(&self, req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
        if req.caller_identity == "x" {
            Ok(RouteOutcome::Deny { reason: "x is not allowed".to_string() })
        } else {
            Ok(RouteOutcome::UseDefault)
        }
    }
    async fn notify(&self, event: NotifyEvent) {
        if let Ok(p) = std::env::var("LINKED_NOTIFY_FILE") {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{}", serde_json::json!({
                    "event_type": event.event_type,
                    "request_id": event.request_id,
                    "payload": event.payload_json,
                }));
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).expect("config path");
    let config = match DaemonConfig::load(std::path::Path::new(&path)) {
        Ok(c) => c,
        Err(e) => { eprintln!("custom-build: config: {e}"); std::process::exit(1); }
    };
    let embedded = Embedded { plugin: Some(Arc::new(Denier)), extractors: vec![] };
    if let Err(e) = run(config, embedded).await {
        eprintln!("custom-build: {e}");
        std::process::exit(1);
    }
}
