//! The keeper: what it knows about each managed credential, when a login
//! flow runs (one at a time, debounced, backed off), the periodic check
//! through the admin API, and the rotation after a login.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use chrono::{DateTime, Utc};
use janus_core::contract::credential::{CredentialKind, CredentialSource, CredentialStatus};
use janus_core::contract::plugin::RouteOutcome;
use serde::Serialize;
use tokio::sync::{mpsc, watch, Notify};

use crate::admin::{Admin, AdminError};
use crate::config::Config;
use crate::login::{Control, Isolation, Login, LoginState, Material, Outcome};
use crate::schedule::Schedule;

/// What the keeper believes about one credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "health", rename_all = "snake_case")]
pub enum Health {
    /// Not checked yet, or the admin surface did not answer.
    Unknown,
    Healthy,
    /// Disabled by an operator: not the keeper's to fix.
    Disabled,
    /// The token endpoint refused the refresh token. The access token still
    /// works until `expires_at`, so requests are served; a login flow
    /// starts.
    RefreshRefused {
        reason: String,
        expires_at: DateTime<Utc>,
    },
    /// Unusable: the daemon marked it invalid. Only a new login fixes it.
    NeedsLogin {
        reason: String,
    },
}

impl Health {
    /// A login flow is wanted.
    fn wants_login(&self) -> bool {
        matches!(
            self,
            Health::NeedsLogin { .. } | Health::RefreshRefused { .. }
        )
    }

    /// Requests through this credential cannot succeed now.
    fn unusable(&self, now: DateTime<Utc>) -> bool {
        match self {
            Health::NeedsLogin { .. } => true,
            Health::RefreshRefused { expires_at, .. } => *expires_at <= now,
            _ => false,
        }
    }
}

struct Entry {
    health: Health,
    schedule: Schedule,
    /// When the admin API last answered a check.
    checked_at: Option<DateTime<Utc>>,
    /// Counts changes of `health`. A check records its answer only if no
    /// event, rotation or other check changed the health while it ran.
    epoch: u64,
}

/// Why a running flow was told to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Operator,
    /// A check or a served request showed the credential works.
    Healed,
    Shutdown,
}

impl Stop {
    fn reason(self) -> &'static str {
        match self {
            Stop::Operator => "cancelled by the operator",
            Stop::Healed => "the credential is usable again",
            Stop::Shutdown => "the keeper is shutting down",
        }
    }
}

/// The attempt now running (from its check to its rotation), as the page
/// and the CLI reach it.
struct Running {
    credential_ref: String,
    /// Started by the operator: a healthy check does not stop it.
    forced: bool,
    /// The flow this attempt runs, once it has one. An operator's attempt
    /// has one from the start; an automatic one only once its check says a
    /// login is needed.
    flow_id: Option<u64>,
    codes: mpsc::Sender<String>,
    cancel: watch::Sender<bool>,
    stop: Option<Stop>,
}

/// How many finished flows `/status` still lists.
const FLOW_HISTORY: usize = 8;

/// One flow, running or finished.
struct FlowRecord {
    /// Counts flows since the keeper started, from 1.
    id: u64,
    credential_ref: String,
    state: Arc<watch::Sender<LoginState>>,
}

struct State {
    entries: BTreeMap<String, Entry>,
    running: Option<Running>,
    /// The task of the running attempt.
    attempt: Option<tokio::task::JoinHandle<()>>,
    /// The latest flows, oldest first; the last is the latest.
    flows: VecDeque<FlowRecord>,
    flows_started: u64,
    /// No new attempt starts.
    shutting_down: bool,
}

impl State {
    fn last_flow(&self) -> Option<&FlowRecord> {
        self.flows.back()
    }

