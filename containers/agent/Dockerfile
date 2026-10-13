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

# tini reaps the processes an agent's shell commands leave behind.
#
# md2pdf turns the agent's Markdown into a web page with python3-markdown and has
# Chromium print it. chromium-headless-shell is Chromium without its window: it prints
# the same page as the whole browser in a third of the memory, and the container's
# memory is shared with Claude Code. For the same reason md2pdf makes large pictures
# smaller before Chromium sees them, with python3-pil. poppler-utils gives md2pdf the
# page count it reports. Chromium brings no fonts, so the ones here are what a PDF is
# set in.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git ripgrep python3 python3-markdown python3-pil xz-utils \
      tini poppler-utils chromium-headless-shell fonts-noto-cjk fonts-noto-color-emoji \
 && rm -rf /var/lib/apt/lists/*

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
# Switches off, by name, the skills that ship with Claude Code for working on code and on
# Claude Code itself, so the agent is shown ours and the few that suit a chat. It also
# has long conversations compacted, at a size that does not depend on the model's name.
COPY containers/agent/settings.json /etc/agent/settings.json
COPY containers/agent/md2pdf/md2pdf /usr/local/bin/md2pdf
COPY containers/agent/mcp-file /usr/local/bin/mcp-file
COPY containers/agent/md2pdf/style.css containers/agent/md2pdf/test_md2pdf.py \
     /etc/agent/md2pdf/
COPY --chown=agent:agent skills/ /home/agent/.claude/skills/
# Standing instructions for the agent. They go into Claude Code's own directory and not
# into /work: the runner starts it with `--setting-sources user`, under which a CLAUDE.md
# in the working directory is not loaded. /work is also saved and restored with each
# conversation, so a copy there would stay as it was when the conversation began.
COPY --chown=agent:agent containers/agent/CLAUDE_agent.md /home/agent/.claude/CLAUDE.md
# Claude keeps its transcripts and settings here, so it has to own all of it whichever
# way the builder created the directories above.
#
# The modes of everything copied from the checkout are set here too, and not taken from
# it. COPY keeps the mode a file has on the machine that builds the image, so a file
# closed to the other users of that machine, as an operator may want a system prompt to
# be, arrived closed to the agent, which is not root. Claude Code does not start
# without a prompt file or a settings file it was told to read.
RUN chown -R agent:agent /home/agent/.claude \
 && chmod -R u=rwX,go=rX /etc/claude-code /etc/agent \
      /home/agent/.claude/skills /home/agent/.claude/CLAUDE.md \
 && chmod 755 /usr/local/bin/md2pdf /usr/local/bin/mcp-file

USER agent
# Fail the build, not a user's request, if the PDF toolchain does not fit together.
# md2pdf's own tests come first, run with the libraries and the Chromium of this image:
# md2pdf runs without the user being asked, and they are what says that nothing in a
# Markdown file reaches Chromium but text, a short list of tags and pictures that are
# files here, none of them larger than the container has memory for.
RUN python3 /etc/agent/md2pdf/test_md2pdf.py
# Then one document through the command as the agent runs it. It has to come out as a
# page, set in the font the style sheet names first. The name is looked for in the PDF
# itself: Chromium writes these fonts as Type 3, and the pdffonts of this Debian shows
# no name for one of those. Chromium is started twice for the document where the
# builder allows no sandbox, as md2pdf does wherever that is so.
RUN printf '<svg xmlns="http://www.w3.org/2000/svg" width="900" height="1600"/>' > /tmp/check.svg \
 && printf '# Check\n\n| a | b |\n|---|---|\n| 中文 | ok |\n\n![check](check.svg)\n' > /tmp/check.md \
 && md2pdf /tmp/check.md check.pdf | grep -F '(1 page)' \
 && grep -a -q '/FontName /[A-Z+]*NotoSansCJK' /work/out/check.pdf \
 && rm /tmp/check.md /tmp/check.svg /work/out/check.pdf
# The file helper has to be runnable, and to refuse what is not a file before it
# contacts anything.
RUN mcp-file get .. 2>/dev/null; [ "$?" -eq 1 ]
# Claude Code ignores a compaction window it finds too small, without a word, and then
# lets conversations grow to whatever it takes the model's own window to be.
RUN python3 -c 'import json; s = json.load(open("/etc/agent/settings.json")); assert s.get("autoCompactEnabled") is True and s.get("autoCompactWindow", 0) >= 100000, "settings.json: Claude Code ignores an autoCompactWindow below 100000"'
ENV HOME=/home/agent \
    PATH=/home/agent/.local/bin:/usr/local/bin:/usr/bin:/bin \
    CLAUDE_CONFIG_DIR=/home/agent/.claude
# Claude Code's installer is piped into a shell further up, and a pipe ends well if its
# last command does: a download that failed leaves an image without Claude Code, in which
# every turn ends at "failed to start claude". It is looked for the way the runner finds
# it, on the PATH above.
RUN command -v claude > /dev/null || { echo "Claude Code was not installed" >&2; exit 1; }
# Last, so that it covers every COPY above: fail the build, not every request, if the
# agent has been given a file it cannot read. COPY makes a file root's whichever USER
# is in effect, and keeps the mode it has in the checkout.
RUN closed=$(find /etc/claude-code /etc/agent /home/agent/.claude /work ! -readable) \
 && [ -z "$closed" ] || { echo "the agent cannot read: $closed" >&2; exit 1; }
WORKDIR /work
EXPOSE 8080
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/runner"]
