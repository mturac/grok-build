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

  function appendMessage(role, text) {
    const el = document.createElement("div");
    el.className = "msg msg-" + role;
    el.textContent = text;
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
  }

  // ---------------------------------------------------------------------
  // session/update rendering
  // ---------------------------------------------------------------------

  function contentText(content) {
    if (!content) return "";
    if (content.type === "text") return content.text || "";
    return ""; // images/audio/resources: not rendered in this MVP client
  }

  function appendAssistantChunk(content) {
    const text = contentText(content);
    if (!text) return;
    if (!state.currentAssistantEl) {
      state.currentAssistantEl = appendMessage("assistant", "");
    }
    state.currentAssistantEl.textContent += text;
    scrollToBottom();
  }

  function appendThoughtChunk(content) {
    const text = contentText(content);
    if (!text) return;
    if (!state.currentThoughtEl) {
      state.currentThoughtEl = appendMessage("thought", "");
    }
    state.currentThoughtEl.textContent += text;
    scrollToBottom();
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

  function toolLineEl(toolCallId) {
    let el = state.toolEls.get(toolCallId);
    if (!el) {
      el = document.createElement("div");
      el.className = "tool-line";
      qs("messages").appendChild(el);
      state.toolEls.set(toolCallId, el);
    }
    return el;
  }

  function renderToolLine(el, title, kind, status) {
    const icon = TOOL_ICONS[kind] || TOOL_ICONS.other;
    const statusLabel = status ? ` — ${status}` : "";
    el.textContent = `${icon} ${title || "Tool call"}${statusLabel}`;
    el.className = "tool-line status-" + (status || "pending");
    scrollToBottom();
  }

  function handleToolCall(update) {
    const el = toolLineEl(update.toolCallId);
    el.dataset.title = update.title || "";
    el.dataset.kind = update.kind || "other";
    renderToolLine(el, update.title, update.kind, update.status);
  }

  function handleToolCallUpdate(update) {
    const el = toolLineEl(update.toolCallId);
    const title = update.title !== undefined ? update.title : el.dataset.title;
    const kind = update.kind !== undefined ? update.kind : el.dataset.kind;
    if (update.title !== undefined) el.dataset.title = update.title;
    if (update.kind !== undefined) el.dataset.kind = update.kind;
    renderToolLine(el, title, kind, update.status);
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
