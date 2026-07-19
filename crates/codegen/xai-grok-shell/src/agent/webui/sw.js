// Minimal service worker for the Grok Remote PWA shell.
//
// Cache-first for the six static shell assets (so the app boots offline);
// network for everything else (in particular the /ws WebSocket endpoint,
// which a service worker cannot and must not intercept).
//
// Bump CACHE_NAME whenever any shell asset changes so clients pick up the
// new version instead of serving stale files from an old cache.
const CACHE_NAME = "grok-remote-shell-v1";
const SHELL_ASSETS = [
  "/",
  "/app.js",
  "/style.css",
  "/manifest.webmanifest",
  "/icon.svg",
];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(CACHE_NAME)
      .then((cache) =>
        // {cache: "reload"} bypasses the HTTP cache so an install always
        // fetches fresh bytes from the network instead of potentially
        // repopulating the offline shell from a stale browser-cache entry.
        cache.addAll(SHELL_ASSETS.map((url) => new Request(url, { cache: "reload" }))),
      )
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(
          keys
            .filter((key) => key !== CACHE_NAME)
            .map((key) => caches.delete(key)),
        ),
      )
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;

  const url = new URL(req.url);
  if (url.origin !== self.location.origin) return;

  // Never intercept the WebSocket upgrade or anything outside the shell —
  // let those go straight to the network.
  if (!SHELL_ASSETS.includes(url.pathname)) return;

  // SECURITY: match/store by the bare pathname, never the full request.
  // A navigation like `/?server-key=<secret>` has pathname "/" but a query
  // string carrying the secret; caching `req` (or matching against it)
  // would key/store the *full URL* — including the secret — in Cache
  // Storage, which is unencrypted on-disk. The pathname-only key guarantees
  // the secret never gets anywhere near the cache.
  const cacheKey = url.pathname;

  event.respondWith(
    caches.match(cacheKey).then((cached) => {
      if (cached) return cached;
      return fetch(req).then((res) => {
        // Never cache error responses (4xx/5xx) — a transient failure
        // must not get "stuck" as the cached answer for the shell asset.
        if (res.ok) {
          const copy = res.clone();
          caches.open(CACHE_NAME).then((cache) => cache.put(cacheKey, copy));
        }
        return res;
      });
    }),
  );
});
