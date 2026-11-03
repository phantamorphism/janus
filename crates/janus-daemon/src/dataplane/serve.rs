//! The data-plane accept loop. One task per connection (HTTP/1.1 and h2c);
//! on cancellation the listener closes, every connection is told to finish
//! its in-flight requests and close, and whatever is still running after
//! [`SHUTDOWN_GRACE`] is aborted — which drops its request futures and
//! relay bodies, so their settlement still runs (`abandoned`, or a
//! relay that ended with the caller gone).

use std::sync::Arc;
use std::time::Duration;

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::{router, DataPlane};

/// How long in-flight requests (streamed responses included) may run after
/// shutdown begins before their connections are aborted.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Pause after a failed `accept` (e.g. out of file descriptors) so the loop
/// does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Serve `listener` until `cancel` fires and every connection has finished
/// or been aborted.
pub(crate) async fn serve(listener: TcpListener, dp: Arc<DataPlane>, cancel: CancellationToken) {
    let app = router(dp);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _peer)) => {
                    if let Err(e) = stream.set_nodelay(true) {
                        tracing::debug!(error = %e, "cannot set TCP_NODELAY on a data-plane connection");
                    }
                    let service = TowerToHyperService::new(app.clone());
                    let cancel = cancel.clone();
                    connections.spawn(async move {
                        let builder = auto::Builder::new(TokioExecutor::new());
                        let connection = builder.serve_connection(TokioIo::new(stream), service);
                        tokio::pin!(connection);
                        let result = tokio::select! {
                            result = connection.as_mut() => result,
                            () = cancel.cancelled() => {
                                connection.as_mut().graceful_shutdown();
                                connection.await
                            }
                        };
                        if let Err(e) = result {
                            tracing::debug!(error = %e, "data-plane connection ended with an error");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "data-plane accept failed");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(listener);

    let drain = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
        tracing::warn!(
            open = connections.len(),
            "data-plane connections still open after the shutdown grace period; aborting them"
        );
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
}
