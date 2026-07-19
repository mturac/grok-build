use super::{AgentNotification, Notifier};

/// Dispatches each event to every registered sink; a failing sink never
/// blocks the others.
pub struct FanoutNotifier {
    sinks: Vec<std::sync::Arc<dyn Notifier>>,
}

impl FanoutNotifier {
    pub fn new(sinks: Vec<std::sync::Arc<dyn Notifier>>) -> Self {
        Self { sinks }
    }
}

#[async_trait::async_trait]
impl Notifier for FanoutNotifier {
    async fn notify(&self, event: &AgentNotification) {
        for s in &self.sinks {
            s.notify(event).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        seen: tokio::sync::Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl Notifier for RecordingSink {
        async fn notify(&self, e: &AgentNotification) {
            self.seen.lock().await.push(e.kind.clone());
        }
    }
    struct FailingSink;
    #[async_trait::async_trait]
    impl Notifier for FailingSink {
        async fn notify(&self, _e: &AgentNotification) { /* returns () but does nothing */ }
    }

    #[tokio::test]
    async fn fanout_delivers_to_all_sinks() {
        let rec = std::sync::Arc::new(RecordingSink::default());
        let fan = FanoutNotifier::new(vec![std::sync::Arc::new(FailingSink), rec.clone()]);
        fan.notify(&AgentNotification {
            kind: "k1".into(),
            title: "t".into(),
            body: "".into(),
            meta: serde_json::Value::Null,
        })
        .await;
        assert_eq!(rec.seen.lock().await.as_slice(), &["k1".to_string()]);
    }
}
