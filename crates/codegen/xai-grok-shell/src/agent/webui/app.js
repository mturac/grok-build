// Grok Remote — vanilla-JS ACP client over the /ws endpoint served by
// `grok agent serve`. No frameworks, no external requests: everything this
// page needs ships in the six assets the server embeds at compile time.
//
// Wire protocol summary (see crates/codegen/xai-grok-shell/src/agent/server.rs
// and docs/user-guide/15-agent-mode.md for the authoritative version):
//   1. Server sends a one-time `{"type":"hello",...}` frame first.
//   2. Then it's ACP JSON-RPC 2.0 lines, one per WebSocket text frame:
//      requests have {id, method, params}, responses have {id, result|error},
//      notifications have {method, params} with no id.
//   3. This client always speaks: initialize -> session/new|session/load ->
//      session/prompt, and renders session/update notifications as they
//      stream in.
//
// Unknown session/update kinds and unknown incoming requests are handled
// tolerantly (ignored, or answered with a JSON-RPC "method not found") rather
// than treated as fatal — the agent evolves independently of this MVP client.

(() => {
  "use strict";

  const SECRET_KEY = "grok-remote-secret";
  const SESSION_KEY = "grok-remote-session-id";
  const RECONNECT_BASE_MS = 1000;
  const RECONNECT_MAX_MS = 30000;
  const EXPECTED_PROTOCOL_VERSION = 1;

  const qs = (id) => document.getElementById(id);

  /** @type {any} */
  const state = {
    ws: null,
    secret: "",
    sessionId: localStorage.getItem(SESSION_KEY) || null,
    cwd: null,
    nextId: 1,
    pending: new Map(),
    sessionActive: false,
    currentAssistantEl: null,
    currentThoughtEl: null,
    toolEls: new Map(),
    reconnectDelay: RECONNECT_BASE_MS,
    reconnectTimer: null,
    manualClose: false,
    permission: null,
    helloSeen: false,
    // Consecutive WS closes where the hello frame never arrived (typically
    // a rejected/rotated secret returning 401 at the handshake). Reset on
    // any successful hello or a fresh manual connect attempt; once it hits
    // MAX_PRE_HELLO_FAILURES we stop auto-retrying instead of looping
    // forever against a key that will never work.
    preHelloFailures: 0,
    agentInstanceId: null,
    binaryVersion: null,
    // Web Push is set up once per page load after the first session is ready.
    pushSetupDone: false,
  };

  const MAX_PRE_HELLO_FAILURES = 2;

  // ---------------------------------------------------------------------
  // Small DOM helpers
  // ---------------------------------------------------------------------

  function showOverlay(id) {
    qs(id).classList.remove("hidden");
  }

  function hideOverlay(id) {
    qs(id).classList.add("hidden");
  }

  function setStatus(sessionLabel, statusLabel, dotClass) {
    qs("session-label").textContent = sessionLabel;
    qs("status-label").textContent = statusLabel;
    qs("status-dot").className = "dot dot-" + dotClass;
  }

  // Transient toast (connection transitions, copy confirmations, ...).
  let toastTimer = null;
  function showToast(text) {
    let t = qs("toast");
    if (!t) {
      t = document.createElement("div");
      t.id = "toast";
      document.body.appendChild(t);
    }
    t.textContent = text;
    t.classList.add("show");
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => t.classList.remove("show"), 2600);
  }

  // Dark/light theme: follow the OS by default; a toggle persists an override.
  const THEME_KEY = "grok-remote-theme";
  function applyTheme(theme) {
    if (theme === "dark" || theme === "light") {
      document.documentElement.setAttribute("data-theme", theme);
    } else {
      document.documentElement.removeAttribute("data-theme"); // follow OS
    }
  }
  function initTheme() {
    applyTheme(localStorage.getItem(THEME_KEY));
    const btn = qs("theme-btn");
    if (btn) {
      btn.addEventListener("click", () => {
        const cur = document.documentElement.getAttribute("data-theme");
        const next = cur === "dark" ? "light" : "dark";
        localStorage.setItem(THEME_KEY, next);
        applyTheme(next);
      });
    }
  }

  function showBanner(text) {
    const b = qs("version-warning");
    b.textContent = text;
    b.classList.remove("hidden");
  }

  function hideBanner() {
    qs("version-warning").classList.add("hidden");
  }

  function shortId(id) {
    return id ? id.slice(0, 8) : "";
  }

  function scrollToBottom() {
    const m = qs("messages");
    m.scrollTop = m.scrollHeight;
  }

  // --- Markdown (tiny, dependency-free, escape-first) ---
  function escapeHtml(s) {
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  // Render a small, safe subset of Markdown to HTML. Everything is escaped
  // first, then only our own known tags are (re)introduced — model/tool text
  // can never inject markup.
  function renderMarkdown(raw) {
    const src = String(raw == null ? "" : raw);
    const codeBlocks = [];
    // Per-render, unguessable placeholder wrapped in private-use codepoints
    // (never present in normal text; survive escapeHtml untouched) so untrusted
    // body text cannot forge a placeholder and displace/repeat a code block.
    const marker = "\uE000" + Math.random().toString(36).slice(2) + "\uE000";
    // 1. Pull fenced code blocks out first (so their contents aren't touched).
    let work = src.replace(/```([\w-]*)\n?([\s\S]*?)```/g, (_m, lang, code) => {
      const idx = codeBlocks.length;
      codeBlocks.push(
        `<div class="code-wrap"><button class="copy-btn" data-copy type="button">copy</button>` +
          `<pre class="code${lang ? " lang-" + escapeHtml(lang) : ""}"><code>${escapeHtml(
            code.replace(/\n$/, ""),
          )}</code></pre></div>`,
      );
      return marker + idx + marker;
    });
    work = escapeHtml(work);
    // 2. Inline + block markdown on the escaped text.
    work = work
      .replace(/`([^`]+)`/g, (_m, c) => `<code class="inline">${c}</code>`)
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>")
      .replace(
        /\[([^\]]+)\]\((https?:[^)\s]+)\)/g,
        '<a href="$2" target="_blank" rel="noreferrer noopener">$1</a>',
      );
    // Headings + lists, line by line.
    const lines = work.split("\n");
    let html = "";
    let inList = false;
    for (const line of lines) {
      const h = line.match(/^(#{1,4})\s+(.*)$/);
      const li = line.match(/^\s*[-*]\s+(.*)$/);
      if (h) {
        if (inList) { html += "</ul>"; inList = false; }
        const level = h[1].length;
        html += `<h${level}>${h[2]}</h${level}>`;
      } else if (li) {
        if (!inList) { html += "<ul>"; inList = true; }
        html += `<li>${li[1]}</li>`;
      } else if (line.indexOf(marker) !== -1) {
        // A code-block placeholder line: emit it bare (never wrap a block-level
        // code <div> in a <p>, which the browser would auto-close into an empty
        // <p></p> and add stray vertical space).
        if (inList) { html += "</ul>"; inList = false; }
        html += line;
      } else {
        if (inList) { html += "</ul>"; inList = false; }
        html += line.trim() === "" ? "" : `<p>${line}</p>`;
      }
    }
    if (inList) html += "</ul>";
    // 3. Restore code blocks.
    codeBlocks.forEach((block, i) => {
      html = html.split(marker + i + marker).join(block);
    });
    return html;
  }

  // Wire copy buttons inside a freshly-rendered element.
  function wireCopyButtons(root) {
    root.querySelectorAll("[data-copy]").forEach((btn) => {
      btn.addEventListener("click", () => {
        const pre = btn.parentElement.querySelector("pre, code");
        const text = pre ? pre.textContent : "";
        if (navigator.clipboard) navigator.clipboard.writeText(text).catch(() => {});
        const old = btn.textContent;
        btn.textContent = "copied";
        setTimeout(() => (btn.textContent = old), 1200);
      });
    });
  }

  // Roles that render Markdown; others stay plain text.
  const MARKDOWN_ROLES = { assistant: true, thought: true };

  function appendMessage(role, text) {
    const el = document.createElement("div");
    el.className = "msg msg-" + role;
    if (MARKDOWN_ROLES[role]) {
      el.dataset.raw = text || "";
      el.innerHTML = renderMarkdown(el.dataset.raw);
      wireCopyButtons(el);
    } else {
      el.textContent = text;
    }
    qs("messages").appendChild(el);
    scrollToBottom();
    return el;
  }

  function clearMessages() {
    qs("messages").textContent = "";
    state.currentAssistantEl = null;
    state.currentThoughtEl = null;
    state.toolEls.clear();
  }

  function disableInput(hint) {
    qs("input").disabled = true;
    qs("send-btn").disabled = true;
    qs("input").placeholder = hint || "Connecting…";
  }

  function enableInput() {
    if (state.permission) return; // stays disabled until the permission choice is made
    qs("input").disabled = false;
    qs("send-btn").disabled = false;
    qs("input").placeholder = "Message Grok…";
  }

  function autoGrow(el) {
    el.style.height = "auto";
    el.style.height = Math.min(el.scrollHeight, 140) + "px";
  }

  // ---------------------------------------------------------------------
  // Connection
  // ---------------------------------------------------------------------

  function connect(secret) {
    // Guard against a reconnect storm: if a previous socket is still
    // connecting/open (e.g. connect() called again before it settled),
    // detach our listeners from it first so its eventual close doesn't
    // trigger a second onClose()/reconnect cycle, then actively close it
    // rather than leaking it.
    if (
      state.ws &&
      (state.ws.readyState === WebSocket.CONNECTING || state.ws.readyState === WebSocket.OPEN)
    ) {
      const stale = state.ws;
      stale.removeEventListener("message", onMessage);
      stale.removeEventListener("close", onClose);
      try {
        stale.close();
      } catch (_err) {
        /* already closing/closed */
      }
    }
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = null;

    state.secret = secret;
    state.manualClose = false;
    state.helloSeen = false;
    setStatus("Grok Remote", "Connecting…", "connecting");

    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${proto}//${location.host}/ws?server-key=${encodeURIComponent(secret)}`;

    let ws;
    try {
      ws = new WebSocket(url);
    } catch (err) {
      setStatus("Grok Remote", "Failed to connect: " + err.message, "disconnected");
      scheduleReconnect();
      return;
    }
    state.ws = ws;
    ws.addEventListener("message", onMessage);
    ws.addEventListener("close", onClose);
    ws.addEventListener("error", () => {
      /* onClose fires right after; nothing extra to do here */
    });
  }

  function rejectAllPending(err) {
    for (const { reject } of state.pending.values()) {
      reject(err);
    }
    state.pending.clear();
  }

  function clearPermissionOnDisconnect() {
    if (!state.permission) return;
    state.permission = null;
    qs("permission-panel").classList.add("hidden");
    qs("permission-panel").textContent = "";
  }

  function onClose() {
    // Regardless of why the socket closed: no request sent on it will ever
    // get an answer, and a permission-request id from it is meaningless on
    // whatever connection comes next.
    rejectAllPending(new Error("connection lost"));
    clearPermissionOnDisconnect();

    if (state.manualClose) return;

    if (!state.helloSeen) {
      // The socket closed before this connection ever got as far as a
      // hello frame — most likely the secret was rejected (401) rather
      // than a transient network blip. Retrying that forever just spams
      // the server with the same doomed handshake, so give it a couple of
      // tries (in case it really was a blip) and then stop and ask the
      // user to check the key instead of looping silently.
      state.preHelloFailures += 1;
      if (state.preHelloFailures >= MAX_PRE_HELLO_FAILURES) {
        disableInput("Connection rejected — check your server key.");
        setStatus("Grok Remote", "Connection rejected", "disconnected");
        const errEl = qs("connect-error");
        errEl.textContent =
          "Could not connect — the server key may be invalid or rotated.";
        errEl.classList.remove("hidden");
        showOverlay("connect-overlay");
        return;
      }
    }

    disableInput("Reconnecting…");
    setStatus("Grok Remote", "Disconnected — reconnecting…", "disconnected");
    scheduleReconnect();
  }

  function scheduleReconnect() {
    clearTimeout(state.reconnectTimer);
    showToast(`Reconnecting in ${Math.round(state.reconnectDelay / 1000)}s…`);
    state.reconnectTimer = setTimeout(() => {
      connect(state.secret);
    }, state.reconnectDelay);
    state.reconnectDelay = Math.min(state.reconnectDelay * 2, RECONNECT_MAX_MS);
  }

  function manualDisconnect() {
    state.manualClose = true;
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = null;
    if (state.ws) {
      // Listeners are removed before closing, so onClose() (and its
      // reconnect logic) never fires for a deliberate disconnect — do its
      // "any disconnect" cleanup duties here instead.
      state.ws.removeEventListener("message", onMessage);
      state.ws.removeEventListener("close", onClose);
      try {
        state.ws.close();
      } catch (_err) {
        /* already closed */
      }
    }
    rejectAllPending(new Error("connection lost"));
    clearPermissionOnDisconnect();
  }

  function sendRaw(obj) {
    if (state.ws && state.ws.readyState === WebSocket.OPEN) {
      state.ws.send(JSON.stringify(obj));
    }
  }

  function sendRequest(method, params) {
    return new Promise((resolve, reject) => {
      if (!state.ws || state.ws.readyState !== WebSocket.OPEN) {
        reject(new Error("not connected"));
        return;
      }
      const id = state.nextId++;
      state.pending.set(id, { resolve, reject });
      state.ws.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
    });
  }

  // ---------------------------------------------------------------------
  // Message dispatch
  // ---------------------------------------------------------------------

  function onMessage(event) {
    let msg;
    try {
      msg = JSON.parse(event.data);
    } catch (_err) {
      return; // not JSON (e.g. a stray keepalive) — ignore
    }

    if (!state.helloSeen) {
      if (msg && msg.type === "hello") {
        state.helloSeen = true;
        handleHello(msg);
      }
      // Anything else before the hello is unexpected; ignore rather than crash.
      return;
    }

    dispatch(msg);
  }

  function dispatch(msg) {
    if (!msg || typeof msg !== "object") return;

    if (msg.method === "session/update") {
      handleSessionUpdate(msg.params);
      return;
    }

    if (msg.method === "session/request_permission") {
      handlePermissionRequest(msg);
      return;
    }

    if (msg.method !== undefined) {
      // x.ai/* NOTIFICATIONS (no id): CI Guardian / scheduled-task events.
      // Render an event card in the stream; never reply method-not-found to a
      // notification.
      if (msg.id === undefined && msg.method.indexOf("x.ai/") === 0) {
        renderEventCard(msg.method, msg.params);
        return;
      }
      // An agent->client request/notification this MVP client doesn't
      // implement (e.g. fs/*, terminal/*, x.ai/*). We advertised no fs/
      // terminal capabilities, so the agent shouldn't normally call those,
      // but answer politely instead of leaving a pending request hanging
      // forever if it does.
      if (msg.id !== undefined) {
        sendRaw({
          jsonrpc: "2.0",
          id: msg.id,
          error: { code: -32601, message: `Method not found: ${msg.method}` },
        });
      }
      return;
    }

    if (msg.id !== undefined) {
      const pending = state.pending.get(msg.id);
      if (!pending) return;
      state.pending.delete(msg.id);
      if (msg.error) {
        pending.reject(new Error(msg.error.message || "request failed"));
      } else {
        pending.resolve(msg.result);
      }
    }
  }

  function handleHello(hello) {
    // A hello frame is proof the secret was accepted and the transport is
    // healthy — whatever ACP-level trouble might follow (initialize,
    // session/new, ...) is not a reason to keep counting toward the
    // "give up on this key" threshold.
    state.preHelloFailures = 0;
    state.agentInstanceId = hello.agent_instance_id;
    state.binaryVersion = hello.binary_version;

    if (hello.protocol_version !== EXPECTED_PROTOCOL_VERSION) {
      showBanner(
        `Server speaks remote-agent protocol v${hello.protocol_version}, this page expects ` +
          `v${EXPECTED_PROTOCOL_VERSION}. Update whichever of the client or grok agent serve is older.`,
      );
    } else {
      hideBanner();
    }

    beginSession();
  }

  // ---------------------------------------------------------------------
  // ACP session lifecycle
  // ---------------------------------------------------------------------

  async function beginSession() {
    setStatus("Grok Remote", "Initializing…", "connecting");
    try {
      const initResult = await sendRequest("initialize", {
        protocolVersion: 1,
        clientCapabilities: {
          fs: { readTextFile: false, writeTextFile: false },
          terminal: false,
        },
        clientInfo: { name: "grok-remote-web", version: "0.1.0" },
      });

      const meta = initResult && initResult._meta;
      state.cwd = (meta && meta.currentWorkingDirectory) || null;
      if (!state.cwd) {
        throw new Error("agent did not report a working directory in initialize()");
      }

      if (!state.sessionActive) {
        // First time this page load establishes a session: offer to resume
        // if we remember one, otherwise just start fresh.
        if (state.sessionId) {
          showOverlay("chooser-overlay");
          return;
        }
        await newSession();
      } else {
        // A WS reconnect mid-session: resume automatically, no re-prompting.
        await loadSession(state.sessionId, { fallbackToNew: true });
      }
    } catch (err) {
      setStatus("Grok Remote", "Error: " + err.message, "disconnected");
      if (state.ws && state.ws.readyState === WebSocket.OPEN) {
        // The transport is fine — this was an ACP-level rejection
        // (initialize/session/new/session/load returned an error), not a
        // dropped connection. Retrying the same request in a reconnect
        // loop wouldn't help, so surface it and stop rather than treating
        // it like a transport failure.
        appendMessage("error", err.message);
        if (state.sessionId) {
          // We already had a session going into this (re)connect — let the
          // user keep typing; a follow-up prompt will surface any further
          // trouble the same way.
          enableInput();
        } else {
          disableInput("Could not start a session — see the error above.");
        }
      } else {
        scheduleReconnect();
      }
    }
  }

  async function newSession() {
    const result = await sendRequest("session/new", { cwd: state.cwd, mcpServers: [] });
    state.sessionId = result.sessionId;
    localStorage.setItem(SESSION_KEY, state.sessionId);
    onSessionReady("New session");
  }

  async function loadSession(sessionId, opts) {
    try {
      await sendRequest("session/load", {
        sessionId,
        cwd: state.cwd,
        mcpServers: [],
      });
      state.sessionId = sessionId;
      localStorage.setItem(SESSION_KEY, sessionId);
      onSessionReady("Resumed session");
    } catch (err) {
      if (opts && opts.fallbackToNew) {
        appendMessage("system", "Could not resume the previous session — starting a new one.");
        localStorage.removeItem(SESSION_KEY);
        await newSession();
      } else {
        throw err;
      }
    }
  }

  function onSessionReady(label) {
    state.sessionActive = true;
    state.reconnectDelay = RECONNECT_BASE_MS;
    state.preHelloFailures = 0;
    // A fully successful (re)connect means there's nothing left to retry —
    // don't let a stale timer from an earlier, still-in-flight reconnect
    // attempt fire later and open a second, redundant connection.
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = null;
    hideOverlay("connect-overlay");
    hideOverlay("chooser-overlay");
    const versionTag = state.binaryVersion ? ` · v${state.binaryVersion}` : "";
    setStatus(
      "Grok Remote",
      `${label} · ${shortId(state.sessionId)}${versionTag}`,
      "connected",
    );
    enableInput();
    // Opportunistically register for Web Push so the phone gets notified about
    // background agent activity (scheduled tasks, etc.) when the PWA is closed.
    // Fire-and-forget: never block or fail the session on push setup.
    setupPush();
  }

  // ---------------------------------------------------------------------
  // Web Push registration
  // ---------------------------------------------------------------------

  /** Decode a base64url (no padding) VAPID key into the Uint8Array that
   * `PushManager.subscribe` expects as `applicationServerKey`. */
  function urlBase64ToUint8Array(base64UrlNoPad) {
    const padding = "=".repeat((4 - (base64UrlNoPad.length % 4)) % 4);
    const base64 = (base64UrlNoPad + padding).replace(/-/g, "+").replace(/_/g, "/");
    const raw = atob(base64);
    const out = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
    return out;
  }

  async function setupPush() {
    if (state.pushSetupDone) return;
    if (
      !("serviceWorker" in navigator) ||
      !("PushManager" in window) ||
      !("Notification" in window)
    ) {
      return; // browser can't do Web Push (e.g. iOS before add-to-home-screen)
    }
    // Mark done up front so overlapping onSessionReady calls don't double-run;
    // reset on failure below so a later session-ready can retry.
    state.pushSetupDone = true;
    try {
      if (Notification.permission === "denied") {
        // Respect the user's block, but allow a later attempt (a future
        // session-ready after they change the browser setting) to retry.
        state.pushSetupDone = false;
        return;
      }
      if (Notification.permission === "default") {
        const perm = await Notification.requestPermission();
        if (perm !== "granted") {
          state.pushSetupDone = false;
          return;
        }
      }
      // Send the secret in the Authorization header, NOT as a ?server-key=
      // query param: unlike the WS upgrade, these are plain HTTP GET/POST whose
      // full URL (secret included) could land in a reverse-proxy access log or
      // a Referer header. validate_auth checks the Bearer header first, so this
      // keeps the secret out of URLs entirely. (Same-origin fetch, so it was
      // never in browser history, but access logs are the real exposure.)
      const authHeaders = { authorization: `Bearer ${state.secret}` };
      const reg = await navigator.serviceWorker.ready;
      const keyResp = await fetch("/push/vapid-public-key", { headers: authHeaders });
      if (!keyResp.ok) {
        state.pushSetupDone = false;
        return;
      }
      const { publicKey } = await keyResp.json();
      let sub = await reg.pushManager.getSubscription();
      if (!sub) {
        sub = await reg.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: urlBase64ToUint8Array(publicKey),
        });
      }
      const keys = sub.toJSON().keys || {};
      await fetch("/push/subscribe", {
        method: "POST",
        headers: { ...authHeaders, "content-type": "application/json" },
        body: JSON.stringify({
          endpoint: sub.endpoint,
          p256dh: keys.p256dh || "",
          auth: keys.auth || "",
        }),
      });
    } catch (_err) {
      // Push is a best-effort enhancement — never surface it as a chat error.
      state.pushSetupDone = false;
    }
  }

  // ---------------------------------------------------------------------
  // session/update rendering
  // ---------------------------------------------------------------------

  function contentText(content) {
    if (!content) return "";
    if (content.type === "text") return content.text || "";
    return ""; // images/audio/resources: not rendered in this MVP client
  }

  // Accumulate raw text on the element and re-render Markdown progressively.
  function appendChunkTo(el, text) {
    el.dataset.raw = (el.dataset.raw || "") + text;
    el.innerHTML = renderMarkdown(el.dataset.raw);
    wireCopyButtons(el);
    scrollToBottom();
  }

  function appendAssistantChunk(content) {
    const text = contentText(content);
    if (!text) return;
    if (!state.currentAssistantEl) {
      state.currentAssistantEl = appendMessage("assistant", "");
    }
    appendChunkTo(state.currentAssistantEl, text);
  }

  function appendThoughtChunk(content) {
    const text = contentText(content);
    if (!text) return;
    if (!state.currentThoughtEl) {
      // A collapsible "thinking" block (collapsed by default, muted).
      const wrap = document.createElement("details");
      wrap.className = "msg msg-thought";
      const summary = document.createElement("summary");
      summary.textContent = "\u{1F4AD} thinking";
      const body = document.createElement("div");
      body.className = "thought-body";
      wrap.appendChild(summary);
      wrap.appendChild(body);
      qs("messages").appendChild(wrap);
      state.currentThoughtEl = body;
      scrollToBottom();
    }
    appendChunkTo(state.currentThoughtEl, text);
  }

  const TOOL_ICONS = {
    read: "\u{1F4D6}",
    edit: "✏️",
    delete: "\u{1F5D1}️",
    move: "\u{1F4E6}",
    search: "\u{1F50D}",
    execute: "⚙️",
    think: "\u{1F4AD}",
    fetch: "\u{1F310}",
    switch_mode: "\u{1F500}",
    other: "\u{1F527}",
  };

  // Extract any text detail (e.g. a diff) from a tool update's content array.
  function toolDetailText(update) {
    const content = update && update.content;
    if (!Array.isArray(content)) return "";
    return content
      .map((c) => {
        if (!c) return "";
        if (typeof c.text === "string") return c.text;
        if (c.content && typeof c.content.text === "string") return c.content.text;
        return "";
      })
      .filter(Boolean)
      .join("\n");
  }

  function looksLikeDiff(text) {
    return /^[+-] /m.test(text) || /^@@ /m.test(text);
  }

  function renderDiff(text) {
    return text
      .split("\n")
      .map((line) => {
        let cls = "";
        if (line.startsWith("+")) cls = "add";
        else if (line.startsWith("-")) cls = "del";
        else if (line.startsWith("@@")) cls = "hunk";
        return `<span class="dl ${cls}">${escapeHtml(line)}</span>`;
      })
      .join("");
  }

  function toolCardEl(toolCallId) {
    let el = state.toolEls.get(toolCallId);
    if (!el) {
      el = document.createElement("div");
      el.className = "tool-card";
      el.innerHTML =
        '<div class="tool-head"><span class="tool-icon"></span>' +
        '<span class="tool-title"></span><span class="tool-badge"></span></div>' +
        '<div class="tool-detail hidden"></div>';
      el.querySelector(".tool-head").addEventListener("click", () => {
        const d = el.querySelector(".tool-detail");
        if (d.textContent.trim() || d.children.length) d.classList.toggle("hidden");
      });
      qs("messages").appendChild(el);
      state.toolEls.set(toolCallId, el);
    }
    return el;
  }

  function renderToolCard(el, title, kind, status, detail) {
    const icon = TOOL_ICONS[kind] || TOOL_ICONS.other;
    el.className = "tool-card status-" + (status || "pending");
    el.querySelector(".tool-icon").textContent = icon;
    el.querySelector(".tool-title").textContent = title || "Tool call";
    el.querySelector(".tool-badge").textContent = status || "pending";
    if (detail && detail.trim()) {
      const d = el.querySelector(".tool-detail");
      if (looksLikeDiff(detail)) {
        d.className = "tool-detail diff";
        d.innerHTML = renderDiff(detail);
      } else {
        d.className = "tool-detail";
        d.innerHTML = `<pre class="code"><code>${escapeHtml(detail)}</code></pre>`;
      }
      el.querySelector(".tool-head").classList.add("expandable");
    }
    scrollToBottom();
  }

  function handleToolCall(update) {
    const el = toolCardEl(update.toolCallId);
    el.dataset.title = update.title || "";
    el.dataset.kind = update.kind || "other";
    renderToolCard(el, update.title, update.kind, update.status, toolDetailText(update));
  }

  function handleToolCallUpdate(update) {
    const el = toolCardEl(update.toolCallId);
    const title = update.title !== undefined ? update.title : el.dataset.title;
    const kind = update.kind !== undefined ? update.kind : el.dataset.kind;
    if (update.title !== undefined) el.dataset.title = update.title;
    if (update.kind !== undefined) el.dataset.kind = update.kind;
    renderToolCard(el, title, kind, update.status, toolDetailText(update));
  }

  // Render an in-band event (CI Guardian / scheduled task) as a distinct card.
  // Reads payload fields defensively — an unexpected shape falls back to a
  // generic card rather than crashing the renderer.
  function renderEventCard(method, params) {
    const p = params || {};
    let icon = "\u{1F514}";
    let title = method;
    let body = "";
    if (method.indexOf("scheduled_task_fired") >= 0) {
      icon = "⏰";
      title = "Scheduled task fired";
      body = p.prompt || p.humanSchedule || "";
    } else if (method.indexOf("scheduled_task_created") >= 0) {
      icon = "⏰";
      title = "Scheduled task created";
      body = p.humanSchedule || "";
    } else if (method.indexOf("ci_guard_event") >= 0) {
      const key = Object.keys(p)[0] || "";
      const d = p[key] || {};
      const pr = d.pr !== undefined ? ` — PR #${d.pr}` : "";
      if (key === "FixReady") {
        icon = "✅";
        title = `CI fix ready${pr}`;
        body = `branch ${d.branch || "?"}\n${d.diffSummary || d.diff_summary || ""}`;
      } else if (key === "CannotAutofix") {
        icon = "⚠️";
        title = `CI: cannot auto-fix${pr}`;
        body = d.reason || "";
      } else if (key === "Blocked") {
        icon = "\u{1F6AB}";
        title = `CI Guardian blocked${pr}`;
        body = d.reason || "";
      } else if (key === "WatchStarted") {
        icon = "\u{1F440}";
        title = `Watching${pr}`;
        body = d.repo || "";
      } else if (key === "JobState") {
        icon = "⚙️";
        title = `CI job: ${d.state || "?"}${pr}`;
      } else {
        title = "CI Guardian event";
        body = JSON.stringify(p);
      }
    }
    const el = document.createElement("div");
    el.className = "event-card";
    el.innerHTML =
      '<span class="event-icon"></span><div class="event-text">' +
      '<div class="event-title"></div><div class="event-body"></div></div>';
    el.querySelector(".event-icon").textContent = icon;
    el.querySelector(".event-title").textContent = title;
    const b = el.querySelector(".event-body");
    if (body) b.textContent = body;
    else b.remove();
    qs("messages").appendChild(el);
    // A new event breaks any in-progress assistant/thought accumulation.
    state.currentAssistantEl = null;
    state.currentThoughtEl = null;
    scrollToBottom();
  }

  function handleSessionUpdate(params) {
    const update = params && params.update;
    if (!update) return;
    switch (update.sessionUpdate) {
      case "agent_message_chunk":
        appendAssistantChunk(update.content);
        break;
      case "agent_thought_chunk":
        appendThoughtChunk(update.content);
        break;
      case "tool_call":
        handleToolCall(update);
        break;
      case "tool_call_update":
        handleToolCallUpdate(update);
        break;
      default:
        // user_message_chunk, plan, available_commands_update,
        // current_mode_update, config_option_update, session_info_update,
        // usage_update, and any future kind: not rendered by this MVP
        // client. Tolerant-by-design — an unknown kind must never crash
        // the renderer.
        break;
    }
  }

  // ---------------------------------------------------------------------
  // session/request_permission
  // ---------------------------------------------------------------------

  function handlePermissionRequest(msg) {
    if (state.permission) {
      // The agent replaced a still-outstanding permission request with a
      // new one (e.g. the tool call was cancelled/superseded). Answer the
      // stale one as cancelled — the ACP-documented outcome for a request
      // that was never actually decided by the user — instead of leaving
      // it dangling or silently dropping it unanswered.
      sendRaw({
        jsonrpc: "2.0",
        id: state.permission.id,
        result: { outcome: { outcome: "cancelled" } },
      });
      state.permission = null;
    }

    const params = msg.params || {};
    state.permission = { id: msg.id };

    const panel = qs("permission-panel");
    panel.textContent = "";

    const title = document.createElement("div");
    title.className = "permission-title";
    title.textContent = (params.toolCall && params.toolCall.title) || "Permission requested";
    panel.appendChild(title);

    const row = document.createElement("div");
    row.className = "permission-buttons";
    (params.options || []).forEach((opt) => {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.textContent = opt.name || opt.optionId;
      btn.className = "permission-btn kind-" + (opt.kind || "allow_once");
      btn.addEventListener("click", () => resolvePermission(opt.optionId));
      row.appendChild(btn);
    });
    panel.appendChild(row);

    panel.classList.remove("hidden");
    disableInput("Waiting for permission decision…");
  }

  function resolvePermission(optionId) {
    if (!state.permission) return;
    sendRaw({
      jsonrpc: "2.0",
      id: state.permission.id,
      result: { outcome: { outcome: "selected", optionId } },
    });
    state.permission = null;
    qs("permission-panel").classList.add("hidden");
    qs("permission-panel").textContent = "";
    enableInput();
  }

  // ---------------------------------------------------------------------
  // Sending prompts
  // ---------------------------------------------------------------------

  function finalizeTurn() {
    state.currentAssistantEl = null;
    state.currentThoughtEl = null;
  }

  async function sendPrompt(text) {
    const trimmed = text.trim();
    if (!trimmed || !state.sessionId) return;

    appendMessage("user", trimmed);
    finalizeTurn();
    disableInput("Waiting for response…");
    try {
      await sendRequest("session/prompt", {
        sessionId: state.sessionId,
        prompt: [{ type: "text", text: trimmed }],
      });
    } catch (err) {
      appendMessage("error", "Prompt failed: " + err.message);
    } finally {
      finalizeTurn();
      enableInput();
    }
  }

  function submitInput() {
    const input = qs("input");
    const text = input.value;
    input.value = "";
    autoGrow(input);
    sendPrompt(text);
  }

  // ---------------------------------------------------------------------
  // Wiring
  // ---------------------------------------------------------------------

  function init() {
    initTheme();
    // Tap the session label to copy the short session id.
    qs("session-label").addEventListener("click", () => {
      if (!state.sessionId) return;
      const short = shortId(state.sessionId);
      if (navigator.clipboard) navigator.clipboard.writeText(state.sessionId).catch(() => {});
      showToast(`Copied session id ${short}`);
    });
    qs("send-btn").addEventListener("click", submitInput);
    qs("input").addEventListener("input", (e) => autoGrow(e.target));
    qs("input").addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        submitInput();
      }
    });

    qs("connect-btn").addEventListener("click", () => {
      const val = qs("secret-input").value.trim();
      if (!val) return;
      // A deliberate, manual retry deserves a fresh couple of tries even if
      // an earlier automatic attempt already burned through the pre-hello
      // failure budget.
      state.preHelloFailures = 0;
      qs("connect-error").classList.add("hidden");
      localStorage.setItem(SECRET_KEY, val);
      hideOverlay("connect-overlay");
      connect(val);
    });
    qs("secret-input").addEventListener("keydown", (e) => {
      if (e.key === "Enter") qs("connect-btn").click();
    });

    qs("resume-btn").addEventListener("click", () => {
      hideOverlay("chooser-overlay");
      loadSession(state.sessionId, { fallbackToNew: true }).catch((err) =>
        appendMessage("error", err.message),
      );
    });
    qs("new-session-btn").addEventListener("click", () => {
      hideOverlay("chooser-overlay");
      state.sessionId = null;
      localStorage.removeItem(SESSION_KEY);
      newSession().catch((err) => appendMessage("error", err.message));
    });

    qs("menu-btn").addEventListener("click", () => {
      qs("menu-dropdown").classList.toggle("hidden");
    });
    document.addEventListener("click", (e) => {
      const menu = qs("menu-dropdown");
      if (!menu.classList.contains("hidden") && !menu.contains(e.target) && e.target !== qs("menu-btn")) {
        menu.classList.add("hidden");
      }
    });
    qs("menu-new-session").addEventListener("click", () => {
      qs("menu-dropdown").classList.add("hidden");
      state.sessionId = null;
      localStorage.removeItem(SESSION_KEY);
      clearMessages();
      disableInput("Starting new session…");
      newSession().catch((err) => appendMessage("error", err.message));
    });
    qs("menu-change-key").addEventListener("click", () => {
      qs("menu-dropdown").classList.add("hidden");
      localStorage.removeItem(SECRET_KEY);
      manualDisconnect();
      clearMessages();
      state.sessionActive = false;
      state.preHelloFailures = 0;
      qs("connect-error").classList.add("hidden");
      qs("secret-input").value = "";
      showOverlay("connect-overlay");
    });

    if ("serviceWorker" in navigator) {
      window.addEventListener("load", () => {
        navigator.serviceWorker.register("/sw.js").catch(() => {
          /* offline shell just won't be available; not fatal */
        });
      });
    }

    const urlParams = new URLSearchParams(location.search);
    const urlSecret = urlParams.get("server-key") || "";
    if (urlParams.has("server-key")) {
      // SECURITY: the secret must not linger in the address bar (visible
      // over a shoulder, kept in browser history, synced to other
      // devices, or picked up by a screenshot/share-sheet). Strip just the
      // `server-key` param and leave everything else (path, hash, other
      // query params) untouched.
      urlParams.delete("server-key");
      const remaining = urlParams.toString();
      const cleanUrl =
        location.pathname + (remaining ? `?${remaining}` : "") + location.hash;
      history.replaceState(null, "", cleanUrl);
    }

    if (urlSecret) {
      // A freshly opened/shared link always wins over whatever's already
      // stored — the user explicitly navigated here with this key, so a
      // stale or different stored secret must not silently take over.
      localStorage.setItem(SECRET_KEY, urlSecret);
      connect(urlSecret);
    } else {
      const storedSecret = localStorage.getItem(SECRET_KEY) || "";
      if (storedSecret) {
        connect(storedSecret);
      } else {
        showOverlay("connect-overlay");
      }
    }
  }

  init();
})();
