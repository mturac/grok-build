# Agent Mode (ACP) and IDE Integration

Agent mode runs Grok as an ACP (Agent Client Protocol) server for integration with IDEs, editors, and custom tooling. Unlike single-prompt mode (`grok -p`, which prints one response and exits), agent mode keeps a persistent process running and communicates through structured JSON-RPC messages.

---

## What is ACP?

The [Agent Client Protocol (ACP)](https://agentclientprotocol.com) is a standard for AI agent communication. It defines how clients (IDEs, editors, custom apps) interact with AI agents through a structured JSON-RPC protocol. ACP provides:

- **Session management** -- create, load, and resume conversations
- **Prompt submission** -- send user messages and receive streamed responses
- **Tool visibility** -- see what tools the agent is using in real time
- **Thought streams** -- observe the agent's reasoning process
- **Permission handling** -- approve or deny tool executions interactively

---

## stdio transport

stdio is the primary integration mode. The agent exchanges JSON-RPC messages over stdin and stdout:

```bash
grok agent stdio
```

Clients that use this mode include:

- IDE extensions (for example, Zed, Neovim, and Emacs)
- Custom automation tools
- ACP client libraries

### Options

These options belong to the `grok agent` command and apply to every mode. Pass them before the mode name, for example `grok agent --model grok-build stdio`. The `stdio` subcommand itself takes no options.

| Flag                       | Description                                                       |
| -------------------------- | ---------------------------------------------------------------- |
| `-m, --model <MODEL>`      | Set the model ID (for example, `grok-build`).                    |
| `--always-approve`         | Auto-approve every tool execution. (Alias: `--yolo`.)            |
| `--reauth`                 | Run authentication before starting the agent.                    |
| `--agent-profile <PATH>`   | Load an agent profile from a file.                               |

---

## Server mode

Run the agent as a WebSocket server for remote clients:

```bash
grok agent serve --bind 127.0.0.1:2419 --secret <token>
```

Clients connect over WebSocket and authenticate with the secret token. If you omit `--secret`, the agent generates a token and prints it at startup; you can also supply one through the `GROK_AGENT_SECRET` environment variable. The agent persists across reconnections, so a client can disconnect and later resume in-flight work.

---

## Remote client (TUI)

Connect your local interactive TUI to a `grok agent serve` instance running on
another machine:

```bash
# On the remote machine (e.g. a dev box):
grok agent serve --bind 0.0.0.0:2419 --secret <token>

# On your local machine:
grok --remote ws://devbox:2419/ws --remote-secret <token>
```

The TUI runs locally while the agent — including terminal commands, file
edits, model authentication, and session persistence — runs entirely on the
remote host. The secret can come from `--remote-secret`, the
`GROK_AGENT_SECRET` environment variable, or a `?server-key=<token>` query
parameter pasted from the server's startup banner.

Notes:

- Prefer `wss://` for anything beyond a trusted network (for example by
  terminating TLS at a reverse proxy in front of `grok agent serve`);
  standard `wss://` certificates work out of the box.
- Sessions live on the remote host. If the connection drops (network blip,
  laptop sleep, server restart), the TUI reconnects automatically with
  exponential backoff — no need to re-run `grok --remote ...` by hand.
- **Automatic reconnection and session replay.** Every server process
  generates a random instance id at startup and sends it to each connecting
  client in a one-time hello frame, before any ACP traffic. On reconnect the
  client compares the new hello's instance id against the one it saw before:
  - **Same instance id** (the server process, and its in-memory agent,
    survived — e.g. a transient network drop): the client just resumes
    pumping messages. Nothing is replayed.
  - **Different instance id** (the server process restarted and its agent
    state is gone): the client automatically replays `initialize` and
    `session/load` for the active session before resuming, so the
    conversation picks back up losslessly without any manual action. If the
    replay itself fails partway through (the new connection drops again
    before every session finishes loading), the client does not give up:
    it re-dials and retries the reconnect-and-replay sequence rather than
    resuming traffic against a half-restored agent.
  - The server must actually send its hello frame promptly: if a WS
    handshake completes but no hello arrives (for example, an old
    `grok agent serve` build that predates the hello frame), the client
    fails fast with an actionable error instead of hanging indefinitely.
- **Version skew.** The hello frame also carries the server's protocol
  version. If it doesn't match what the client expects, the connection (or
  reconnect) fails immediately with an error telling you to update whichever
  side — client or `grok agent serve` — is older, instead of limping along
  with an incompatible wire format.
