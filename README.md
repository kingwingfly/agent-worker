# agent-worker

A hosted agent: people chat with a Telegram bot, and an unmodified [Claude Code](https://code.claude.com/docs) CLI does the work inside a Cloudflare container, using a shared MCP server and a set of skills. Users install nothing. Containers exist only while someone is being answered.

```
Telegram ──webhook──▶ Worker
                        ├─ /tg/webhook ─▶ SessionDO (one per chat) ──WebSocket──▶ agent container
                        │                   SQLite: session, open prompts,          runner
                        │                   usage. R2: state, output files            └─ claude -p
                        ├─ /llm/*  ◀───── model calls, with a session token ───────────────┘  │
                        │     └─▶ your LLM gateway (real key added here)                       │
                        └─ /mcp    ◀───── MCP calls, with a session token ─────────────────────┘
                              └─▶ McpHostDO ──▶ MCP container (shared by all chats)
```

| Path | What it is |
|---|---|
| `crates/proto` | Types shared by both sides: WebSocket frames, session tokens |
| `crates/runner` | Runs in the agent container. Launches `claude`, relays its output, turns permission prompts into remote requests, packs up state |
| `crates/gateway` | The Worker: Telegram webhook, `/llm` and `/mcp` proxies, `SessionDO`, `McpHostDO` |
| `crates/mcp-example` | Placeholder MCP server so the stack deploys before yours is wired in |
| `containers/agent` | Agent image: runner, pinned Claude Code, `md2pdf`, permission policy, system prompt |
| `containers/mcp` | MCP image. Builds the placeholder; point it at your server |
| `skills/` | Skills baked into the agent image |

## How a message is handled

1. Telegram calls `/tg/webhook`. The Worker checks the webhook secret and the user allow-list and hands the update to the chat's `SessionDO`.
2. The session starts its container if needed, restores the previous files and transcript from R2, and opens a WebSocket to the runner.
3. The runner feeds the message to a long-lived `claude -p --input-format stream-json` and streams events back. The session shows the tools Claude uses, and what it says before using them, in one progress message that it edits as it goes and leaves in the chat.
4. A tool call that the policy does not pre-approve reaches the runner's `permission_prompt` MCP tool (`--permission-prompt-tool`). It becomes a Telegram message with **Allow / Always / Deny** buttons; Claude waits for the answer. `AskUserQuestion` arrives the same way and is shown as option buttons.
5. When the turn ends, the answer is sent, files written to `/work/out/` are delivered as documents, and the state is saved to R2. The container is stopped after `IDLE_TTL_SECS`.

An alarm re-checks every 30 seconds while a turn is open, so an evicted Durable Object or a crashed container is picked up and the turn is run again. A new container is given the saved state before it runs anything; if that fails the turn is refused rather than run on an empty disk, and the saved copy is left as it is.

## Develop

```sh
cargo test --workspace          # the runner's tests use a scripted stand-in for claude
```

### Try the runner with the real CLI

```sh
RUNNER_WORK_DIR=/tmp/agent/work RUNNER_RUNTIME_DIR=/tmp/agent/run \
  RUNNER_LISTEN=127.0.0.1:8080 cargo run -p agent-runner
# in another terminal: plays the Durable Object's part
cargo run -p agent-runner --example drive -- "Create out/hello.md containing one line: hi"
```

`drive` approves permission requests (`--deny` to decline, `--interrupt-after-ms N` to interrupt, `--raw` for every frame). Without `AGENT_WORKER_ORIGIN` the CLI uses whatever credentials it already has.

### Run the Worker without a container engine

`wrangler dev` needs Docker to run containers. To work on the Worker and session logic without it, start a runner by hand and point every session at it:

```sh
RUNNER_LISTEN=127.0.0.1:8080 RUNNER_WORK_DIR=/tmp/agent/work RUNNER_RUNTIME_DIR=/tmp/agent/run \
  cargo run -p agent-runner &
npx wrangler dev --enable-containers=false \
  --var DEV_RUNNER_URL:http://127.0.0.1:8080 --var ALLOWED_TG_USERS:'*' \
  --var TELEGRAM_BOT_TOKEN:… --var TELEGRAM_WEBHOOK_SECRET:… \
  --var LLM_API_KEY:… --var SESSION_TOKEN_SECRET:…
```

`DEV_RUNNER_URL` is for development only: all chats share that one runner. `TELEGRAM_API_BASE` redirects Bot API calls, for a mock or a self-hosted Bot API server.

## Deploy

Needs a Workers Paid plan, Docker or podman for building the images, and `cargo install worker-build`.

With podman, put `WRANGLER_DOCKER_BIN="$PWD/containers/docker-via-podman"` in front of the `wrangler deploy` command below. The wrapper adapts what podman does differently from Docker: two `build` arguments, and the image digest reported after a push, which wrangler uses to tell Cloudflare what to run. Rootless podman needs subordinate id ranges for your user (`grep "^$USER:" /etc/subuid /etc/subgid`), or installing packages inside the image build fails.

1. Edit `[vars]` in `wrangler.toml`: `PUBLIC_ORIGIN`, `LLM_BASE_URL`, `MODEL`, `SMALL_MODEL`, `ALLOWED_TG_USERS`. `PUBLIC_ORIGIN` must be an address the Worker is really served at, because the agent containers call it for every model and MCP request: either its `workers.dev` address, or your own domain declared under `routes` with `workers_dev = false`.
2. Create the bucket and set the secrets:
   ```sh
   npx wrangler r2 bucket create agent-worker-state
   npx wrangler secret put TELEGRAM_BOT_TOKEN
   npx wrangler secret put TELEGRAM_WEBHOOK_SECRET   # any random string
   npx wrangler secret put LLM_API_KEY
   npx wrangler secret put SESSION_TOKEN_SECRET      # any long random string
   ```
3. `npx wrangler deploy`. The first deploy builds and pushes both images, which takes a few minutes.
4. Register the webhook with the same secret:
   ```sh
   curl "https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/setWebhook" \
     -d url="$PUBLIC_ORIGIN/tg/webhook" -d secret_token="$TELEGRAM_WEBHOOK_SECRET" \
     -d 'allowed_updates=["message","callback_query"]'
   ```
5. Message the bot. If you are not on the allow-list it replies with your user id; add it to `ALLOWED_TG_USERS` and deploy again.

### The LLM gateway

It must expose the Anthropic Messages format: `POST /v1/messages` with unbuffered SSE, the `anthropic-version` and `anthropic-beta` headers forwarded, and `cache_control` left in place (otherwise every turn is billed uncached). Claude Code is only supported with Claude models. If the gateway rejects beta fields, add `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1` to the agent image's environment.

### Your MCP server

Replace `containers/mcp/Dockerfile` with a build of your server, or set `image` in `wrangler.toml` to a registry image. It must listen on `0.0.0.0:8080` and serve streamable HTTP at `/mcp`. Requests arrive with `Host: localhost:8080` and an `x-agent-session` header naming the chat, set by the Worker from the verified token.

The container's disk and memory are lost when it stops (`MCP_IDLE_TTL_SECS` after the last request). If your server's shared state cannot be rebuilt, persist it elsewhere or keep the container up.

### What the agent may do

`containers/agent/managed-settings.json` is the permission policy and `crates/runner/src/config.rs` (`DEFAULT_TOOLS`) the list of built-in tools. As shipped: files under `/work` can be read and edited freely, the `domain` MCP server and `md2pdf` are pre-approved, read-only shell commands run without asking, and every other shell command needs a button press. Web access, subagents, background tasks and scheduling are off.

A prompt can be answered with **Always**. For a shell command that covers the identical command only; for any other tool it covers every later use of that tool, and the button says which. The session keeps these answers, so they last until `/new` even though the container is replaced after every idle period.

### PDFs

The agent never writes a typesetting language. It writes Markdown and runs `md2pdf report.md`, which renders `/work/out/report.pdf` with pandoc and Typst, so the result does not depend on how capable the model is. Everything about the look is in `containers/agent/md2pdf/`:

- `header.typ`: fonts and styling. Put the Noto CJK variant for your users first in the fallback list (`SC`, `TC`, `JP` or `KR`).
- `defaults.yaml`: paper size, margins, font size, link colour.
- `tidy.lua`: repairs for Markdown as models tend to write it. `<br>` becomes a line break, real HTML tags are removed, other text in angle brackets such as `<customer name>` is printed, and rows of `| a | b |` written without a header become a table instead of text full of pipes.

A line break in the Markdown is a line break in the PDF (`hard_line_breaks`), as in a chat window, so an address block written one item per line stays that way.

To try a change without building the image, install pandoc and Typst and run `MD2PDF_HOME=containers/agent/md2pdf MD2PDF_OUT_DIR=. containers/agent/md2pdf/md2pdf some.md`.

## Configuration

| Variable | Default | |
|---|---|---|
| `PUBLIC_ORIGIN` | required | Where containers reach the Worker |
| `LLM_BASE_URL` | required | Gateway base URL, without `/v1` |
| `LLM_AUTH_STYLE` | `bearer` | `bearer` or `x-api-key` |
| `MODEL`, `SMALL_MODEL` | CLI default | Model ids as the gateway names them |
| `ALLOWED_TG_USERS` | nobody | Comma separated user ids, or `*` |
| `IDLE_TTL_SECS` | 120 | How long a container stays warm after answering |
| `APPROVAL_TIMEOUT_SECS` | 600 | Unanswered prompts are denied after this |
| `TURN_TIMEOUT_SECS` | 1800 | A turn is interrupted after this |
| `DAILY_TOKEN_BUDGET` | 0 (off) | Fresh tokens per chat per UTC day, counted at the `/llm` proxy from what each model call reports |
| `MCP_IDLE_TTL_SECS` | 900 | Idle time before the MCP container stops |

## Security model

- A container holds one credential: a session token that expires after 24 hours and is only accepted by this Worker's `/llm` and `/mcp` routes. The gateway key and bot token never leave the Worker.
- The runner's control API needs a secret the session sets before any agent process exists, and the runner makes its own memory unreadable to the agent, so the agent cannot approve its own prompts.
- The permission policy is a root-owned managed settings file. Settings the agent writes in its working directory are not loaded.
- Containers currently have outbound internet access. An approved shell command can therefore send data anywhere; keep the pre-approved list short.

## Upgrading Claude Code

The runner relies on `--permission-prompt-tool`, `--input-format stream-json`, `--replay-user-messages` and `--resume`. After changing `CLAUDE_CODE_VERSION`, run the runner against the new CLI and check with `drive` that: a non-approved Bash command produces a permission request and runs when allowed; a second prompt in the same run gets its own result; an interrupt ends the turn and the next prompt still works; restarting the runner resumes the conversation.

## Not built yet

- Web UI (Leptos) on the same session API.
- Locking containers down to the Worker only (`enableInternet: false` with outbound interception).
- Letting a container sleep while a prompt waits (`defer`), and filesystem snapshots instead of the tar archive.
