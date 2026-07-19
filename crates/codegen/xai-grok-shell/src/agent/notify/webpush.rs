//! Web Push delivery sink (payload-less MVP).
//!
//! Implements [`Notifier`] by POSTing to every stored push subscription's
//! endpoint with a VAPID `Authorization` header. This MVP sends NO encrypted
//! payload (RFC 8291 `aes128gcm` is a follow-up): a payload-less push simply
//! wakes the PWA service worker, which shows a static line and fetches details
//! over `/ws`. Endpoints the push service reports as gone (HTTP 404/410) are
//! pruned from the store.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_LENGTH};

use super::store::PushStore;
use super::vapid::VapidKeys;
use super::{AgentNotification, Notifier};

/// TTL (seconds) a push service holds an undelivered message. Short: these are
/// "wake up and refresh" pings, not durable messages.
const PUSH_TTL_SECONDS: u32 = 60;

/// Per-request timeout. `notify()` contacts subscriptions sequentially, so
/// without this a single hung push endpoint would stall delivery to every
/// other device for the OS-level TCP timeout.
const PUSH_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Delivers agent notifications to browser PWA clients over Web Push.
pub struct WebPushNotifier {
    store: PushStore,
    vapid: Arc<VapidKeys>,
    http: reqwest::Client,
    /// VAPID `sub` claim: a `mailto:` or `https:` contact URI for this server.
    subject: String,
}

impl WebPushNotifier {
    pub fn new(store: PushStore, vapid: Arc<VapidKeys>, subject: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(PUSH_REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            store,
            vapid,
            http,
            subject: subject.into(),
        }
    }
}

/// The VAPID `aud`: the endpoint's origin (`scheme://host[:port]`), never a
/// path. Returns `None` for a non-tuple (opaque) or unparseable origin.
fn endpoint_origin(endpoint: &str) -> Option<String> {
    let url = url::Url::parse(endpoint).ok()?;
    match url.origin() {
        origin @ url::Origin::Tuple(..) => Some(origin.ascii_serialization()),
        url::Origin::Opaque(_) => None,
    }
}

#[async_trait]
impl Notifier for WebPushNotifier {
    async fn notify(&self, _event: &AgentNotification) {
        // Payload-less: the event content is intentionally not sent. The SW
        // renders a generic line and the PWA pulls specifics over the WS.
        for sub in self.store.all().await {
            let Some(aud) = endpoint_origin(&sub.endpoint) else {
                tracing::warn!("skipping push to malformed endpoint {:?}", sub.endpoint);
                continue;
            };
            let auth = match self.vapid.sign_auth_header(&aud, &self.subject) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!("VAPID signing failed for {:?}: {e}", sub.endpoint);
                    continue;
                }
            };
            let resp = self
                .http
                .post(&sub.endpoint)
                .header(AUTHORIZATION, auth)
                .header("TTL", PUSH_TTL_SECONDS.to_string())
                .header(CONTENT_LENGTH, "0")
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let status = r.status();
                    let code = status.as_u16();
                    if code == 404 || code == 410 {
                        // Subscription is permanently gone — stop trying it.
                        tracing::info!("pruning gone push endpoint {:?}", sub.endpoint);
                        self.store.prune(&sub.endpoint).await;
                    } else if !status.is_success() {
                        // 429 (rate limit), 401/403 (VAPID/auth), 413 (too large),
                        // 5xx (transient) — keep the subscription but surface it;
                        // these are not "gone", so pruning would be wrong.
                        tracing::warn!(
                            "push endpoint {:?} returned {} (kept, not pruned)",
                            sub.endpoint,
                            code
                        );
                    }
                }
                Err(e) => tracing::warn!("push send failed for {:?}: {e}", sub.endpoint),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::notify::PushSubscription;
    use std::sync::Mutex;

    #[test]
    fn endpoint_origin_strips_path_and_keeps_scheme_host() {
        assert_eq!(
            endpoint_origin("https://fcm.googleapis.com/fcm/send/abc123"),
            Some("https://fcm.googleapis.com".to_string())
        );
        assert_eq!(
            endpoint_origin("http://127.0.0.1:8080/push/x"),
            Some("http://127.0.0.1:8080".to_string())
        );
        assert_eq!(endpoint_origin("not a url"), None);
    }

    fn sub(endpoint: String) -> PushSubscription {
        PushSubscription {
            endpoint,
            p256dh: "k".into(),
            auth: "a".into(),
        }
    }

    #[tokio::test]
    async fn notify_sends_vapid_header_and_prunes_gone_endpoints() {
        use axum::extract::State;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::post;
        use axum::Router;

        // Mock push service: /ok records the Authorization header and returns
        // 201; /gone returns 410 so the notifier should prune it.
        let recorded: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let rec_state = recorded.clone();
        let app = Router::new()
            .route(
                "/ok",
                post(|State(rec): State<Arc<Mutex<Option<String>>>>, headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    *rec.lock().unwrap() = auth;
                    StatusCode::CREATED
                }),
            )
            .route("/gone", post(|| async { StatusCode::GONE }))
            .with_state(rec_state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let tmp = tempfile::tempdir().unwrap();
        let store = PushStore::load(tmp.path());
        store.add(sub(format!("http://{addr}/ok"))).await;
        store.add(sub(format!("http://{addr}/gone"))).await;

        let vapid = Arc::new(VapidKeys::generate_for_test());
        let notifier = WebPushNotifier::new(store.clone(), vapid, "mailto:test@example.com");
        notifier
            .notify(&AgentNotification {
                kind: "k".into(),
                title: "t".into(),
                body: "".into(),
                meta: serde_json::Value::Null,
            })
            .await;

        // The reachable endpoint received a VAPID Authorization header.
        let got = recorded.lock().unwrap().clone();
        let got = got.expect("/ok should have been called");
        assert!(got.starts_with("vapid t="), "expected VAPID header, got {got:?}");

        // The 410 endpoint was pruned; only /ok remains.
        let remaining = store.all().await;
        assert_eq!(remaining.len(), 1, "gone endpoint must be pruned");
        assert!(remaining[0].endpoint.ends_with("/ok"));
    }
}
