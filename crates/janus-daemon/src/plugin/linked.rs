//! `LinkedPluginClient`: the compiled-in plugin, called directly
//! (`docs/plugin-contract#plugin-client`).
//!
//! No proto types are constructed and nothing is serialized. Each
//! `resolve_route` is wrapped so that a panic in the plugin — whether while
//! building its future or while it is being polled — becomes a
//! [`PluginError`] of class `plugin_error`, which the fail-closed path turns
//! into a denied request, instead of unwinding into the request task. That
//! is a guardrail, not isolation.
//!
//! `[plugin] timeout_ms` applies here too: config accepts it next to
//! `linked = true` (where it would otherwise be refused as having no
//! effect), and the deadline it names is on each `ResolveRoute` whatever
//! the execution model. A linked plugin that blocks its thread without
//! yielding cannot be interrupted; one that awaits past the deadline fails
//! closed as `plugin_timeout`.
//!
//! The linked plugin's answer is held to the same contract invariants as an
//! answer decoded from the wire ([`RouteOutcome::validate`]).

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use janus_core::contract::plugin::{
    JanusPlugin, NotifyEvent, PluginClient, PluginError, PluginFailureClass, ResolveRoute,
    RouteOutcome,
};
use tokio::sync::mpsc;

use super::notify::{NotifyQueue, NOTIFY_QUEUE_CAPACITY};
use super::SetupError;

/// The client for a plugin compiled into this build.
pub struct LinkedPluginClient<P: JanusPlugin> {
    plugin: Arc<P>,
    timeout: Duration,
    notify: NotifyQueue,
}

impl<P: JanusPlugin> std::fmt::Debug for LinkedPluginClient<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkedPluginClient")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// The text of a panic payload.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

fn panicked(payload: &(dyn Any + Send)) -> PluginError {
    PluginError::new(
        PluginFailureClass::Error,
        format!("linked plugin panicked: {}", panic_message(payload)),
    )
}

impl<P: JanusPlugin> LinkedPluginClient<P> {
    /// Wrap `plugin`, with `timeout` as the `ResolveRoute` deadline. Starts
    /// the task that delivers `notify` events, so it must be called from
    /// within a Tokio runtime.
    pub fn new(plugin: P, timeout: Duration) -> Result<Self, SetupError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| SetupError::NoRuntime)?;
        let plugin = Arc::new(plugin);
        let (notify, rx) = NotifyQueue::new(NOTIFY_QUEUE_CAPACITY);
        runtime.spawn(deliver(Arc::clone(&plugin), rx));
        Ok(LinkedPluginClient {
            plugin,
            timeout,
            notify,
        })
    }

    /// `notify` events dropped because the plugin could not keep up.
    pub fn dropped_events(&self) -> u64 {
        self.notify.dropped()
    }
}

/// Deliver events one at a time, in order. A panic in the plugin's
/// `notify` is logged and the next event is delivered.
async fn deliver<P: JanusPlugin>(plugin: Arc<P>, mut rx: mpsc::Receiver<NotifyEvent>) {
    while let Some(event) = rx.recv().await {
        let event_type = event.event_type.clone();
        let call = std::panic::catch_unwind(AssertUnwindSafe(|| plugin.notify(event)));
        let outcome = match call {
            Ok(fut) => AssertUnwindSafe(fut).catch_unwind().await,
            Err(payload) => Err(payload),
        };
        if let Err(payload) = outcome {
            tracing::warn!(
                event_type = %event_type,
                "linked plugin panicked in notify: {}",
                panic_message(payload.as_ref())
            );
        }
    }
}

#[async_trait]
impl<P: JanusPlugin> PluginClient for LinkedPluginClient<P> {
    async fn resolve_route(&self, req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
        // A panic while constructing the future (a hand-written
        // implementation can panic before its first poll) ...
        let fut = std::panic::catch_unwind(AssertUnwindSafe(|| self.plugin.resolve_route(req)))
            .map_err(|payload| panicked(payload.as_ref()))?;
        // ... and one while polling it are both caught.
        let guarded = AssertUnwindSafe(fut).catch_unwind();
        let answer = match tokio::time::timeout(self.timeout, guarded).await {
            Err(_) => {
                return Err(PluginError::new(
                    PluginFailureClass::Timeout,
                    format!(
                        "linked plugin did not answer within {} ms",
                        self.timeout.as_millis()
                    ),
                ))
            }
            Ok(Err(payload)) => return Err(panicked(payload.as_ref())),
            Ok(Ok(answer)) => answer?,
        };
        answer.validate()?;
        Ok(answer)
    }

