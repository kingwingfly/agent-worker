# The agent container: the runner, the Claude Code CLI it drives, and the tools the
# skills rely on. The build context is the repository root.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p agent-runner --bin runner

FROM debian:bookworm-slim
# The runner depends on how specific CLI flags behave. When changing the version, run
# the checks under "Upgrading Claude Code" in the README.
ARG CLAUDE_CODE_VERSION=2.1.296
# md2pdf turns the agent's Markdown into PDFs with pandoc, which typesets through Typst.
# The two are pinned together: pandoc writes Typst source for the Typst of its day.
ARG PANDOC_VERSION=3.12.1
ARG TYPST_VERSION=0.15.1

# tini reaps the processes an agent's shell commands leave behind. poppler-utils gives
# md2pdf the page count it reports. The fonts are for PDFs: Typst ships Latin fonts only.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git ripgrep python3 xz-utils tini poppler-utils \
      fonts-noto-cjk fonts-noto-color-emoji \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL "https://github.com/typst/typst/releases/download/v${TYPST_VERSION}/typst-x86_64-unknown-linux-musl.tar.xz" \
  | tar -xJ --strip-components=1 -C /usr/local/bin typst-x86_64-unknown-linux-musl/typst \
 && curl -fsSL "https://github.com/jgm/pandoc/releases/download/${PANDOC_VERSION}/pandoc-${PANDOC_VERSION}-linux-amd64.tar.gz" \
  | tar -xz --strip-components=2 -C /usr/local/bin "pandoc-${PANDOC_VERSION}/bin/pandoc"

# Everything the agent user writes at run time is under /work or its home directory.
# Nothing may be prepared under /run: Cloudflare mounts a fresh one over it when the
# container starts, and the agent user cannot create directories there.
RUN useradd --create-home --uid 1000 agent \
 && mkdir -p /work/out \
 && chown -R agent:agent /work

USER agent
RUN curl -fsSL https://claude.ai/install.sh | bash -s -- "${CLAUDE_CODE_VERSION}"
USER root

COPY --from=build /src/target/release/runner /usr/local/bin/runner
# Root-owned, so the agent cannot rewrite its own permissions or instructions.
COPY containers/agent/managed-settings.json /etc/claude-code/managed-settings.json
COPY containers/agent/system.md /etc/agent/system.md
COPY containers/agent/md2pdf/md2pdf /usr/local/bin/md2pdf
COPY containers/agent/mcp-file /usr/local/bin/mcp-file
COPY containers/agent/md2pdf/defaults.yaml containers/agent/md2pdf/header.typ \
     containers/agent/md2pdf/tidy.lua /etc/agent/md2pdf/
COPY --chown=agent:agent skills/ /home/agent/.claude/skills/
# Claude keeps its transcripts and settings here, so it has to own all of it whichever
# way the builder created the directories above.
RUN chown -R agent:agent /home/agent/.claude

USER agent
# Fail the build, not a user's request, if the PDF toolchain does not fit together.
RUN printf '# Check\n\n| a | b |\n|---|---|\n| 中文 | ok |\n' > /tmp/check.md \
 && md2pdf /tmp/check.md /tmp/check.pdf \
 && rm /tmp/check.md /tmp/check.pdf
# The file helper has to be runnable, and to refuse what is not a file before it
# contacts anything.
RUN mcp-file get .. 2>/dev/null; [ "$?" -eq 1 ]
ENV HOME=/home/agent \
    PATH=/home/agent/.local/bin:/usr/local/bin:/usr/bin:/bin \
    CLAUDE_CONFIG_DIR=/home/agent/.claude
WORKDIR /work
EXPOSE 8080
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/runner"]
