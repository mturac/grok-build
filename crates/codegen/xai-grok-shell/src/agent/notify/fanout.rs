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
        // Dispatch to every sink concurrently, each on its own task, so a slow,
        // hanging, or panicking sink (e.g. an HTTP push sink that stalls) can
        // neither block nor abort delivery to the others. `tokio::spawn`
        // isolates panics: a panicking task resolves to a `JoinError` we ignore
        // rather than unwinding this fan-out. Each task gets an owned clone of
        // the event (`AgentNotification: Clone + Send + 'static`).
        let handles: Vec<_> = self
            .sinks
            .iter()
            .map(|s| {
                let sink = s.clone();
                let ev = event.clone();
                tokio::spawn(async move { sink.notify(&ev).await })
            })
            .collect();
        for h in handles {
            // Ignore JoinError (a panicked sink) — isolation is the whole point.
            let _ = h.await;
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
    struct NoOpSink;
    #[async_trait::async_trait]
    impl Notifier for NoOpSink {
        async fn notify(&self, _e: &AgentNotification) { /* returns () but does nothing */ }
    }

    /// A sink that panics on every event — used to prove the fan-out isolates
    /// a failing sink and still delivers to the healthy ones.
    struct PanicSink;
    #[async_trait::async_trait]
    impl Notifier for PanicSink {
        async fn notify(&self, _e: &AgentNotification) {
            panic!("sink boom");
        }
    }

    fn event(kind: &str) -> AgentNotification {
        AgentNotification {
            kind: kind.into(),
            title: "t".into(),
            body: "".into(),
            meta: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn fanout_delivers_to_all_sinks() {
        let rec = std::sync::Arc::new(RecordingSink::default());
        let fan = FanoutNotifier::new(vec![std::sync::Arc::new(NoOpSink), rec.clone()]);
        fan.notify(&event("k1")).await;
        assert_eq!(rec.seen.lock().await.as_slice(), &["k1".to_string()]);
    }

    #[tokio::test]
    async fn fanout_isolates_panicking_sink() {
        // A panicking sink must NOT stop delivery to the healthy recording sink.
        let rec = std::sync::Arc::new(RecordingSink::default());
        let fan = FanoutNotifier::new(vec![std::sync::Arc::new(PanicSink), rec.clone()]);
        fan.notify(&event("k1")).await;
        assert_eq!(rec.seen.lock().await.as_slice(), &["k1".to_string()]);
    }
}
