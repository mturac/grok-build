# Sandbox runtime image for grok-build.
#
# Runs the agent inside a container so its built-in tools and `!` shell
# commands act on the container's filesystem/process space, not the host —
# isolation by containment, since grok-build has no in-process permission
# jail. Pattern ported from earendil-works/pi's containerization docs.
#
# This image installs the RELEASED `grok` CLI. To sandbox THIS fork's build
# instead, see docs/containerization.md (build-from-source variant).
#
# Build:  docker build -f docker/sandbox.Dockerfile -t grok-sandbox .
# Run:    scripts/sandbox.sh   (wraps `docker run` with sane mounts/limits)

FROM debian:12-slim

# Runtime deps: TLS certs + a shell/git/ripgrep the agent commonly shells out to.
# `--no-install-recommends` keeps the surface small; lists are pruned after.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl git ripgrep bash \
    && apt-get clean \
    && find /var/lib/apt/lists -mindepth 1 -delete

# Non-root by default — a sandbox that runs as root defeats the purpose.
RUN useradd --create-home --shell /bin/bash agent
USER agent
ENV HOME=/home/agent
# grok's installer links the binary into ~/.grok/bin.
ENV PATH=/home/agent/.grok/bin:$PATH

# Install the released grok CLI. Fails the build if the binary isn't runnable,
# so a broken image never ships silently.
RUN curl -fsSL https://x.ai/cli/install.sh | bash \
    && grok --version

# The workspace is bind-mounted here at run time (see scripts/sandbox.sh).
WORKDIR /work

ENTRYPOINT ["grok"]
