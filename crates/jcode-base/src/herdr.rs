// cspell:ignore herdr
//! Native Herdr lifecycle reporting.
//!
//! [Herdr](https://herdr.dev) is a terminal workspace manager for coding
//! agents. When a jcode session runs inside a Herdr pane, Herdr tracks the
//! pane's agent lifecycle (`working` / `idle` / `blocked`) and restores
//! panes with the agent's native session id. Without a reporter, Herdr can
//! only screen-scrape the pane for an agent it knows — and knows nothing of
//! jcode.
//!
//! This module is the emitter side of that contract: it speaks Herdr's
//! socket API (`pane.report_agent`, `pane.report_agent_session`,
//! `pane.release_agent`) with newline-delimited JSON, like the integrations
//! Herdr installs for Pi or OpenCode, but lives inside jcode so transitions
//! come from real runtime events instead of shell-hook configuration.
//!
//! Wiring (three seams, all cheap no-ops outside Herdr):
//! - `hooks::dispatch_observer` funnels every lifecycle event
//!   (`session_start` / `turn_start` / `turn_end` / `session_end`) through
//!   [`report_observer_event`]. The shared daemon runs dispatch under the
//!   owning client's terminal env (`with_client_terminal_env`), so reports
//!   stay per-pane correct.
//! - `safety::SafetySystem::request_permission` calls
//!   [`report_permission_queued`] so a mid-turn permission prompt shows as
//!   `blocked` immediately instead of only at turn end.
//! - decision paths (`record_decision`, expiry, file-based replies) call
//!   [`report_permission_resolved`].
//!
//! State model: a session publishes `working` during a turn and `idle` when
//! the turn settles, unless it has unresolved permission requests, which pin
//! `blocked`. A new turn implies the user interacted with the session, so
//! stale pins (decided out-of-band: `jcode permissions`, email/telegram
//! reply, another process) clear on `turn_start`. `session_end` releases
//! authority so Herdr never shows a dead agent's state.
//!
//! Gating: off unless `HERDR_ENV=1`, `HERDR_SOCKET_PATH`, and
//! `HERDR_PANE_ID` are present in the *client* terminal environment for the
//! current request (shared daemon: per-pane via the request scope; single
//! process: the env itself). `JCODE_HERDR_REPORT=0` disables reporting
//! unconditionally; the hook recursion guard (`JCODE_HOOKS_DISABLED`) also
//! suppresses it so hook processes that invoke jcode never emit nested
//! reports.
//!
//! Delivery: one NDJSON request per fresh Unix-socket connection on a
//! detached spawned task with short timeouts (the pi integration's
//! protocol). Failures are logged at debug level only: reporting can never
//! block or break the agent.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Stable source identifier; Herdr orders and deduplicates reports per source.
const SOURCE: &str = "herdr:jcode";
/// Agent label displayed by Herdr for reporting panes.
const AGENT: &str = "jcode";

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const IO_TIMEOUT: Duration = Duration::from_millis(1500);

/// Lifecycle state as understood by Herdr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HerdrState {
    Working,
    Idle,
    Blocked,
}

impl HerdrState {
    fn as_str(self) -> &'static str {
        match self {
            HerdrState::Working => "working",
            HerdrState::Idle => "idle",
            HerdrState::Blocked => "blocked",
        }
    }
}

/// Pane identity inherited from the Herdr client that owns a session.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PaneIdentity {
    socket_path: String,
    pane_id: String,
}

/// Per-session reporter state (a jcode session lives in at most one pane).
#[derive(Debug)]
struct PaneReporter {
    pane: PaneIdentity,
    seq: u64,
    last_state: Option<HerdrState>,
    last_message: Option<String>,
    /// Whether a turn is currently running. Decides whether an unblocked
    /// session returns to `working` or `idle`.
    turn_active: bool,
    /// Permission requests queued for this session awaiting a user decision,
    /// as `(request_id, label)` in arrival order. Non-empty pins the state
    /// to `blocked`.
    pending_permissions: Vec<(String, String)>,
}

impl PaneReporter {
    fn new(pane: PaneIdentity) -> Self {
        Self {
            pane,
            seq: initial_seq(),
            last_state: None,
            last_message: None,
            turn_active: false,
            pending_permissions: Vec::new(),
        }
    }
}

static STATE: OnceLock<Mutex<HashMap<String, PaneReporter>>> = OnceLock::new();

fn state() -> &'static Mutex<HashMap<String, PaneReporter>> {
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reverse map: permission request id -> owning session id, for decisions
/// observed in this process (which only know the request id).
static PERMISSION_OWNERS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn permission_owners() -> &'static Mutex<HashMap<String, String>> {
    PERMISSION_OWNERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn reporting_disabled() -> bool {
    if std::env::var_os("JCODE_HOOKS_DISABLED").is_some() {
        return true;
    }
    matches!(
        std::env::var("JCODE_HERDR_REPORT").ok().as_deref(),
        Some("0") | Some("off") | Some("false")
    )
}