- **Permanent connect failures don't retry forever.** A rejected secret (HTTP
  401) or a protocol version mismatch is not something a retry will ever fix.
  Reconnect attempts recognize these as permanent and give up immediately with
  the actionable message, rather than retrying silently under exponential
  backoff and leaving you looking at a TUI that seems stuck.
- Agent-startup flags such as `--experimental-memory` or `--storage-mode`
  have no effect in remote mode; configure them where the server runs.
- The server currently streams updates to one client at a time: a second
  connection takes over the update stream from the first.

---

## Mobile / browser client (PWA)

`grok agent serve` also serves a self-contained mobile chat UI directly from
the same process — no separate build step, no external requests, and it
works fully offline once the shell is cached. Open it from a phone or any
browser:

```bash
grok agent serve --bind 0.0.0.0:2419 --secret <token>
```

```
http://<host>:2419/
```

On first load, the page asks for the server key (the same `--secret` /
`GROK_AGENT_SECRET` value the TUI's `--remote-secret` uses). You can also
paste a link with the key pre-filled:

```
http://<host>:2419/?server-key=<token>
```

The key is stored in the browser's `localStorage` after you connect once, so
you won't be asked again on that device. The browser WebSocket API cannot set
an `Authorization` header, so the page connects with `?server-key=<token>` on
the `/ws` URL — the same query-parameter fallback the server already accepts
for the TUI's `--remote-secret`.

### What it does

- Speaks the same ACP JSON-RPC protocol as the TUI and IDE clients:
  `initialize` -> `session/new` (or `session/load` to resume) ->
  `session/prompt`, rendering `session/update` notifications as they stream
  in (agent text, dimmed "thinking" text, and compact tool-call lines).
- Renders `session/request_permission` prompts as inline buttons; the input
  box stays disabled until you pick an option.
- Remembers your last session id (in `localStorage`) and offers "Resume last
  session" vs. "Start a new session" the next time you open the page.
- Reconnects automatically with capped exponential backoff on a dropped
  connection, replaying `initialize` + `session/load` so the conversation
  picks back up without you doing anything.

### Install to your home screen

The page ships a web app manifest and a small service worker that
cache-first-serves the six static shell files (HTML/CSS/JS/manifest/service
worker/icon), so the shell itself loads even with no network. On Android
Chrome, use "Add to Home Screen"; on iOS Safari, use the Share sheet's "Add
to Home Screen". Either way you get a standalone, full-screen app icon — the
live chat itself still needs a WebSocket connection to the server, only the
shell is offline-capable.

### Caveats

- **Single active client.** Just like the TUI's `--remote` mode, the agent
  process only streams updates to one connected client at a time — if you
  open the page on a second device (or a second tab) while the first is
  connected, the new connection takes over the update stream from the first.
- **TLS.** The page connects with plain `ws://` unless you load it over
  `https://`, in which case it automatically upgrades to `wss://`. As with
  the TUI, `grok agent serve` doesn't terminate TLS itself — put a reverse
  proxy in front of it for anything beyond a trusted local network.
- **No filesystem/terminal capabilities.** The web client advertises
  `fs: { readTextFile: false, writeTextFile: false }` and `terminal: false`
  in its `initialize` call, matching a browser's actual capabilities.

---

## WebSocket relay

To reach the agent over the internet instead of the local network, run a WebSocket relay server and have the agent connect to it:

```bash
grok agent headless --grok-ws-url wss://your-relay.example.com/ws
```

The agent connects out to your relay, and your web clients connect to the same relay. This is useful for building web UIs where browsers cannot spawn local processes.

---

## ACP protocol basics

Communication follows the JSON-RPC 2.0 format. A typical session lifecycle:

1. **Initialize** -- client sends `initialize` with capabilities
2. **Create session** -- client sends `session/new` with working directory
3. **Send prompts** -- client sends `session/prompt` with user messages
4. **Receive updates** -- agent sends `session/update` notifications with streamed content
5. **Handle permissions** -- agent may request tool execution approval

### Architecture

```
+------------------------------------------+
|           ACP Client                     |
|  (IDE, Editor, Custom Application)       |
+-------------------+----------------------+
                    | JSON-RPC over stdio
+-------------------v----------------------+
|           grok agent stdio               |
|                                          |
|  +---------+  +---------+  +---------+   |
|  | Session |  |  Tools  |  |   MCP   |   |
|  | Manager |  | Registry|  | Servers |   |
|  +---------+  +---------+  +---------+   |
+------------------------------------------+
```

---

## Streaming updates

ACP streams structured events. Each `session/update` notification carries a `sessionUpdate` field that identifies the update type:

| `sessionUpdate` value | Description                                            |
| --------------------- | ----------------------------------------------------- |
| `agent_message_chunk` | A chunk of the agent's response text.                 |
| `agent_thought_chunk` | A chunk of the agent's internal reasoning.            |
| `tool_call`           | A new tool invocation (title, kind, status, input).   |
| `tool_call_update`    | A status or result update for an in-flight tool call. |
| `plan`                | The agent's execution plan.                           |

Each update names its type, so a client can render distinct panels for reasoning, tool calls, and response text.

---

## Extension methods

Beyond the base ACP protocol, Grok defines extension methods under the `x.ai/` prefix for SpaceXAI-specific functionality. These cover:

| Category                   | Prefix               | Examples                                         |
| -------------------------- | -------------------- | ------------------------------------------------ |
| **Filesystem**             | `x.ai/fs/*`          | `list`, `exists`, `read_file`, `write_file`      |
| **Git**                    | `x.ai/git/*`         | `status`, `stage`, `commit`, `diffs`, `discard`  |
| **Git Worktree**           | `x.ai/git/worktree/*`| `create`, `remove`, `apply`, `list`, `gc`        |
| **Search**                 | `x.ai/search/*`      | `fuzzy/open`, `fuzzy/change`, `content`          |
| **Terminal**               | `x.ai/terminal/*`    | `create`, `kill`, `output`, `wait_for_exit`      |
| **Session Management**     | `x.ai/session/*`     | `fork`, `resolve_local_for_worktree_resume`      |
| **Conversation & History** | `x.ai/*`             | `prompt_history`, `rewind/*`, `compact_conversation` |
| **Authentication**         | `x.ai/auth/*`        | `get_url`, `submit_code`                         |
| **Feedback & Telemetry**   | `x.ai/*`             | `feedback`, `telemetry/*`                        |

The tables here show representative methods in each category. The `x.ai/*` set is SpaceXAI-specific and may expand across releases, so treat it as non-exhaustive and discover the available methods from the agent's `initialize` response.

### Notifications (agent to client)

The agent sends push notifications to clients for real-time updates:

| Notification               | Description                          |
| -------------------------- | ------------------------------------ |
| `x.ai/search/fuzzy/status` | Fuzzy search results update          |
| `x.ai/git/worktree/status` | Worktree creation progress           |
| `x.ai/fs_notify`           | Filesystem change notification       |
| `x.ai/fs/index`            | Full file index update               |
| `x.ai/fs/index/delta`      | Incremental file index update        |
| `x.ai/session_notification`| Session-specific updates (diff review, retry state, auto-compact) |
| `x.ai/session/update`      | Session update (tool calls, content) |

---

## Session `_meta` options

The `session/new` request accepts these optional `_meta` fields:

| Field                  | Description                                    |
| ---------------------- | ---------------------------------------------- |
| `rules`                | Extra rules appended to the system prompt.     |
| `systemPromptOverride` | A replacement system prompt.                   |
| `agentProfile`         | An agent profile, as a name or a JSON object.  |

---

## ACP SDKs

Official SDK libraries are available for multiple languages:

| Language   | Package                                                                                  |
| ---------- | ---------------------------------------------------------------------------------------- |
| TypeScript | [`@agentclientprotocol/sdk`](https://www.npmjs.com/package/@agentclientprotocol/sdk)     |
| Rust       | [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol)                |
| Python     | [`agent-client-protocol-python`](https://github.com/PsiACE/agent-client-protocol-python) |
| Go         | [`acp-go-sdk`](https://github.com/coder/acp-go-sdk)                                     |
| Kotlin     | [`acp`](https://github.com/agentclientprotocol/kotlin-sdk)                               |

---

## Compatible clients

| Client                                                   | Status      |
| -------------------------------------------------------- | ----------- |
| [Zed](https://zed.dev/docs/ai/external-agents)           | Supported   |
| [Neovim](https://neovim.io) (CodeCompanion, avante.nvim) | Supported   |
| [Emacs](https://github.com/xenodium/agent-shell)         | Supported   |
| [marimo notebook](https://github.com/marimo-team/marimo) | Supported   |
| JetBrains                                                | Coming soon |

---

## Integration example: a TypeScript ACP client

```typescript
import { spawn, ChildProcess } from "child_process";
import * as readline from "readline";

class GrokACPChat {
  private proc!: ChildProcess;
  private sessionId!: string;
  private rl!: readline.Interface;

  constructor(private cwd = ".") {}

  async init() {
    this.proc = spawn("grok", ["agent", "stdio"]);
    this.rl = readline.createInterface({ input: this.proc.stdout! });

    // Initialize
    await this.request("initialize", {
      protocolVersion: 1,
      clientCapabilities: {
        fs: { readTextFile: true, writeTextFile: true },
        terminal: true,
      },
    });

    // Create session
    const { sessionId } = await this.request("session/new", {
      cwd: this.cwd,
      mcpServers: [],
    });
    this.sessionId = sessionId;
    return this;
  }

  private async request(method: string, params: any): Promise<any> {
    return new Promise((resolve) => {
      const msg = JSON.stringify({ jsonrpc: "2.0", id: 1, method, params });
      this.proc.stdin!.write(msg + "\n");

      this.rl.once("line", (line) => {
        resolve(JSON.parse(line).result || {});
      });
    });
  }

  async *streamPrompt(text: string) {
    const msg = JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "session/prompt",
      params: {
        sessionId: this.sessionId,
        prompt: [{ type: "text", text }],
      },
    });
    this.proc.stdin!.write(msg + "\n");

    for await (const line of this.rl) {
      const data = JSON.parse(line);

      if (data.method === "session/update") {
        const update = data.params.update;
        yield update; // { sessionUpdate, content, title, ... }
      } else if (data.result) {
        break; // Final response
      }
    }
  }
}

// Usage
const client = await new GrokACPChat(".").init();

for await (const update of client.streamPrompt("List the files in this project")) {
  switch (update.sessionUpdate) {
    case "agent_message_chunk":
      process.stdout.write(update.content?.text || "");
      break;
    case "agent_thought_chunk":
      console.log(`\n[Thinking: ${update.content?.text}]`);
      break;
    case "tool_call":
      console.log(`\n[Tool: ${update.title}]`);
      break;
  }
}
```

---

## CI Guardian (`ci_guard`)

The CI Guardian watches a GitHub PR's CI and, on a **confident code failure**,
prepares a fix on an isolated local branch for you to review — it **never pushes,
merges, or deploys**. It builds on the scheduler (for polling) and the push
notifications above (to alert you when a fix is ready or a failure needs you).

You don't call it by hand — just ask the agent (e.g. "watch CI on PR #7 and fix
it if it breaks"). The agent uses the `ci_guard` tool, which has these actions:

| Action | What it does |
|--------|--------------|
| `start {repo, pr}` | Begin watching (polls every ~5 min via a durable scheduler task). Requires `gh` to be authenticated. |
| `check {repo, pr}` | One poll tick — the watch's fired prompt calls this. On a *new* confident code failure it returns the logs + diagnosis. |
| `commit_fix {repo, pr, head_sha, files}` | Apply the agent's edits on branch `grok-ci-fix/<pr>-<sha>` and commit locally. **Never pushes.** |
| `rearm {repo, pr}` | Allow one more autonomous fix after you've reviewed the last one. |
| `stop {repo, pr}` / `status` | Stop a watch (also automatic on PR merge/close) / list active watches. |

### Safety model

- **One autonomous fix per PR until you re-arm.** After a fix is prepared, the
  guardian will not prepare another — even for a brand-new failing commit — until
  you review it and run `rearm`. A new head SHA does **not** refill the budget.
  This prevents an unattended fix→push→fail→fix loop.
- **Never pushes.** The fix lands as a local commit on an isolated
  `grok-ci-fix/<pr>-<sha>` branch. You review and push it yourself.
- **Diagnosis-first.** Only a confident, localized code failure (a cited panic,
  assertion, or compiler error) is auto-fixed. Flaky, infra, timeout, permission,
  or ambiguous failures are reported for you to handle — never auto-patched.
- **Fail-closed.** If `gh` auth or repo access is missing, the guardian pauses and
  tells you, rather than silently treating it as "CI still pending".
- **Clean-worktree only.** A fix is refused if the worktree has uncommitted
  changes, and aborted if the PR head moved since diagnosis (a stale patch).

### Notifications

CI Guardian outcomes (`fix ready`, `cannot auto-fix`, `blocked`) ride the same
delivery as scheduled tasks: in-band to a connected TUI/PWA, and (when Web Push
is configured, see above) as a push notification so you're alerted even when the
app is closed.

## Resources

- [ACP Specification](https://agentclientprotocol.com/protocol/prompt-turn)
- [Protocol Introduction](https://agentclientprotocol.com/overview/introduction)
