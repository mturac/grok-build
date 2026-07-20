pub mod ece;
pub mod fanout;
pub mod store;
pub mod vapid;
pub mod webpush;

pub use fanout::FanoutNotifier;
pub use store::{PushStore, PushSubscription};
pub use webpush::WebPushNotifier;

/// A single agent-originated notification to deliver to clients.
#[derive(Clone, Debug)]
pub struct AgentNotification {
    pub kind: String,             // stable machine tag, e.g. "scheduled_task_fired"
    pub title: String,            // human summary line
    pub body: String,             // optional detail
    pub meta: serde_json::Value,  // structured payload (pr, branch, etc.)
}

/// A sink that can receive `AgentNotification` events (in-band ACP, push, etc.).
#[async_trait::async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, event: &AgentNotification);
}

/// Process-wide out-of-band notifier, set once by `grok agent serve` at startup.
///
/// The push transport (subscription store + VAPID keypair) is genuinely one per
/// server process, and the axum route handlers that own it live outside any
/// agent session. Rather than thread an `Arc<dyn Notifier>` down through every
/// layer between the server and each session's notification bridge, the server
/// publishes it here once and the session-spawn path reads it. In modes with no
/// push transport (local TUI, leader), it stays unset and the bridge's notifier
/// is simply `None`.
static GLOBAL_NOTIFIER: std::sync::OnceLock<std::sync::Arc<dyn Notifier>> =
    std::sync::OnceLock::new();

/// Publish the process-wide notifier. First writer wins; later calls are ignored
/// (a process has a single push transport). Safe to call once at server startup.
///
/// Test note: because this is process-global, multiple `run_agent_server_on`
/// calls in one test binary share the FIRST server's notifier/store/VAPID — do
/// not write tests that assume per-server push isolation.
pub fn set_global_notifier(notifier: std::sync::Arc<dyn Notifier>) {
    let _ = GLOBAL_NOTIFIER.set(notifier);
}

/// The process-wide notifier, if `grok agent serve` published one.
pub fn global_notifier() -> Option<std::sync::Arc<dyn Notifier>> {
    GLOBAL_NOTIFIER.get().cloned()
}