/// True when Herdr's own jcode hook adapter is installed (upstream
/// `herdr integration install jcode` appends a `herdr-agent-state` command
/// to `[hooks] session_start`). The adapter reports under this same
/// `herdr:jcode` source; two reporters would fight over the source's seq
/// ramp and drop each other's updates, so the adapter (explicitly installed
/// by the user) wins and the native emitter stays silent. Checked through
/// `hook_commands`, so config reloads flip it without a restart.
fn hook_adapter_installed() -> bool {
    crate::hooks::hook_commands("session_start")
        .iter()
        .any(|command| command.contains("herdr-agent-state"))
}

/// Monotonic floor so a restart can never reuse a Herdr seq number.
fn initial_seq() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(1)
}

fn request_id(kind: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("herdr:jcode:{kind}:{nanos}")
}

/// Whether this process is a shared daemon (never launched inside a Herdr
/// pane itself; clients carry pane identity via their request scope). Set
/// once by the server bootstrap so the process-env fallback below can be
/// disabled without a new protocol field.
static SHARED_DAEMON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark this process as a shared server daemon.
pub fn mark_shared_daemon() {
    SHARED_DAEMON.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Read the caller's Herdr pane identity: first from the request-scoped
/// client terminal env (shared daemon), falling back to the process env only
/// when no scoped env is active at all (single-process launch). A scoped
/// env without `HERDR_ENV=1` means this client is not inside Herdr and the
/// daemon's inherited vars must not leak into its pane's reports. A daemon
/// process never consults its own env: work outside a client scope has no
/// displayable pane.
fn current_pane() -> Option<PaneIdentity> {
    if reporting_disabled() || hook_adapter_installed() {
        return None;
    }
    let (herdr_env, socket_path, pane_id) = if crate::hooks::has_client_terminal_env() {
        (
            crate::hooks::client_terminal_env_entry("HERDR_ENV"),
            crate::hooks::client_terminal_env_entry("HERDR_SOCKET_PATH"),
            crate::hooks::client_terminal_env_entry("HERDR_PANE_ID"),
        )
    } else if SHARED_DAEMON.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    } else {
        (
            std::env::var("HERDR_ENV").ok(),
            std::env::var("HERDR_SOCKET_PATH").ok(),
            std::env::var("HERDR_PANE_ID").ok(),
        )
    };
    if herdr_env.as_deref() != Some("1") {
        return None;
    }
    let socket_path = socket_path.unwrap_or_default();
    let pane_id = pane_id.unwrap_or_default();
    if socket_path.is_empty() || pane_id.is_empty() {
        return None;
    }
    Some(PaneIdentity {
        socket_path,
        pane_id,
    })
}

/// Whether Herdr is attached to the current request scope's pane. Runtime
/// fast paths call this (alongside their own session-id gate) to keep
/// dispatching lifecycle events when no shell hook is configured but a
/// Herdr client is watching. Pane attribution is inherently correct because
/// the shared daemon runs dispatch inside the owning client's terminal-env
/// scope (`hooks::with_client_terminal_env`).
pub fn active() -> bool {
    current_pane().is_some()
}

/// Whether this event's lifecycle transitions feed Herdr reports. `pre_tool`
/// and `post_tool` are deliberately excluded: they are hot paths and carry
/// no pane-state meaning.
pub fn consumes_event(event: &str) -> bool {
    matches!(
        event,
        "session_start" | "session_end" | "turn_start" | "turn_end"
    ) && active()
}

/// Deliver one NDJSON request to the Herdr socket, reading a response byte
/// before dropping the connection so the request is never truncated.
async fn deliver(socket_path: String, payload: String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;

    let result = async {
        let stream =
            tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&socket_path)).await??;
        let (mut reader, mut writer) = stream.into_split();
        tokio::time::timeout(IO_TIMEOUT, async {
            writer.write_all(payload.as_bytes()).await?;
            writer.flush().await
        })
        .await??;
        let mut buf = [0u8; 512];
        let _ = tokio::time::timeout(IO_TIMEOUT, reader.read(&mut buf)).await;
        std::io::Result::Ok(())
    }
    .await;
    if let Err(error) = result {
        crate::logging::debug(&format!("Herdr report to {socket_path} failed: {error}"));
    }
}

/// Spawn a best-effort report task. Silently skips outside a tokio runtime
/// (e.g. CLI pre-launch code paths) so reporting can never panic.
fn spawn_report(socket_path: String, payload: String) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        deliver(socket_path, payload).await;
    });
}

/// Send a session-identity report for `pane`, letting Herdr restore the pane
/// with `jcode --resume <session_id>` after a restart.
fn send_session_report(pane: &PaneIdentity, session_id: &str, start_source: Option<&str>) {
    let mut params = serde_json::Map::new();
    params.insert("pane_id".into(), serde_json::json!(pane.pane_id));
    params.insert("source".into(), serde_json::json!(SOURCE));
    params.insert("agent".into(), serde_json::json!(AGENT));
    params.insert("agent_session_id".into(), serde_json::json!(session_id));
    if let Some(start_source) = start_source {
        params.insert(
            "session_start_source".into(),
            serde_json::json!(start_source),
        );
    }
    let request = serde_json::json!({
        "id": request_id("session"),
        "method": "pane.report_agent_session",
        "params": serde_json::Value::Object(params),
    });
    spawn_report(pane.socket_path.clone(), format!("{request}\n"));
}

