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