    /// Publish a new flow for `credential_ref`.
    fn new_flow(&mut self, credential_ref: &str) -> (u64, Arc<watch::Sender<LoginState>>) {
        self.flows_started += 1;
        let id = self.flows_started;
        let state = Arc::new(watch::channel(LoginState::Starting).0);
        if self.flows.len() == FLOW_HISTORY {
            self.flows.pop_front();
        }
        self.flows.push_back(FlowRecord {
            id,
            credential_ref: credential_ref.to_owned(),
            state: state.clone(),
        });
        (id, state)
    }
}

/// An attempt's claim on the single flow slot.
struct Claim {
    credential_ref: String,
    forced: bool,
    /// The flow published at claim time (an operator's start).
    flow: Option<Arc<watch::Sender<LoginState>>>,
    control: Control,
}

/// Why `start` refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StartError {
    #[error("credential {0:?} is not managed by this keeper")]
    Unknown(String),
    #[error("a login attempt for {credential_ref:?} is already running")]
    Busy {
        credential_ref: String,
        /// Its flow, once it has one.
        flow_id: Option<u64>,
    },
    #[error("the keeper is shutting down")]
    ShuttingDown,
}

/// The keeper's state for the page and the CLI.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub credentials: Vec<CredentialView>,
    /// The latest flow.
    pub flow: Option<FlowView>,
    /// The latest flows, oldest first, so a client can follow one flow by
    /// its `id` although a newer one started.
    pub flows: Vec<FlowView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialView {
    #[serde(rename = "ref")]
    pub credential_ref: String,
    pub instances: Vec<String>,
    #[serde(flatten)]
    pub health: Health,
    pub checked_at: Option<DateTime<Utc>>,
    pub failed_flows: u32,
    /// Seconds until the pending flow may start; `None` when none is.
    pub next_flow_in_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlowView {
    pub id: u64,
    pub credential_ref: String,
    #[serde(flatten)]
    pub state: LoginState,
}

/// The result of one check.
#[derive(Debug)]
enum Check {
    Healthy,
    Disabled,
    RefreshRefused {
        reason: String,
        expires_at: DateTime<Utc>,
    },
    NeedsLogin(String),
    /// The admin surface did not answer usefully: says nothing.
    Unknown(String),
}

#[derive(Clone)]
pub struct Keeper {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    admin: Admin,
    login: Login,
    state: Mutex<State>,
    wake: Notify,
    /// Tests: the next attempt panics.
    #[cfg(test)]
    panic_next_attempt: std::sync::atomic::AtomicBool,
}

impl Keeper {
    pub fn new(config: Config, admin: Admin) -> Keeper {
        let entries = config
            .credentials
            .iter()
            .map(|c| {
                let entry = Entry {
                    health: Health::Unknown,
                    schedule: Schedule::new(
                        config.debounce,
                        config.backoff_min,
                        config.backoff_max,
                    ),
                    checked_at: None,
                    epoch: 0,
                };
                (c.credential_ref.clone(), entry)
            })
            .collect();
        let login = Login {
            claude_bin: config.claude_bin.clone(),
            isolation: Isolation::new(&config.state_dir),
            timeout: config.login_timeout,
        };
        Keeper {
            inner: Arc::new(Inner {
                config,
                admin,
                login,
                state: Mutex::new(State {
                    entries,
                    running: None,
                    attempt: None,
                    flows: VecDeque::new(),
                    flows_started: 0,
                    shutting_down: false,
                }),
                wake: Notify::new(),
                #[cfg(test)]
                panic_next_attempt: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn isolation(&self) -> &Isolation {
        &self.inner.login.isolation
    }

    /// A `credential_invalid` or `account_error` event. `invalid` marks
    /// the credential as needing a login at once (the daemon has already
    /// marked it), and a check already running cannot overwrite that; an
    /// account error only asks for a check, because a login does not fix a
    /// billing or organization problem. A report while an attempt for the
    /// credential runs queues nothing: the attempt's end looks again.
    pub fn report(&self, credential_ref: &str, invalid: bool, reason: &str) -> bool {
        let mut st = self.lock();
        let busy = running_for(&st, credential_ref);
        let Some(entry) = st.entries.get_mut(credential_ref) else {
            return false;
        };
        if invalid {
            entry.health = Health::NeedsLogin {
                reason: reason.to_owned(),
            };
            entry.epoch += 1;
        }
        if !busy {
            entry.schedule.report(Instant::now());
        }
        drop(st);
        self.inner.wake.notify_one();
        true
    }

    /// The operator asked for a flow: it takes the flow slot now and runs
    /// without debounce, backoff or health gate. Returns the new flow's
    /// id. While any attempt runs, a start is refused as busy.
    pub fn start(&self, credential_ref: &str) -> Result<u64, StartError> {
        let mut st = self.lock();
        if !st.entries.contains_key(credential_ref) {
            return Err(StartError::Unknown(credential_ref.to_owned()));
        }
        if st.shutting_down {
            return Err(StartError::ShuttingDown);
        }
        if let Some(running) = &st.running {
            return Err(StartError::Busy {
                credential_ref: running.credential_ref.clone(),
                flow_id: running.flow_id,
            });
        }
        Ok(self
            .launch(&mut st, credential_ref, true)
            .expect("a forced attempt has a flow"))
    }

    /// Cancel the running attempt for `credential_ref`. `false` when none
    /// runs for it.
    pub fn cancel(&self, credential_ref: &str) -> bool {
        let mut st = self.lock();
        running_for(&st, credential_ref) && stop_running(&mut st, Stop::Operator)
    }

    /// Pass a pasted code to the running flow for `credential_ref`. `false`
    /// when none runs for it.
    pub fn submit_code(&self, credential_ref: &str, code: &str) -> bool {
        match &self.lock().running {
            Some(running) if running.credential_ref == credential_ref => {
                running.codes.try_send(code.to_owned()).is_ok()
            }
            _ => false,
        }
    }

    /// The credential of the running attempt.
    pub fn running_credential(&self) -> Option<String> {
        self.lock()
            .running
            .as_ref()
            .map(|r| r.credential_ref.clone())
    }

    pub fn snapshot(&self) -> Snapshot {
        let st = self.lock();
        let now = Instant::now();
        let credentials = self
            .inner
            .config
            .credentials
            .iter()
            .filter_map(|c| {
                let e = st.entries.get(&c.credential_ref)?;
                Some(CredentialView {
                    credential_ref: c.credential_ref.clone(),
                    instances: c.instances.clone(),
                    health: e.health.clone(),
                    checked_at: e.checked_at,
                    failed_flows: e.schedule.failures(),
                    next_flow_in_secs: e
                        .schedule
                        .due()
                        .map(|due| due.saturating_duration_since(now).as_secs()),
                })
            })
            .collect();
        let view = |f: &FlowRecord| FlowView {
            id: f.id,
            credential_ref: f.credential_ref.clone(),
            state: f.state.borrow().clone(),
        };
        Snapshot {
            credentials,
            flow: st.last_flow().map(view),
            flows: st.flows.iter().map(view).collect(),
        }
    }

    /// `ResolveRoute`: `use_default`, unless every managed credential for
    /// the instance is unusable now and denying is configured. A refused
    /// refresh token alone does not deny while the access token is valid.
    pub fn route(&self, instance: &str) -> RouteOutcome {
        let config = &self.inner.config;
        if !config.deny_when_unusable {
            return RouteOutcome::UseDefault;
        }
        let refs: Vec<&str> = config
            .serving(instance)
            .map(|c| c.credential_ref.as_str())
            .collect();
        let st = self.lock();
        let now = Utc::now();
        let all_dead = !refs.is_empty()
            && refs
                .iter()
                .all(|r| st.entries.get(*r).is_some_and(|e| e.health.unusable(now)));
        if !all_dead {
            return RouteOutcome::UseDefault;
        }
        let page = format!("http://{}/", config.http_bind);
        let waiting = st.last_flow().and_then(|f| match &*f.state.borrow() {
            LoginState::AwaitingBrowser { auth_url, .. } => {
                Some((f.credential_ref.clone(), auth_url.clone()))
            }
            _ => None,
        });
        let how = match waiting {
            Some((r, url)) => format!(
                "open {url} , sign in, and paste the code it shows at {page} (credential {r})"
            ),
            None => format!(
                "open {page} on the keeper host, or run `claude-login-keeper login --credential {}` there",
                refs[0]
            ),
        };
        RouteOutcome::Deny {
            reason: format!("claude login needs re-authorization: {how}"),
        }
    }

    /// The keeper's loop until `shutdown` resolves: check every credential
    /// now and every `validate_interval` (also while a flow runs), and start
    /// due attempts one at a time. An attempt running at shutdown is
    /// cancelled and awaited.
    pub async fn run(&self, shutdown: impl Future<Output = ()>) {
        self.inner.login.reap_orphan().await;
        tokio::pin!(shutdown);
        let mut tick = tokio::time::interval(self.inner.config.validate_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let due = self.next_due();
            let sleep = async {
                match &due {
                    Some((_, at)) => tokio::time::sleep_until((*at).into()).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = &mut shutdown => break,
                _ = tick.tick() => {
                    tokio::select! {
                        () = self.validate_all() => {}
                        () = &mut shutdown => break,
                    }
                }
                () = self.inner.wake.notified() => {}
                () = sleep => {
                    if let Some((credential_ref, _)) = due {
                        self.start_due(&credential_ref);
                    }
                }
            }
        }
        let handle = {
            let mut st = self.lock();
            st.shutting_down = true;
            stop_running(&mut st, Stop::Shutdown);
            st.attempt.take()
        };
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// The pending credential whose attempt may start first; `None` while
    /// an attempt runs.
    fn next_due(&self) -> Option<(String, Instant)> {
        let st = self.lock();
        if st.running.is_some() {
            return None;
        }
        st.entries
            .iter()
            .filter_map(|(r, e)| Some((r.clone(), e.schedule.due()?)))
            .min_by_key(|(_, at)| *at)
    }

    /// Start the automatic attempt for `credential_ref` if it is still due
    /// and the slot is free.
    fn start_due(&self, credential_ref: &str) {
        let mut st = self.lock();
        let due = st
            .entries
            .get(credential_ref)
            .and_then(|e| e.schedule.due())
            .is_some_and(|at| at <= Instant::now());
        if due && st.running.is_none() && !st.shutting_down {
            self.launch(&mut st, credential_ref, false);
        }
    }

    /// Take the flow slot for `credential_ref` and spawn its attempt. A
    /// `forced` (operator's) attempt publishes its flow now and returns its
    /// id. The caller checked that the slot is free.
    fn launch(&self, st: &mut State, credential_ref: &str, forced: bool) -> Option<u64> {
        if let Some(entry) = st.entries.get_mut(credential_ref) {
            entry.schedule.started();
        }
        let flow = forced.then(|| st.new_flow(credential_ref));
        let (codes_tx, codes_rx) = mpsc::channel(4);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        st.running = Some(Running {
            credential_ref: credential_ref.to_owned(),
            forced,
            flow_id: flow.as_ref().map(|(id, _)| *id),
            codes: codes_tx,
            cancel: cancel_tx,
            stop: None,
        });
        let claim = Claim {
            credential_ref: credential_ref.to_owned(),
            forced,
            flow: flow.as_ref().map(|(_, state)| state.clone()),
            control: Control {
                codes: codes_rx,
                cancel: cancel_rx,
            },
        };
        let keeper = self.clone();
        st.attempt = Some(tokio::spawn(async move { keeper.attempt(claim).await }));
        flow.map(|(id, _)| id)
    }

    async fn validate_all(&self) {
        let refs: Vec<String> = self.lock().entries.keys().cloned().collect();
        for credential_ref in refs {
            self.check_and_record(&credential_ref).await;
            let mut st = self.lock();
            let busy = running_for(&st, &credential_ref);
            if let Some(e) = st.entries.get_mut(&credential_ref) {
                if e.health.wants_login() && !busy {
                    e.schedule.report(Instant::now());
                }
            }
            drop(st);
            self.inner.wake.notify_one();
        }
    }

    /// Check `credential_ref` and record the answer. Returns the check and
    /// the credential's health afterwards, which is not the check's answer
    /// if something newer changed it while the check ran.
    async fn check_and_record(&self, credential_ref: &str) -> (Check, Option<Health>) {
        let epoch = self.lock().entries.get(credential_ref).map(|e| e.epoch);
        let check = self.check(credential_ref).await;
        let health = epoch.and_then(|epoch| self.record(credential_ref, &check, epoch));
        (check, health)
    }

    /// Store a check's result, unless the health changed since the check
    /// started (`epoch`). A failed check changes nothing. A healthy one
    /// clears the schedule, deletes a kept login (the pool's own material
    /// works, and the kept refresh token may be stale by now) and stops an
    /// automatic attempt for the credential. Returns the health now.
    fn record(&self, credential_ref: &str, check: &Check, epoch: u64) -> Option<Health> {
        let health = match check {
            Check::Unknown(detail) => {
                tracing::warn!(event = "credential_checked", credential_ref, outcome = "unknown", detail = %detail);
                return self
                    .lock()
                    .entries
                    .get(credential_ref)
                    .map(|e| e.health.clone());
            }
            Check::Healthy => Health::Healthy,
            Check::Disabled => Health::Disabled,
            Check::RefreshRefused { reason, expires_at } => Health::RefreshRefused {
                reason: reason.clone(),
                expires_at: *expires_at,
            },
            Check::NeedsLogin(reason) => Health::NeedsLogin {
                reason: reason.clone(),
            },
        };
        match check {
            Check::RefreshRefused { reason, expires_at } => {
                tracing::warn!(event = "credential_checked", credential_ref, outcome = "refresh_refused",
                    expires_at = %expires_at.to_rfc3339(), reason = %reason);
            }
            Check::NeedsLogin(reason) => {
                tracing::warn!(event = "credential_checked", credential_ref, outcome = "needs_login", reason = %reason);
            }
            Check::Healthy => tracing::info!(
                event = "credential_checked",
                credential_ref,
                outcome = "healthy"
            ),
            _ => tracing::info!(
                event = "credential_checked",
                credential_ref,
                outcome = "disabled"
            ),
        }
        let mut st = self.lock();
        let entry = st.entries.get_mut(credential_ref)?;
        if entry.epoch != epoch {
            tracing::info!(
                event = "check_superseded",
                credential_ref,
                "the health changed while the check ran; its answer is dropped"
            );
            return Some(entry.health.clone());
        }
        entry.checked_at = Some(Utc::now());
        entry.health = health.clone();
        entry.epoch += 1;
        if matches!(check, Check::Healthy) {
            let pending = self.isolation().pending_file(credential_ref);
            if std::fs::remove_file(&pending).is_ok() {
                tracing::info!(
                    event = "pending_login_discarded",
                    credential_ref,
                    "the credential is healthy"
                );
            }
        }
        if matches!(check, Check::Healthy | Check::Disabled) {
            healed(&mut st, credential_ref);
        }
        Some(health)
    }

    /// Ask the daemon about the credential; refresh an OAuth credential
    /// that expires within `refresh_ahead`, unless the token endpoint
    /// already refused the refresh token of this same material.
    async fn check(&self, credential_ref: &str) -> Check {
        let admin = &self.inner.admin;
        let meta = match admin.show(credential_ref).await {
            Ok(meta) => meta,
            Err(e) => return Check::Unknown(e.to_string()),
        };
        if meta.source != CredentialSource::Pool {
            tracing::warn!(
                event = "not_pool_managed",
                credential_ref,
                "a config-sourced credential cannot be rotated through the admin API"
            );
        }
        match meta.status {
            CredentialStatus::Invalid => {
                return Check::NeedsLogin(
                    meta.last_error
                        .unwrap_or_else(|| "the daemon marked it invalid".into()),
                )
            }
            CredentialStatus::Disabled => return Check::Disabled,
            CredentialStatus::Active | CredentialStatus::RateLimited => {}
        }
        let Some(expires_at) = meta
            .expires_at
            .filter(|_| meta.kind == CredentialKind::Oauth)
        else {
            return Check::Healthy;
        };
        // The same material (same expiry) whose refresh was refused: asking
        // the vendor again cannot help. A rotation changes the expiry.
        let known_refusal = match &self.lock().entries.get(credential_ref).map(|e| &e.health) {
            Some(Health::RefreshRefused {
                reason,
                expires_at: at,
            }) if *at == expires_at => Some(reason.clone()),
            _ => None,
        };
        if let Some(reason) = known_refusal {
            return Check::RefreshRefused { reason, expires_at };
        }
        let ahead = chrono::Duration::from_std(self.inner.config.refresh_ahead)
            .unwrap_or(chrono::Duration::MAX);
        if expires_at - Utc::now() > ahead {
            return Check::Healthy;
        }
        match admin.refresh(credential_ref).await {
            Ok(_) => {
                tracing::info!(event = "credential_refreshed", credential_ref);
                Check::Healthy
            }
            Err(e) if e.is_refresh_refused() => Check::RefreshRefused {
                reason: e.to_string(),
                expires_at,
            },
            Err(e) => Check::Unknown(e.to_string()),
        }
    }

    /// One attempt. The flow slot is released when it ends, also if it
    /// panics.
    async fn attempt(&self, claim: Claim) {
        let _release = Release {
            keeper: self.clone(),
            credential_ref: claim.credential_ref.clone(),
        };
        #[cfg(test)]
        if self
            .inner
            .panic_next_attempt
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            panic!("injected panic");
        }
        self.attempt_claimed(claim).await;
    }

    async fn attempt_claimed(&self, claim: Claim) {
        let credential_ref = claim.credential_ref.as_str();
        // Always ask the daemon: an event's word, or an older check, may
        // be out of date. A newer event's word stands over the answer.
        let (check, health) = self.check_and_record(credential_ref).await;
        if !claim.forced {
            if let Check::Unknown(_) = check {
                // Without the admin surface a login could not be rotated
                // in; try again after a backoff.
                self.failed(credential_ref, true);
                return;
            }
            if !health.is_some_and(|h| h.wants_login()) {
                return;
            }
            // A login an earlier flow for this credential finished but never
            // rotated in (the keeper stopped, or the rotation failed). An
            // operator's start always runs a fresh flow.
            let pending = self.isolation().pending_file(credential_ref);
            if let Ok(material) = Material::read(&pending) {
                tracing::info!(event = "pending_login_found", credential_ref);
                if self.rotate(credential_ref, &material).await.is_ok() {
                    self.mark_last_flow_rotated(credential_ref);
                }
                // On failure the rotation backed off; the next attempt
                // retries the file (or, if the pool refused it, a flow).
                return;
            }
        }
        self.flow(claim).await;
    }

    /// The latest flow for `credential_ref` failed only at its rotation,
    /// and that login is in the pool now.
    fn mark_last_flow_rotated(&self, credential_ref: &str) {
        let st = self.lock();
        if let Some(f) = st
            .flows
            .iter()
            .rev()
            .find(|f| f.credential_ref == credential_ref)
        {
            if matches!(&*f.state.borrow(), LoginState::Failed { .. }) {
                f.state
                    .send_replace(LoginState::Succeeded { at: Utc::now() });
            }
        }
    }

    /// Run a login flow and rotate its result in.
    async fn flow(&self, mut claim: Claim) {
        let credential_ref = claim.credential_ref.as_str();
        let state = match claim.flow.take() {
            Some(state) => state,
            None => {
                let mut st = self.lock();
                let (id, state) = st.new_flow(credential_ref);
                if let Some(running) = st.running.as_mut() {
                    running.flow_id = Some(id);
                }
                state
            }
        };
        let outcome = if *claim.control.cancel.borrow() {
            Outcome::Cancelled
        } else {
            self.inner
                .login
                .run(credential_ref, &state, claim.control)
                .await
        };
        let end = match outcome {
            Outcome::Authorized(material) => {
                tracing::info!(
                    event = "login_authorized",
                    credential_ref,
                    expires_at = material.expires_at().map(|t| t.to_rfc3339())
                );
                if let Err(e) = self.isolation().keep_pending(credential_ref) {
                    tracing::error!(event = "pending_login_not_kept", credential_ref, error = %e);
                }
                match self.rotate(credential_ref, &material).await {
                    Ok(()) => LoginState::Succeeded { at: Utc::now() },
                    Err(e) => LoginState::Failed {
                        reason: format!("logged in, but the rotation failed: {e}"),
                    },
                }
            }
            Outcome::Failed(reason) => {
                tracing::warn!(event = "login_failed", credential_ref, reason = %reason);
                self.failed(credential_ref, true);
                LoginState::Failed { reason }
            }
            Outcome::Cancelled => {
                let stop = {
                    let st = self.lock();
                    st.running.as_ref().and_then(|r| r.stop)
                }
                .unwrap_or(Stop::Shutdown);
                tracing::info!(
                    event = "login_cancelled",
                    credential_ref,
                    reason = stop.reason()
                );
                if stop == Stop::Operator {
                    self.pause(credential_ref);
                }
                LoginState::Cancelled {
                    reason: stop.reason().to_owned(),
                }
            }
        };
        state.send_replace(end);
    }

    /// Push `material` into the pool. On success the pending file is
    /// deleted, so the pool is the only holder of the new refresh token. On
    /// failure the attempt backs off; the file is kept unless the daemon
    /// refused the material itself (`400 invalid_material`), which no retry
    /// changes. Any other answer (a 401, a 409, a 5xx, no answer) keeps it.
    async fn rotate(&self, credential_ref: &str, material: &Material) -> Result<(), AdminError> {
        let pending = self.isolation().pending_file(credential_ref);
        match self.inner.admin.rotate(credential_ref, material).await {
            Ok(meta) => {
                let _ = std::fs::remove_file(&pending);
                tracing::info!(event = "credential_rotated", credential_ref,
                    status = ?meta.status, expires_at = meta.expires_at.map(|t| t.to_rfc3339()));
                let mut st = self.lock();
                if let Some(e) = st.entries.get_mut(credential_ref) {
                    e.health = Health::Healthy;
                    e.checked_at = Some(Utc::now());
                    e.epoch += 1;
                    e.schedule.succeeded();
                }
                Ok(())
            }
            Err(e) => {
                tracing::error!(event = "rotation_failed", credential_ref, error = %e);
                if matches!(&e, AdminError::Api { status: 400, code, .. } if code == "invalid_material")
                {
                    let _ = std::fs::remove_file(&pending);
                    tracing::warn!(event = "pending_login_discarded", credential_ref);
                }
                self.failed(credential_ref, true);
                Err(e)
            }
        }
    }

    /// Back off; with `retry`, the next attempt is pending after the wait.
    fn failed(&self, credential_ref: &str, retry: bool) {
        let mut st = self.lock();
        if let Some(e) = st.entries.get_mut(credential_ref) {
            let now = Instant::now();
            let wait = e.schedule.failed(now);
            if retry {
                e.schedule.report(now);
            }
            tracing::info!(
                event = "backoff",
                credential_ref,
                wait_secs = wait.as_secs(),
                failed_flows = e.schedule.failures(),
                retry
            );
        }
    }

    /// The operator cancelled: no automatic flow for `backoff_max`, and
    /// none until the next problem report or check.
    fn pause(&self, credential_ref: &str) {
        let pause = self.inner.config.backoff_max;
        if let Some(e) = self.lock().entries.get_mut(credential_ref) {
            e.schedule.paused(Instant::now(), pause);
        }
        tracing::info!(
            event = "automatic_flows_paused",
            credential_ref,
            pause_secs = pause.as_secs()
        );
    }
}

/// An attempt for `credential_ref` holds the flow slot.
fn running_for(st: &State, credential_ref: &str) -> bool {
    st.running
        .as_ref()
        .is_some_and(|r| r.credential_ref == credential_ref)
}

/// Tell the running attempt to stop. `false` when none runs.
fn stop_running(st: &mut State, stop: Stop) -> bool {
    match st.running.as_mut() {
        Some(running) => {
            running.stop.get_or_insert(stop);
            running.cancel.send_replace(true);
            true
        }
        None => false,
    }
}

/// Frees the flow slot when an attempt ends, however it ends. A flow left
/// running by a panic is marked failed. If the credential still wants a
/// login and nothing is scheduled (a report that came while the attempt
/// ran), an attempt is scheduled; the backoff and an operator's hold apply.
struct Release {
    keeper: Keeper,
    credential_ref: String,
}

impl Drop for Release {
    fn drop(&mut self) {
        let credential_ref = self.credential_ref.as_str();
        let mut st = self.keeper.lock();
        let flow_id = st.running.take().and_then(|r| r.flow_id);
        st.attempt = None;
        if let Some(f) = st.flows.iter().find(|f| Some(f.id) == flow_id) {
            if f.state.borrow().is_running() {
                f.state.send_replace(LoginState::Failed {
                    reason: "the attempt ended unexpectedly".into(),
                });
            }
        }
        if std::thread::panicking() {
            tracing::error!(event = "attempt_panicked", credential_ref);
        }
        let shutting_down = st.shutting_down;
        if let Some(e) = st.entries.get_mut(credential_ref) {
            if e.health.wants_login() && e.schedule.due().is_none() && !shutting_down {
                e.schedule.report(Instant::now());
            }
        }
        drop(st);
        self.keeper.inner.wake.notify_one();
        tracing::debug!(event = "attempt_ended", credential_ref);
    }
}

/// `credential_ref` works: forget its failures and pending reports, and
/// stop an automatic attempt for it.
fn healed(st: &mut State, credential_ref: &str) {
    if let Some(e) = st.entries.get_mut(credential_ref) {
        e.schedule.succeeded();
    }
    let automatic = st
        .running
        .as_ref()
        .is_some_and(|r| r.credential_ref == credential_ref && !r.forced);
    if automatic && stop_running(st, Stop::Healed) {
        tracing::info!(
            event = "flow_superseded",
            credential_ref,
            "the credential is usable again"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::Address;
    use std::time::Duration;

    async fn until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < deadline, "never: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Review finding (round 2) 5: a panic in an attempt frees the flow
    /// slot and ends its flow as failed.
    #[tokio::test]
    async fn a_panicking_attempt_frees_the_slot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let text = format!(
            "socket = {:?}\nstate_dir = {:?}\nclaude_bin = \"/usr/bin/false\"\n\
             [[credential]]\nref = \"sub\"\ninstances = [\"claude\"]\n",
            dir.path().join("keeper.sock").display().to_string(),
            dir.path().join("state").display().to_string(),
        );
        let keeper = Keeper::new(
            Config::parse(&text).expect("config"),
            Admin::new(Address::Unix(dir.path().join("no-admin.sock")), None),
        );
        keeper
            .inner
            .panic_next_attempt
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(keeper.start("sub"), Ok(1));
        until("the slot is free", || keeper.running_credential().is_none()).await;
        assert!(matches!(
            &*keeper.lock().flows[0].state.borrow(),
            LoginState::Failed { .. }
        ));
        assert_eq!(keeper.start("sub"), Ok(2), "the slot is free again");
        until("the second attempt ends", || {
            keeper.running_credential().is_none()
        })
        .await;
    }
}