/// Begin tracking `session_id` in the caller's Herdr pane: report session
/// identity (so Herdr can restore with `jcode --resume <id>`) and publish
/// the derived state. Re-attach (client reconnect to the same session,
/// possibly from a new pane) refreshes the pane identity; permission pins
/// survive because a reconnect does not answer an outstanding prompt. The
/// dedup cache resets so the pane always gets a fresh report, and the seq
/// ramp stays monotonic.
pub fn attach_session(session_id: &str) {
    attach_session_with_source(session_id, None);
}

/// Internal attach with an explicit Herdr `session_start_source`
/// (`startup` / `resume`) for restore bookkeeping.
fn attach_session_with_source(session_id: &str, start_source: Option<&str>) {
    let Some(pane) = current_pane() else {
        return;
    };
    // One pane hosts one displayed agent. A second session attaching to an
    // already-owned pane is a headless session (swarm workers are created
    // inside the spawning client's request scope) or a takeover race; it has
    // no visible UI of its own, and publishing for it would fight the
    // visible session's state (a worker running while the coordinator idles
    // must not flip the pane to working). Normal client switches release
    // via session_end before the next attach, so skipping keeps the visible
    // owner authoritative.
    let owned_by_other = {
        let Ok(map) = state().lock() else {
            return;
        };
        map.iter()
            .any(|(other, reporter)| other != session_id && reporter.pane == pane)
    };
    if owned_by_other {
        return;
    }
    send_session_report(&pane, session_id, start_source);
    {
        let Ok(mut map) = state().lock() else {
            return;
        };
        match map.get_mut(session_id) {
            Some(existing) => {
                existing.pane = pane.clone();
                existing.last_state = None;
                existing.last_message = None;
            }
            None => {
                map.insert(session_id.to_string(), PaneReporter::new(pane.clone()));
            }
        }
    }
    publish_state(session_id);
}

/// Stop tracking `session_id` and release lifecycle authority on its pane so
/// Herdr clears the agent label instead of showing a stale state.
pub fn detach_session(session_id: &str) {
    let removed = {
        let Ok(mut map) = state().lock() else {
            return;
        };
        match map.remove(session_id) {
            Some(mut reporter) => {
                if let Ok(mut owners) = permission_owners().lock() {
                    for (request_id, _) in &reporter.pending_permissions {
                        owners.remove(request_id);
                    }
                }
                reporter.seq = reporter.seq.saturating_add(1);
                let request = serde_json::json!({
                    "id": request_id("release"),
                    "method": "pane.release_agent",
                    "params": {
                        "pane_id": reporter.pane.pane_id,
                        "source": SOURCE,
                        "agent": AGENT,
                        "seq": reporter.seq,
                    }
                });
                Some((reporter.pane.socket_path, format!("{request}\n")))
            }
            None => None,
        }
    };
    if let Some((socket_path, payload)) = removed {
        spawn_report(socket_path, payload);
    }
}

/// Recompute the state a session should publish and send it. Deduplicated
/// against the last published (state, message) pair per pane, like Herdr's
/// pi integration.
fn publish_state(session_id: &str) {
    let outgoing = {
        let Ok(mut map) = state().lock() else {
            return;
        };
        let Some(reporter) = map.get_mut(session_id) else {
            return;
        };
        let (state, message) = derive_state(reporter);
        if reporter.last_state == Some(state) && reporter.last_message == message {
            return;
        }
        reporter.last_state = Some(state);
        reporter.last_message = message.clone();
        reporter.seq = reporter.seq.saturating_add(1);
        let mut params = serde_json::Map::new();
        params.insert("pane_id".into(), serde_json::json!(reporter.pane.pane_id));
        params.insert("source".into(), serde_json::json!(SOURCE));
        params.insert("agent".into(), serde_json::json!(AGENT));
        params.insert("state".into(), serde_json::json!(state.as_str()));
        params.insert("seq".into(), serde_json::json!(reporter.seq));
        if let Some(message) = &message {
            params.insert("message".into(), serde_json::json!(message));
        }
        let request = serde_json::json!({
            "id": request_id("state"),
            "method": "pane.report_agent",
            "params": serde_json::Value::Object(params),
        });
        (reporter.pane.socket_path.clone(), format!("{request}\n"))
    };
    spawn_report(outgoing.0, outgoing.1);
}

