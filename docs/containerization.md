# Running grok-build in a sandbox

grok-build has **no in-process permission system** — its `read`/`write`/`edit`
tools and `!` shell commands run with the privileges of the process that
launched it. If you want stronger boundaries (an untrusted repo, an autonomous
loop, a CI runner), run the agent **inside a container** so its tools act on the
container's filesystem and process space, not your host.

This mirrors the containerization pattern in
[earendil-works/pi](https://github.com/earendil-works/pi).

## Quick start

```sh
scripts/sandbox.sh                 # build the image (first run) + launch grok in a container
scripts/sandbox.sh --no-network    # same, but cut all network egress
scripts/sandbox.sh --rebuild       # force an image rebuild
scripts/sandbox.sh -- --version    # pass args through to grok
```

The current directory is bind-mounted read-write at `/work`; the agent edits
your code there (that is the point). The container runs as a non-root user with
**all Linux capabilities dropped**, `no-new-privileges`, and a PID cap.

## What is and isn't isolated

| Boundary | Status |
|---|---|
| Host filesystem outside the mounted dir | **Isolated** — only the bind-mounted CWD is visible |
| Host processes / other containers | **Isolated** by the container namespace |
| Network egress | On by default (models/auth need it); `--no-network` cuts it entirely |
| The mounted working directory | **NOT isolated** — the agent can modify it (intended); commit or back up first |

This is containment, not a security guarantee against a determined escape. For
stronger isolation use a VM (e.g. a Linux micro-VM) or a policy-controlled
sandbox; the container pattern here is the pragmatic default.

## Credentials

- **API keys** in your environment are forwarded automatically (`XAI_API_KEY`,
  `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `GROQ_API_KEY`,
  `DEEPSEEK_API_KEY`, and the `TURAC_LLM_ROUTER_URL`/`_KEY` router vars).
- **Subscription auth** (`/login`) stores credentials under `$HOME`, which is
  ephemeral in the container — it does **not** persist across runs. Either use
  API keys, or mount a persistent credential volume:

  ```sh
  docker run --rm -it -v grok-creds:/home/agent/.grok -v "$PWD:/work" grok-sandbox
  ```

## Sandboxing this fork's build (not the released CLI)

`docker/sandbox.Dockerfile` installs the **released** `grok` CLI, which does not
include this fork's features (artifacts, `/review`, `code_context`, …). To
sandbox the fork, build from source in the container instead — replace the
install step with a multi-stage build:

```dockerfile
# builder stage
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
# Requires DotSlash + protoc on PATH (see README "Building from source").
RUN cargo build -p xai-grok-pager-bin --release
# runtime stage: copy target/release/xai-grok-pager in as `grok`
```

This is heavier (a full Rust build) and is left as a documented variant rather
than the default image, since most sandbox users want a quick, disposable
runtime.

## Notes

- `--network none` is the safest mode for inspecting an untrusted repo, but the
  agent can't reach a model provider — use it for offline/read-only tasks.
- Add `--memory`/`--cpus` to `docker run` in `scripts/sandbox.sh` if you want
  hard resource caps.
