use janus_core::contract::proto::janus_plugin_server::{JanusPlugin, JanusPluginServer};
use janus_core::contract::proto::{NotifyAck, NotifyEvent, ResolveRouteRequest, ResolveRouteResponse, resolve_route_response::Outcome};
use janus_core::peer::PeerUid;
use tonic::{Request, Response, Status, Streaming};

struct P;
#[tonic::async_trait]
impl JanusPlugin for P {
    async fn resolve_route(&self, _r: Request<ResolveRouteRequest>) -> Result<Response<ResolveRouteResponse>, Status> {
        eprintln!("RESOLVE_ROUTE_SERVED");
        Ok(Response::new(ResolveRouteResponse { outcome: Some(Outcome::UseDefault(true)) }))
    }
    async fn notify(&self, _r: Request<Streaming<NotifyEvent>>) -> Result<Response<NotifyAck>, Status> {
        Ok(Response::new(NotifyAck {}))
    }
}

#[tokio::main]
async fn main() {
    let sock = std::env::args().nth(1).expect("socket path");
    let uid: u32 = std::env::args().nth(2).expect("expected uid").parse().unwrap();
    let _ = std::fs::remove_file(&sock);
    let l = tokio::net::UnixListener::bind(&sock).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o660)).unwrap();
    }
    eprintln!("PEERPLUG_READY expected_uid={uid}");
    tonic::transport::Server::builder()
        .add_service(JanusPluginServer::with_interceptor(P, PeerUid::new(uid)))
        .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(l))
        .await
        .unwrap();
}