fn derive_state(reporter: &PaneReporter) -> (HerdrState, Option<String>) {
    let (state, message) = match reporter.pending_permissions.first() {
        Some((_, label)) => {
            let extra = reporter.pending_permissions.len() - 1;
            if extra > 0 {
                (
                    HerdrState::Blocked,
                    Some(format!("{label} (+{extra} more)")),
                )
            } else {
                (HerdrState::Blocked, Some(label.clone()))
            }
        }
        None if reporter.turn_active => (HerdrState::Working, None),
        None => (HerdrState::Idle, None),
    };
    (state, message)
}

fn is_tracked(session_id: &str) -> bool {
    state()
        .lock()
        .map(|map| map.contains_key(session_id))
        .unwrap_or(false)
}

/// Track a session observed without a prior attach (e.g. a session created
/// by an older server build or restored without a session_start event).
/// The caller has just mutated turn/pin state, so the derived state is
/// already correct; no-op for unattached sessions outside Herdr.
fn ensure_tracked(session_id: &str) {
    if !is_tracked(session_id) && current_pane().is_some() {
        attach_session(session_id);
    }
}

/// Translate a dispatched lifecycle hook event into Herdr reports. This is
/// the single integration point for turn/session transitions; the caller is
/// `hooks::dispatch_observer`, which runs under the owning client's
/// terminal env on the shared daemon.
pub fn report_observer_event(event: &crate::hooks::HookEvent) {
    let Some(session_id) = event.session_id.as_deref() else {
        return;
    };
    match event.event {
        "session_start" => {
            let start_source = match event.field_value("SOURCE") {
                Some("resume") => Some("resume"),
                Some("create") | Some("attach") => Some("startup"),
                _ => None,
            };
            attach_session_with_source(session_id, start_source);
        }
        "turn_start" => {
            // Sessions observed without a prior attach adopt their pane now;
            // mutation below then reflects this event's semantics.
            ensure_tracked(session_id);
            {
                let Ok(mut map) = state().lock() else {
                    return;
                };
                if let Some(reporter) = map.get_mut(session_id) {
                    // User activity answered any outstanding prompt (possibly
                    // out-of-band); drop stale pins before recomputing.
                    if !reporter.pending_permissions.is_empty()
                        && let Ok(mut owners) = permission_owners().lock()
                    {
                        for (request_id, _) in reporter.pending_permissions.drain(..) {
                            owners.remove(&request_id);
                        }
                    }
                    reporter.turn_active = true;
                }
            }
            publish_state(session_id);
        }
        "turn_end" => {
            ensure_tracked(session_id);
            {
                let Ok(mut map) = state().lock() else {
                    return;
                };
                if let Some(reporter) = map.get_mut(session_id) {
                    reporter.turn_active = false;
                }
            }
            publish_state(session_id);
        }
        "session_end" => {
            detach_session(session_id);
        }
        _ => {}
    }
}

/// A permission request was queued awaiting a user decision (called from
/// `SafetySystem::request_permission` under the requesting client's scope
/// when available). Sessions with no pane to display them in are ignored.
pub fn report_permission_queued(request: &crate::safety::PermissionRequest) {
    let Some(session_id) = crate::safety::permission_request_session_id(request) else {
        return;
    };
    // Only pin sessions already tracked with a pane: a permission request
    // arriving here mid-turn means its lifecycle funnel ran. Never adopt
    // unknown sessions — on the shared daemon their pane is unknowable
    // (ambient/headless have no client scope) and misattribution is worse
    // than silence.
    if !is_tracked(&session_id) {
        return;
    }
    let label = truncate_label(&format!("{}: {}", request.action, request.description));
    {
        let Ok(mut map) = state().lock() else {
            return;
        };
        let Some(reporter) = map.get_mut(&session_id) else {
            return;
        };
        if !reporter
            .pending_permissions
            .iter()
            .any(|(id, _)| id == &request.id)
        {
            reporter
                .pending_permissions
                .push((request.id.clone(), label));
        }
    }
    if let Ok(mut owners) = permission_owners().lock() {
        owners.insert(request.id.clone(), session_id.clone());
    }
    publish_state(&session_id);
}

/// Router registered via `safety::register_permission_observer` at startup:
/// permission lifecycle events feed the Herdr blocked pins.
pub fn observe_permission_event(event: crate::safety::PermissionEvent) {
    match event {
        crate::safety::PermissionEvent::Queued(request) => report_permission_queued(request),
        crate::safety::PermissionEvent::Resolved { request_id } => {
            report_permission_resolved(request_id);
        }
    }
}

/// A permission request observed as queued by this process was decided
/// (approved/denied/expired). Cross-process decisions are covered by the
/// pin-clear on the next `turn_start`.
pub fn report_permission_resolved(permission_request_id: &str) {
    let owner = {
        let Ok(mut owners) = permission_owners().lock() else {
            return;
        };
        owners.remove(permission_request_id)
    };
    let Some(session_id) = owner else {
        return;
    };
    {
        let Ok(mut map) = state().lock() else {
            return;
        };
        let Some(reporter) = map.get_mut(&session_id) else {
            return;
        };
        reporter
            .pending_permissions
            .retain(|(id, _)| id != permission_request_id);
    }
    publish_state(&session_id);
}