    fn notify(&self, event: NotifyEvent) {
        self.notify.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    fn request() -> ResolveRoute {
        ResolveRoute {
            request_id: "req-1".into(),
            upstream: "anthropic".into(),
            implementation: "anthropic".into(),
            caller_identity: "session-1".into(),
            correlation_id: None,
            identity_claims: BTreeMap::new(),
        }
    }

    /// A linked plugin whose behaviour is chosen per test.
    struct Scripted {
        mode: &'static str,
        seen: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(mode: &'static str) -> Self {
            Scripted {
                mode,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl JanusPlugin for Scripted {
        async fn resolve_route(&self, req: ResolveRoute) -> Result<RouteOutcome, PluginError> {
            tokio::task::yield_now().await;
            match self.mode {
                "panic_after_await" => panic!("policy table missing for {}", req.caller_identity),
                "hang" => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(RouteOutcome::UseDefault)
                }
                "empty_deny" => Ok(RouteOutcome::Deny {
                    reason: String::new(),
                }),
                "own_error" => Err(PluginError::new(
                    PluginFailureClass::Unreachable,
                    "policy database down",
                )),
                _ => Ok(RouteOutcome::Credential(format!(
                    "cred-{}",
                    req.caller_identity
                ))),
            }
        }

        async fn notify(&self, event: NotifyEvent) {
            if event.event_type == "boom" {
                panic!("cannot handle boom");
            }
            self.seen.lock().expect("lock").push(event.event_type);
        }
    }

    /// Panics synchronously, before any future exists.
    struct PanicsEagerly;

    impl JanusPlugin for PanicsEagerly {
        fn resolve_route<'a, 'b>(
            &'a self,
            _req: ResolveRoute,
        ) -> Pin<Box<dyn Future<Output = Result<RouteOutcome, PluginError>> + Send + 'b>>
        where
            'a: 'b,
        {
            panic!("eager panic")
        }

        fn notify<'a, 'b>(
            &'a self,
            _event: NotifyEvent,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'b>>
        where
            'a: 'b,
        {
            panic!("eager notify panic")
        }
    }

    fn event(kind: &str) -> NotifyEvent {
        NotifyEvent {
            event_type: kind.into(),
            request_id: None,
            correlation_id: None,
            payload_json: "{}".into(),
        }
    }

    #[tokio::test]
    async fn linked_panic_becomes_plugin_error() {
        let client =
            LinkedPluginClient::new(Scripted::new("panic_after_await"), Duration::from_secs(5))
                .expect("client");
        let err = client
            .resolve_route(request())
            .await
            .expect_err("panic fails closed");
        assert_eq!(err.class, PluginFailureClass::Error);
        assert!(
            err.message.contains("policy table missing for session-1"),
            "{err}"
        );

        let eager = LinkedPluginClient::new(PanicsEagerly, Duration::from_secs(5)).expect("client");
        let err = eager
            .resolve_route(request())
            .await
            .expect_err("eager panic");
        assert_eq!(err.class, PluginFailureClass::Error);
        assert!(err.message.contains("eager panic"), "{err}");

        // The client is still usable after a caught panic.
        let ok = LinkedPluginClient::new(Scripted::new("answer"), Duration::from_secs(5))
            .expect("client");
        assert_eq!(
            ok.resolve_route(request()).await,
            Ok(RouteOutcome::Credential("cred-session-1".into()))
        );
    }

    #[tokio::test]
    async fn linked_hung_plugin_fails_closed_as_timeout() {
        let client = LinkedPluginClient::new(Scripted::new("hang"), Duration::from_millis(50))
            .expect("client");
        let started = std::time::Instant::now();
        let err = client.resolve_route(request()).await.expect_err("hung");
        assert_eq!(err.class, PluginFailureClass::Timeout);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn linked_answers_are_held_to_contract_invariants() {
        let client = LinkedPluginClient::new(Scripted::new("empty_deny"), Duration::from_secs(5))
            .expect("client");
        let err = client
            .resolve_route(request())
            .await
            .expect_err("empty deny reason");
        assert_eq!(err.class, PluginFailureClass::Error);

        let client = LinkedPluginClient::new(Scripted::new("own_error"), Duration::from_secs(5))
            .expect("client");
        let err = client
            .resolve_route(request())
            .await
            .expect_err("plugin's own error");
        assert_eq!(err.class, PluginFailureClass::Unreachable);
        assert_eq!(err.message, "policy database down");
    }

    #[tokio::test]
    async fn linked_notify_is_delivered_in_order_and_survives_panics() {
        let plugin = Arc::new(Scripted::new("answer"));
        let client =
            LinkedPluginClient::new(Arc::clone(&plugin), Duration::from_secs(5)).expect("client");
        for kind in ["call_completed", "boom", "call_abandoned"] {
            client.notify(event(kind));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while plugin.seen.lock().expect("lock").len() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            *plugin.seen.lock().expect("lock"),
            vec!["call_completed".to_owned(), "call_abandoned".to_owned()]
        );

        let eager = LinkedPluginClient::new(PanicsEagerly, Duration::from_secs(5)).expect("client");
        // A dead worker closes the queue, and pushes to a closed queue are
        // counted as dropped: give it time to die between the two.
        eager.notify(event("anything"));
        tokio::time::sleep(Duration::from_millis(20)).await;
        eager.notify(event("again"));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(eager.dropped_events(), 0, "the delivery task survived");
    }

    #[test]
    fn linked_client_needs_a_runtime() {
        let err = LinkedPluginClient::new(Scripted::new("answer"), Duration::from_secs(1))
            .expect_err("no runtime");
        assert!(matches!(err, SetupError::NoRuntime));
    }
}
