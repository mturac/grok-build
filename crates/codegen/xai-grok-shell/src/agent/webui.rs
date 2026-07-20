//! Static asset routes for the mobile/browser PWA chat client served by
//! `grok agent serve`.
//!
//! Assets live under `src/agent/webui/` and are embedded at compile time via
//! `include_str!` — no filesystem access at runtime, no new dependencies. The
//! page speaks ACP JSON-RPC directly over the existing `/ws` endpoint (see
//! `super::server`); this module only serves the HTML/CSS/JS shell.
//!
//! Serving the shell requires NO auth: it contains no secrets (the user
//! types the server key into the page, which is then sent as the `/ws`
//! query parameter exactly like the TUI's `--remote-secret`). The `/ws`
//! endpoint's `validate_auth` check is completely untouched by this module.

use axum::http::header;
use axum::response::{IntoResponse, Response};

const INDEX_HTML: &str = include_str!("webui/index.html");
const APP_JS: &str = include_str!("webui/app.js");
const STYLE_CSS: &str = include_str!("webui/style.css");
const MANIFEST_WEBMANIFEST: &str = include_str!("webui/manifest.webmanifest");
const SW_JS: &str = include_str!("webui/sw.js");
const ICON_SVG: &str = include_str!("webui/icon.svg");

/// `Cache-Control: no-cache` on every shell asset (not just `sw.js`): the
/// service worker is the durable offline cache (with its own explicit
/// versioning and `{cache: "reload"}` install), so the HTTP layer should
/// always revalidate rather than let a browser's own HTTP cache pin a stale
/// shell version independently of the service worker's cache lifecycle.
fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// `GET /` — the app shell.
pub async fn index() -> Response {
    asset(INDEX_HTML, "text/html; charset=utf-8")
}

/// `GET /app.js` — the vanilla-JS ACP client.
pub async fn app_js() -> Response {
    asset(APP_JS, "text/javascript; charset=utf-8")
}

/// `GET /style.css`
pub async fn style_css() -> Response {
    asset(STYLE_CSS, "text/css; charset=utf-8")
}

/// `GET /manifest.webmanifest` — PWA install metadata.
pub async fn manifest() -> Response {
    asset(MANIFEST_WEBMANIFEST, "application/manifest+json")
}

/// `GET /sw.js` — the offline-shell service worker.
///
/// `Cache-Control: no-cache` (via [`asset`]) so browsers re-check for an
/// updated worker on every load instead of pinning an old shell version
/// indefinitely (a well-known service-worker foot-gun).
pub async fn service_worker() -> Response {
    asset(SW_JS, "text/javascript; charset=utf-8")
}

/// `GET /icon.svg` — app icon, referenced by the manifest and as the
/// favicon/apple-touch-icon.
pub async fn icon_svg() -> Response {
    asset(ICON_SVG, "image/svg+xml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_string(resp: Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn index_serves_html_shell() {
        let resp = index().await;
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = body_string(resp).await;
        assert!(body.contains("<title>Grok Remote</title>"));
        assert!(body.contains("id=\"app\""));
    }

    #[tokio::test]
    async fn app_js_is_nonempty_javascript() {
        let resp = app_js().await;
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
        let body = body_string(resp).await;
        assert!(body.contains("session/prompt"));
    }

    #[tokio::test]
    async fn manifest_has_correct_content_type_and_shape() {
        let resp = manifest().await;
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/manifest+json"
        );
        let body = body_string(resp).await;
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON manifest");
        assert_eq!(value["name"], "Grok Remote");
        assert_eq!(value["display"], "standalone");
    }

    #[tokio::test]
    async fn service_worker_has_no_cache_header() {
        let resp = service_worker().await;
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        let body = body_string(resp).await;
        assert!(body.contains("SHELL_ASSETS"));
    }

    #[tokio::test]
    async fn icon_svg_has_svg_content_type() {
        let resp = icon_svg().await;
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/svg+xml"
        );
        let body = body_string(resp).await;
        assert!(body.trim_start().starts_with("<svg"));
    }

    #[tokio::test]
    async fn style_css_is_nonempty() {
        let resp = style_css().await;
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/css; charset=utf-8"
        );
        let body = body_string(resp).await;
        assert!(body.contains("#app"));
    }

    /// Every shell asset — not just `sw.js` — must tell the browser's HTTP
    /// cache to revalidate rather than pin a stale version independently of
    /// the service worker's own (explicitly versioned) cache.
    #[tokio::test]
    async fn all_assets_carry_no_cache_header() {
        for resp in [
            index().await,
            app_js().await,
            style_css().await,
            manifest().await,
            service_worker().await,
            icon_svg().await,
        ] {
            assert_eq!(
                resp.headers().get(header::CACHE_CONTROL).unwrap(),
                "no-cache"
            );
        }
    }

    /// The PWA client must carry the Web Push wiring: `app.js` subscribes via
    /// `PushManager` and posts to `/push/subscribe`; `sw.js` handles incoming
    /// `push` events and `notificationclick`. Guards against a refactor
    /// silently dropping the push path from the shipped assets.
    #[test]
    fn pwa_assets_contain_web_push_wiring() {
        assert!(
            APP_JS.contains("pushManager"),
            "app.js must subscribe via PushManager"
        );
        assert!(
            APP_JS.contains("/push/subscribe"),
            "app.js must register the subscription with the server"
        );
        assert!(
            SW_JS.contains("addEventListener(\"push\""),
            "sw.js must handle incoming push events"
        );
        assert!(
            SW_JS.contains("addEventListener(\"notificationclick\""),
            "sw.js must handle notification clicks"
        );
    }
}