fn truncate_label(label: &str) -> String {
    const LIMIT: usize = 120;
    let trimmed = label.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    let mut kept: String = trimmed.chars().take(LIMIT - 1).collect();
    kept.push('…');
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unique_session(prefix: &str) -> String {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        format!(
            "ses_herdrtest_{prefix}_{}",
            COUNTER.fetch_add(1, Ordering::SeqCst)
        )
    }

    fn set_herdr_env(socket: &std::path::Path) {
        unsafe {
            std::env::set_var("HERDR_ENV", "1");
            std::env::set_var("HERDR_SOCKET_PATH", socket);
            std::env::set_var("HERDR_PANE_ID", "w1:p-test");
            std::env::remove_var("JCODE_HERDR_REPORT");
            std::env::remove_var("JCODE_HOOKS_DISABLED");
        }
    }

    fn clear_herdr_env() {
        for key in [
            "HERDR_ENV",
            "HERDR_SOCKET_PATH",
            "HERDR_PANE_ID",
            "JCODE_HERDR_REPORT",
            "JCODE_HOOKS_DISABLED",
        ] {
            unsafe { std::env::remove_var(key) };
        }
    }

    /// Fake Herdr server: accept connections and buffer every NDJSON request.
    /// Reports travel on independent connections, so arrival order between
    /// requests is not guaranteed; assertions match on method (+ seq) instead
    /// of adjacency, hence the queue.
    struct FakeHerdr {
        socket: std::path::PathBuf,
        requests: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<serde_json::Value>>>,
        _dir: tempfile::TempDir,
    }

    impl FakeHerdr {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().expect("temp dir");
            let socket = dir.path().join("herdr.sock");
            let listener = tokio::net::UnixListener::bind(&socket).expect("bind fake herdr socket");
            let requests: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<_>>> =
                std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
            let sink = requests.clone();
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let sink = sink.clone();
                    tokio::spawn(async move {
                        use tokio::io::AsyncBufReadExt;
                        let mut reader = tokio::io::BufReader::new(stream);
                        if let Ok(Some(line)) = reader.lines().next_line().await {
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                                if let Ok(mut queue) = sink.lock() {
                                    queue.push_back(value);
                                }
                            }
                        }
                    });
                }
            });
            FakeHerdr {
                socket,
                requests,
                _dir: dir,
            }
        }

        fn drain_matching(
            &self,
            matches: impl Fn(&serde_json::Value) -> bool,
        ) -> Option<serde_json::Value> {
            let mut queue = self.requests.lock().expect("fake queue lock");
            let index = queue.iter().position(&matches)?;
            Some(queue.remove(index).expect("positioned index"))
        }

        /// Wait until a buffered request satisfies `matches` and claim it.
        fn wait_for(
            &self,
            what: &str,
            matches: impl Fn(&serde_json::Value) -> bool,
        ) -> serde_json::Value {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(request) = self.drain_matching(&matches) {
                    return request;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fake herdr never received: {what}"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        fn method(&self, method: &str) -> impl Fn(&serde_json::Value) -> bool + '_ {
            let method = method.to_string();
            move |request| request["method"] == method.as_str()
        }

        fn next_request_of(&self, method: &str) -> serde_json::Value {
            let named = method.to_string();
            self.wait_for(&named, self.method(method))
        }

        fn next_state(&self) -> serde_json::Value {
            self.next_request_of("pane.report_agent")
        }

        fn next_release(&self) -> serde_json::Value {
            self.next_request_of("pane.release_agent")
        }

        fn next_session(&self) -> serde_json::Value {
            self.next_request_of("pane.report_agent_session")
        }

        fn assert_no_request(&self) {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let queue = self.requests.lock().expect("fake queue lock");
            assert!(queue.is_empty(), "no request expected, got: {queue:?}");
        }
    }

    fn hook_event(
        event: &'static str,
        session_id: &str,
        fields: &[(&'static str, &'static str)],
    ) -> crate::hooks::HookEvent {
        let mut builder = crate::hooks::HookEvent::new(event).session_id(session_id);
        for (key, value) in fields {
            builder = builder.field(key, *value);
        }
        builder
    }

    fn permission_request(
        id: &str,
        session_id: &str,
        description: &str,
    ) -> crate::safety::PermissionRequest {
        crate::safety::permission_request_for_test(id, session_id, "bash", description)
    }

    fn set_turn_active(session_id: &str, active: bool) {
        let Ok(mut map) = state().lock() else {
            return;
        };
        if let Some(reporter) = map.get_mut(session_id) {
            reporter.turn_active = active;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_start_attach_reports_session_then_idle() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("attach");

        report_observer_event(&hook_event(
            "session_start",
            &session,
            &[("SOURCE", "create")],
        ));

        let session_report = fake.next_session();
        assert_eq!(session_report["method"], "pane.report_agent_session");
        assert_eq!(session_report["params"]["source"], SOURCE);
        assert_eq!(session_report["params"]["agent"], AGENT);
        assert_eq!(
            session_report["params"]["session_start_source"], "startup",
            "create should map to Herdr's startup source"
        );

        let state_report = fake.next_state();
        assert_eq!(state_report["method"], "pane.report_agent");
        assert_eq!(state_report["params"]["state"], "idle");
        assert_eq!(state_report["params"]["pane_id"], "w1:p-test");
        let first_seq = state_report["params"]["seq"].as_u64().unwrap();

        report_observer_event(&hook_event("session_end", &session, &[("SOURCE", "close")]));
        let release = fake.next_release();
        assert_eq!(release["method"], "pane.release_agent");
        assert!(release["params"]["seq"].as_u64().unwrap() > first_seq);
        assert!(!is_tracked(&session));
        clear_herdr_env();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_maps_session_start_source() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("resume");

        report_observer_event(&hook_event(
            "session_start",
            &session,
            &[("SOURCE", "resume")],
        ));
        let session_report = fake.next_request_of("pane.report_agent_session");
        assert_eq!(session_report["params"]["session_start_source"], "resume");
        fake.next_request_of("pane.report_agent"); // idle state
        report_observer_event(&hook_event("session_end", &session, &[("SOURCE", "close")]));
        clear_herdr_env();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_transitions_publish_working_then_idle() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("turn");

        attach_session(&session);
        fake.next_request_of("pane.report_agent_session");
        fake.next_request_of("pane.report_agent"); // idle

        report_observer_event(&hook_event("turn_start", &session, &[("MODEL", "test")]));
        assert_eq!(fake.next_state()["params"]["state"], "working");

        report_observer_event(&hook_event("turn_end", &session, &[("STATUS", "ok")]));
        assert_eq!(fake.next_state()["params"]["state"], "idle");

        // Dedup: repeating idle publishes nothing.
        report_observer_event(&hook_event("turn_end", &session, &[("STATUS", "ok")]));
        detach_session(&session);
        assert_eq!(fake.next_release()["method"], "pane.release_agent");
        clear_herdr_env();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permission_pins_blocked_until_resolved() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("permission");

        attach_session(&session);
        fake.next_request_of("pane.report_agent_session");
        fake.next_request_of("pane.report_agent");
        report_observer_event(&hook_event("turn_start", &session, &[]));
        fake.next_request_of("pane.report_agent"); // working

        report_permission_queued(&permission_request(
            "req_1",
            &session,
            "delete the database",
        ));
        let blocked = fake.next_state();
        assert_eq!(blocked["params"]["state"], "blocked");
        assert_eq!(blocked["params"]["message"], "bash: delete the database");

        // turn_end stays blocked while the pin is unresolved.
        report_observer_event(&hook_event("turn_end", &session, &[("STATUS", "ok")]));
        fake.assert_no_request();

        report_permission_resolved("req_1");
        let unpinned = fake.next_state();
        assert_eq!(unpinned["params"]["state"], "idle");

        detach_session(&session);
        clear_herdr_env();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unpin_returns_to_working_during_a_turn() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("unpin");

        attach_session(&session);
        fake.next_request_of("pane.report_agent_session");
        fake.next_request_of("pane.report_agent");
        report_observer_event(&hook_event("turn_start", &session, &[]));
        fake.next_request_of("pane.report_agent"); // working

        report_permission_queued(&permission_request(
            "req_u",
            &session,
            "delete the database",
        ));
        fake.next_request_of("pane.report_agent"); // blocked
        report_permission_resolved("req_u");
        let working = fake.next_state();
        assert_eq!(working["params"]["state"], "working");

        detach_session(&session);
        clear_herdr_env();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_turn_clears_stale_permission_pins() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("stale");

        attach_session(&session);
        fake.next_request_of("pane.report_agent_session");
        fake.next_request_of("pane.report_agent");
        report_permission_queued(&permission_request(
            "req_out",
            &session,
            "delete the database",
        ));
        fake.next_request_of("pane.report_agent"); // blocked

        // Decision made out-of-band (different process); the next user turn
        // must republish working without an explicit resolution signal.
        report_observer_event(&hook_event("turn_start", &session, &[]));
        let working = fake.next_state();
        assert_eq!(working["params"]["state"], "working");
        assert!(
            permission_owners().lock().unwrap().get("req_out").is_none(),
            "stale pin ownership must be dropped with the pin"
        );

        detach_session(&session);
        clear_herdr_env();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn second_pending_request_extends_blocked() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("two");

        attach_session(&session);
        fake.next_request_of("pane.report_agent_session");
        fake.next_request_of("pane.report_agent");
        report_permission_queued(&permission_request(
            "req_a",
            &session,
            "delete the database",
        ));
        fake.next_request_of("pane.report_agent"); // blocked req_a
        report_permission_queued(&permission_request("req_b", &session, "drop the schema"));
        let both = fake.next_state();
        assert_eq!(both["params"]["state"], "blocked");
        assert_eq!(
            both["params"]["message"], "bash: delete the database (+1 more)",
            "additional pending requests must surface in the label"
        );
        report_permission_resolved("req_a");
        let still = fake.next_state();
        assert_eq!(still["params"]["state"], "blocked");
        assert_eq!(still["params"]["message"], "bash: drop the schema");
        report_permission_resolved("req_b");
        assert_eq!(fake.next_state()["params"]["state"], "idle");

        detach_session(&session);
        clear_herdr_env();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permission_for_untracked_session_outside_herdr_is_noop() {
        let _guard = crate::storage::lock_test_env();
        clear_herdr_env();
        let session = unique_session("untracked");
        report_permission_queued(&permission_request(
            "req_n",
            &session,
            "delete the database",
        ));
        assert!(
            !is_tracked(&session),
            "no pane means no tracking, even for pins"
        );
        assert!(permission_owners().lock().unwrap().get("req_n").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn seams_are_noop_outside_herdr() {
        let _guard = crate::storage::lock_test_env();
        clear_herdr_env();
        let session = unique_session("noop");
        report_observer_event(&hook_event(
            "session_start",
            &session,
            &[("SOURCE", "create")],
        ));
        report_observer_event(&hook_event("turn_start", &session, &[]));
        report_observer_event(&hook_event("turn_end", &session, &[("STATUS", "ok")]));
        report_observer_event(&hook_event("session_end", &session, &[("SOURCE", "close")]));
        assert!(
            !is_tracked(&session),
            "sessions must not be tracked outside Herdr"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_switch_and_recursion_guard_disable_reporting() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("kill");

        unsafe { std::env::set_var("JCODE_HERDR_REPORT", "0") };
        report_observer_event(&hook_event(
            "session_start",
            &session,
            &[("SOURCE", "create")],
        ));
        fake.assert_no_request();
        assert!(!is_tracked(&session));
        unsafe { std::env::remove_var("JCODE_HERDR_REPORT") };

        unsafe { std::env::set_var("JCODE_HOOKS_DISABLED", "1") };
        report_observer_event(&hook_event(
            "session_start",
            &session,
            &[("SOURCE", "create")],
        ));
        fake.assert_no_request();
        assert!(!is_tracked(&session));
        unsafe { std::env::remove_var("JCODE_HOOKS_DISABLED") };
        clear_herdr_env();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn seq_is_strictly_monotonic_per_session() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("seq");

        attach_session(&session);
        let mut seen = vec![
            fake.next_state()["params"]["seq"]
                .as_u64()
                .expect("state seq"),
        ];
        for active in [true, false, true] {
            set_turn_active(&session, active);
            publish_state(&session);
            seen.push(
                fake.next_state()["params"]["seq"]
                    .as_u64()
                    .expect("state seq"),
            );
        }
        assert!(seen.windows(2).all(|w| w[1] > w[0]), "seqs: {seen:?}");

        detach_session(&session);
        clear_herdr_env();
    }

    /// Production path test: the shared daemon dispatches lifecycle hooks
    /// inside the owning client's terminal-env scope. Reports must flow from
    /// the scoped env even though the daemon process itself has no HERDR_*
    /// vars.
    #[tokio::test(flavor = "multi_thread")]
    async fn scoped_client_env_drives_dispatch_observer_reports() {
        let _guard = crate::storage::lock_test_env();
        clear_herdr_env();
        let fake = FakeHerdr::new();
        let session = unique_session("scoped");
        let scoped = vec![
            ("HERDR_ENV".to_string(), "1".to_string()),
            (
                "HERDR_SOCKET_PATH".to_string(),
                fake.socket.to_string_lossy().to_string(),
            ),
            ("HERDR_PANE_ID".to_string(), "w1:p-scoped".to_string()),
        ];
        let event_session = session.clone();
        crate::hooks::with_client_terminal_env(scoped, async move {
            crate::hooks::dispatch_observer(
                crate::hooks::HookEvent::new("session_start")
                    .session_id(&event_session)
                    .field("SOURCE", "create"),
            );
            crate::hooks::dispatch_observer(
                crate::hooks::HookEvent::new("turn_start")
                    .session_id(&event_session)
                    .field("MODEL", "test"),
            );
            crate::hooks::dispatch_observer(
                crate::hooks::HookEvent::new("turn_end")
                    .session_id(&event_session)
                    .field("STATUS", "ok"),
            );
            crate::hooks::dispatch_observer(
                crate::hooks::HookEvent::new("session_end")
                    .session_id(&event_session)
                    .field("SOURCE", "close"),
            );
        })
        .await;
        let session_report = fake.next_session();
        assert_eq!(session_report["params"]["pane_id"], "w1:p-scoped");
        assert_eq!(session_report["params"]["session_start_source"], "startup");
        assert_eq!(fake.next_state()["params"]["state"], "idle");
        assert_eq!(fake.next_state()["params"]["state"], "working");
        assert_eq!(fake.next_state()["params"]["state"], "idle");
        assert_eq!(fake.next_release()["method"], "pane.release_agent");
        assert!(!is_tracked(&session));
    }

    /// A scoped client *outside* Herdr must suppress even a Herdr-shaped
    /// process env: the daemon's inherited vars never leak into non-Herdr
    /// clients' sessions.
    #[tokio::test(flavor = "multi_thread")]
    async fn non_herdr_scoped_env_suppresses_process_fallback() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("suppress");
        let scoped = vec![("TERM".to_string(), "xterm-256color".to_string())];
        let event_session = session.clone();
        crate::hooks::with_client_terminal_env(scoped, async move {
            crate::hooks::dispatch_observer(
                crate::hooks::HookEvent::new("session_start")
                    .session_id(&event_session)
                    .field("SOURCE", "create"),
            );
        })
        .await;
        fake.assert_no_request();
        assert!(!is_tracked(&session));
        clear_herdr_env();
    }

    /// A second session attaching to an already-owned pane (headless swarm
    /// worker inheriting the spawning client's scope) must not hijack the
    /// primary session's state.
    #[tokio::test(flavor = "multi_thread")]
    async fn second_session_cannot_take_over_owned_pane() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let primary = unique_session("primary");
        let worker = unique_session("worker");
        attach_session(&primary);
        fake.next_session();
        fake.next_state();
        attach_session(&worker);
        fake.assert_no_request();
        assert!(!is_tracked(&worker));
        // The primary keeps its state; worker turns are ignored.
        report_observer_event(&hook_event("turn_start", &worker, &[]));
        fake.assert_no_request();
        detach_session(&primary);
        fake.next_release();
        clear_herdr_env();
    }

    /// Runtime fast paths gate payload construction on `hooks::hook_configured`
    /// (no shell hook set); Herdr attachment must keep those events flowing.
    #[tokio::test(flavor = "multi_thread")]
    async fn hook_configured_covers_herdr_lifecycle_events() {
        let _guard = crate::storage::lock_test_env();
        clear_herdr_env();
        for event in ["session_start", "turn_start", "turn_end", "session_end"] {
            assert!(
                !crate::hooks::hook_configured(event),
                "outside Herdr with no shell hook, {event} must stay gated off"
            );
        }
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        for event in ["session_start", "turn_start", "turn_end", "session_end"] {
            assert!(
                crate::hooks::hook_configured(event),
                "{event} must be considered configured while Herdr watches"
            );
        }
        for event in ["pre_tool", "post_tool"] {
            assert!(
                !crate::hooks::hook_configured(event),
                "{event} is a hot path and must never be enabled by Herdr"
            );
        }
        clear_herdr_env();
    }

    /// The safety observer router (`observe_permission_event`) is what the
    /// startup registration wires up; verify both arms reach the pins.
    #[tokio::test(flavor = "multi_thread")]
    async fn permission_observer_router_feeds_both_events() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("router");

        attach_session(&session);
        fake.next_session();
        fake.next_state();

        observe_permission_event(crate::safety::PermissionEvent::Queued(&permission_request(
            "req_r",
            &session,
            "wipe volumes",
        )));
        let blocked = fake.next_state();
        assert_eq!(blocked["params"]["state"], "blocked");
        assert_eq!(blocked["params"]["message"], "bash: wipe volumes");

        observe_permission_event(crate::safety::PermissionEvent::Resolved {
            request_id: "req_r",
        });
        assert_eq!(fake.next_state()["params"]["state"], "idle");

        detach_session(&session);
        clear_herdr_env();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn installed_hook_adapter_suppresses_native_emitter() {
        let _guard = crate::storage::lock_test_env();
        let fake = FakeHerdr::new();
        set_herdr_env(&fake.socket);
        let session = unique_session("adapter");

        // Same command-shape upstream's `herdr integration install jcode`
        // appends to [hooks] session_start (herdrdev/herdr#2248).
        unsafe {
            std::env::set_var(
                "JCODE_HOOK_SESSION_START",
                "~/.jcode/hooks/herdr-agent-state.sh",
            );
        }
        assert!(
            !active(),
            "native emitter must defer to the installed adapter"
        );
        report_observer_event(&hook_event(
            "session_start",
            &session,
            &[("SOURCE", "create")],
        ));
        fake.assert_no_request();
        assert!(!is_tracked(&session));

        unsafe { std::env::remove_var("JCODE_HOOK_SESSION_START") };
        assert!(active(), "removing the adapter re-enables native reports");
        report_observer_event(&hook_event(
            "session_start",
            &session,
            &[("SOURCE", "create")],
        ));
        fake.next_session();
        fake.next_state();
        detach_session(&session);
        clear_herdr_env();
    }

    #[test]
    fn truncates_long_labels() {
        let long = "x".repeat(300);
        let out = truncate_label(&long);
        assert_eq!(out.chars().count(), 120);
        assert!(out.ends_with('…'));
        assert_eq!(truncate_label(" short "), "short");
    }
}
