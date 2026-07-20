# PWA Enrichment — Design

- **Status:** Approved design (pre-implementation)
- **Date:** 2026-07-20
- **Author:** mturac

## Motivation
The mobile/browser PWA served by `grok agent serve` (`agent/webui/{index.html,app.js,style.css}`) is a working-but-basic chat client: plain-text messages, single-line tool entries, no in-band rendering of the CI-Guardian / scheduled-task events, and a minimal mobile look. This enriches it — **client-side only**, no server/ACP change, no external CDN (the PWA must stay self-contained and offline-cacheable via `sw.js`).

## Scope (4 areas, all in the embedded assets)

### A. Markdown + code rendering
- A small hand-rolled `renderMarkdown(raw)` in `app.js`: HTML-escape first (no injection), then render fenced code blocks ```` ``` ````, inline `code`, `**bold**`, `*italic*`, headings, `-`/`*` lists, and links. No external library.
- Assistant (and thought) chunks accumulate **raw** text on the element (`dataset.raw`) and re-render `innerHTML = renderMarkdown(raw)` per chunk (progressive; a half-open code fence renders tolerantly).
- Code blocks: `<pre><code>` with an horizontal-scroll container and a **copy** button; each assistant message also gets a copy-to-clipboard control.

### B. Tool-call cards + diff + collapsible thought
- `tool_call`/`tool_call_update` render as a **card** (icon + title + a status badge: pending / in_progress / completed / failed, color-coded) instead of a one-liner. The card is expandable to show detail when present.
- When a tool update carries file-edit content that looks like a diff, render it in a `<pre class="diff">` with `+`/`-` line coloring, collapsed by default.
- `agent_thought_chunk` renders as a muted, **collapsible** "thinking" block (collapsed by default), markdown-rendered inside.

### C. In-band event cards (CI Guardian / scheduled tasks)
- In `dispatch`, before the generic "method not found" reply, catch notification methods (no `id`) whose name starts with `x.ai/` — e.g. `x.ai/ci_guard_event`, `x.ai/scheduled_task_fired`, `x.ai/scheduled_task_created` — and render a distinct **event card** (icon + title + body) in the message stream, then return (do not reply method-not-found for these notifications). Unknown `x.ai/*` notifications render a generic event card; unknown `x.ai/*` **requests** (with `id`) keep the existing polite `-32601`.
- Event → card mapping (best-effort, tolerant of shape): CI fix ready → "✅ CI fix ready — branch X"; CI cannot-autofix / blocked → "⚠️/🚫 …"; scheduled task fired → "⏰ …". Payload fields are read defensively.

### D. Mobile UX + connection polish
- **Dark theme** via `prefers-color-scheme` (and a small toggle persisted in `localStorage`), using CSS custom properties.
- **Toasts** for connection transitions (connecting / reconnecting / disconnected / queued), auto-dismissing.
- Tap the session label to **copy the short session id**.
- Mobile layout: safe-area insets (notch), sticky composer above the keyboard, momentum scroll, larger tap targets.

## Safety / constraints
- HTML-escape all model/tool/event text before any `innerHTML` (the markdown renderer escapes first, then re-introduces only its own known tags). No `x.ai/*` handler ever executes remote content as code.
- No new network origins (CSP/offline unaffected); `sw.js` continues to cache the same asset set.
- The existing secret-handling, reconnect/replay, and permission flows are untouched — this is additive rendering + styling.

## Testing
- Rust asset string-guard tests (`webui.rs` / `tests/test_webui_routes.rs`) extended to assert the new features are present in the shipped assets (`renderMarkdown`, a tool-card class, `x.ai/` event handling, a theme variable). Runtime JS behavior is verified by a live browser demo (there is no JS test harness in the Rust build).
- Manual demo: `grok agent serve` + open the PWA; drive the render functions with sample `session/update` and `x.ai/*` payloads to show markdown, tool cards, event cards, and the dark theme.

## Non-goals
- Session list/switcher (needs a server `session/list` — deferred).
- Full syntax highlighting (styled code blocks + copy only; a highlighter lib is out of scope for a no-CDN bundle).
- Native app.
