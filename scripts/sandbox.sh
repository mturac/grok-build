#!/usr/bin/env bash
# Run grok-build inside a container so its tools and `!` shell commands act on
# the container, not the host. See docs/containerization.md.
#
#   scripts/sandbox.sh [--rebuild] [--no-network] [-- <grok args...>]
#
# The current directory is bind-mounted read-write at /work (the agent needs to
# edit your code — that is the point); provider API keys present in the
# environment are forwarded. Subscription (/login) auth does not persist across
# runs — use API keys, or mount your own credential volume (see the docs).

set -euo pipefail

IMAGE="grok-sandbox"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCKERFILE="${REPO_ROOT}/docker/sandbox.Dockerfile"

rebuild=0
network="bridge" # default: network on (auth/model calls need it)
grok_args=()

while [ "$#" -gt 0 ]; do
    case "$1" in
        --rebuild) rebuild=1; shift ;;
        --no-network) network="none"; shift ;;
        -h|--help)
            grep '^#' "$0" | cut -c3-
            exit 0
            ;;
        --) shift; grok_args=("$@"); break ;;
        *) grok_args+=("$1"); shift ;;
    esac
done

if ! command -v docker >/dev/null 2>&1; then
    echo "error: docker is not installed or not on PATH" >&2
    exit 1
fi

# Build the image on first use or when asked. The runtime image COPYs nothing
# from context, so build with the small docker/ dir as context (not the whole
# repo) to keep the daemon transfer tiny.
if [ "$rebuild" -eq 1 ] || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "building $IMAGE from ${DOCKERFILE#"$REPO_ROOT"/} ..." >&2
    docker build -f "$DOCKERFILE" -t "$IMAGE" "$(dirname "$DOCKERFILE")"
fi

# Forward provider credentials that are actually set (never define empties).
env_args=()
for key in \
    XAI_API_KEY GROK_API_KEY ANTHROPIC_API_KEY OPENAI_API_KEY \
    GEMINI_API_KEY GOOGLE_API_KEY GROQ_API_KEY DEEPSEEK_API_KEY \
    TURAC_LLM_ROUTER_URL TURAC_LLM_ROUTER_KEY; do
    if [ -n "${!key:-}" ]; then
        env_args+=("-e" "${key}=${!key}")
    fi
done

# Isolation: drop all Linux capabilities, forbid privilege escalation, cap
# process count, and remove the container on exit. `--network none` (via
# --no-network) fully cuts egress for an offline/inspection run.
docker run --rm -it \
    --network "$network" \
    --cap-drop ALL \
    --security-opt no-new-privileges \
    --pids-limit 512 \
    -v "$PWD:/work:rw" \
    -w /work \
    "${env_args[@]}" \
    "$IMAGE" "${grok_args[@]}"
