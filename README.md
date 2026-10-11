# agent-worker

A hosted agent: people chat with a Telegram bot, and an unmodified [Claude Code](https://code.claude.com/docs) CLI does the work inside a Cloudflare container, using a shared MCP server and a set of skills. Users install nothing. Containers exist only while someone is being answered.

```
Telegram ──webhook──▶ Worker
                        ├─ /tg/webhook ─▶ SessionDO (one per chat) ──WebSocket──▶ agent container
                        │                   SQLite: session, open prompts,          runner
                        │                   usage. R2: state, output files            └─ claude -p
                        ├─ /llm/*  ◀───── model calls, with a session token ───────────────┘   │
                        │     └─▶ your LLM gateway (real key added here)                       │
                        └─ /mcp, /file ◀─ MCP calls and file downloads, with a session token ──┘
                              └─▶ McpHostDO ──▶ MCP container (shared by all chats)
                                                  mcp-front ─┬─▶ the MCP server
                                                             └─▶ a file server
```

| Path | What it is |
|---|---|
| `crates/proto` | Types shared by both sides: WebSocket frames, session tokens |
| `crates/runner` | Runs in the agent container. Launches `claude`, relays its output, turns permission prompts into remote requests, packs up state |
| `crates/gateway` | The Worker: Telegram webhook, `/llm` and `/mcp` proxies, `SessionDO`, `McpHostDO` |
| `crates/mcp-front` | Runs in the MCP container, in front of the MCP server: starts it, serves the files it saves, and routes requests between the two |
| `crates/mcp-example` | Placeholder MCP server so the stack deploys before yours is wired in |
| `containers/agent` | Agent image: runner, pinned Claude Code, `md2pdf`, `mcp-file`, permission policy, system prompt |
| `containers/mcp` | MCP image: `mcp-front` and the placeholder; point it at your server |
| `skills/` | Skills baked into the agent image |

## How a message is handled

1. Telegram calls `/tg/webhook`. The Worker checks the webhook secret and the user allow-list and hands the update to the chat's `SessionDO`.
2. The session starts its container if needed, restores the previous files and transcript from R2, and opens a WebSocket to the runner.
3. The runner feeds the message to a long-lived `claude -p --input-format stream-json` and streams events back. The session shows the tools Claude uses, and what it says before using them, in one progress message that it edits as it goes and leaves in the chat. When a prompt or a message of the user's has pushed it up the chat, it is sent again below them, so the newest message is the one that changes.
4. A tool call that the policy does not pre-approve reaches the runner's `permission_prompt` MCP tool (`--permission-prompt-tool`). It becomes a Telegram message with **Allow / Always / Deny** buttons; Claude waits for the answer. `AskUserQuestion` arrives the same way and is shown as option buttons.
5. When the turn ends, the answer is sent, every file created or changed under `/work/out/` during the turn is delivered as a document, and the state is saved to R2. The container is stopped after `IDLE_TTL_SECS`.

### Conversations and going back

The state saved after each turn is a checkpoint of its own: the transcript and `/work` as they were then, at `sessions/<chat>/<conversation>/<time>.tar.gz`. That is what the bot's three commands for conversations work on.

- `/rewind` lists your recent messages. Picking one puts the conversation and the files back to how they were before you sent it; that turn and the later ones are forgotten and their archives deleted.
- `/new` starts an empty conversation and keeps the one you leave.
- `/resume` lists the kept conversations, and picking one continues it where it was left.

All three replace the container, so the next message starts one from the checkpoint. A chat keeps its 10 most recently used conversations and each conversation its 10 newest checkpoints, which is 9 turns that can be undone (`CONVERSATIONS_KEPT` and `CHECKPOINTS_KEPT` in `session.rs`); older ones are deleted from R2. Nothing deletes a conversation on request.

An alarm re-checks every 30 seconds while a turn is open, so an evicted Durable Object or a crashed container is picked up and the turn is run again. A new container is given the saved state before it runs anything; if that fails the turn is refused rather than run on an empty disk, and the saved copy is left as it is.

### Long conversations

Claude Code compacts a conversation on its own when it gets long: the older part is replaced by a summary and the turn goes on. `containers/agent/settings.json` makes sure of that in two ways.

- `autoCompactEnabled` switches it on in a file the agent cannot write. Left unsaid it is on as well, but then a settings file in the agent's home directory can switch it off.
- `autoCompactWindow` is the size it works from, 128,000 tokens. Compaction starts 33,000 below it, at 95,000. Without the setting Claude Code goes by the window it believes the model to have: 1,000,000 tokens for the current Claude models, so that it would compact at 967,000, and 200,000 for a model it does not know, whatever that model's window really is.

Choose the window for the model in `MODEL`, and look at it again when that changes. It has to be no more than the context your gateway really gives the model. If the model costs more per token from some prompt size on, stay well below that size too: a turn is only checked between model calls, so one large tool result can carry a request some tens of thousands of tokens past the point where compaction starts. A smaller window also means less to pay for with every message, and more summaries. Claude Code takes nothing below 100,000 and ignores a smaller value without saying so, which would leave the model's own window in force; the image build fails on such a value for that reason.

Claude Code measures a conversation by the token usage the gateway reports with each answer, the same reports the daily budget is counted from. A gateway that reports none leaves it to estimate.

The summary is one more model call inside the turn that crosses the line. That turn takes longer, and the call is metered like any other. A conversation whose single exchange is already over the line cannot be compacted, because there is no older part to summarise.

If the gateway refuses a request as too long before that point, Claude Code compacts and tries again, as long as the refusal is worded the way the Anthropic API words it (`prompt is too long: N tokens > M maximum`). The `/llm` proxy passes on exactly that wording and nothing else of the refusal.

### Effort

`/effort` is Claude Code's command of that name. On its own it lists the levels as buttons (`low`, `medium`, `high`, `xhigh`, `max`, and `auto` for the model's default), and `/effort high` sets one directly. The level belongs to the chat: the session keeps it, so it outlasts containers and `/new`, and `/status` shows it.

The session hands the level to the runner with each message. A new `claude` is launched with `--effort`. One that is already running at another level is told just before the message, with the stream-json control request `apply_flag_settings` (`effortLevel`), so nothing is restarted and a change costs no time. Claude Code puts the level into every model call as `output_config.effort`, and it does that without `/effort` too, with its own default for the model; what a level changes is up to the model behind the gateway. A higher level uses more tokens, which count against `DAILY_TOKEN_BUDGET` like any others.

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

`drive` approves permission requests (`--deny` to decline, `--interrupt-after-ms N` to interrupt, `--effort LEVEL` to answer at a level, `--raw` for every frame). Without `AGENT_WORKER_ORIGIN` the CLI uses whatever credentials it already has.

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

`DEV_RUNNER_URL` is for development only: all chats share that one runner, and nothing replaces it. `/new`, `/resume` and `/rewind` only take effect on it once you restart it with an empty working directory and `AGENT_CLAUDE_SESSION_ID` set to the conversation's id. `TELEGRAM_API_BASE` redirects Bot API calls, for a mock or a self-hosted Bot API server.

The MCP container can be stood in for the same way. Start what runs inside it, then add `--var DEV_MCP_URL:http://127.0.0.1:8090` to the `wrangler dev` command:

```sh
cargo build -p agent-mcp-example
MCP_FRONT_LISTEN=127.0.0.1:8090 MCP_FRONT_FILES_DIR=/tmp/agent/files \
  cargo run -p agent-mcp-front -- target/debug/agent-mcp-example
```

## Deploy

Needs a Workers Paid plan, Docker or podman for building the images, and `cargo install worker-build`.

With podman, put `WRANGLER_DOCKER_BIN="$PWD/containers/docker-via-podman"` in front of the `wrangler deploy` command below. The wrapper adapts what podman does differently from Docker: two `build` arguments, and the image digest reported after a push, which wrangler uses to tell Cloudflare what to run. Rootless podman needs subordinate id ranges for your user (`grep "^$USER:" /etc/subuid /etc/subgid`), or installing packages inside the image build fails.

1. Edit `[vars]` in `wrangler.toml`: `PUBLIC_ORIGIN`, `LLM_BASE_URL`, `MODEL`, `SMALL_MODEL`, `ALLOWED_TG_USERS`. For a model other than the default, see "Long conversations" above for the size at which conversations are compacted. `PUBLIC_ORIGIN` must be an address the Worker is really served at, because the agent containers call it for every model and MCP request: either its `workers.dev` address, or your own domain declared under `routes` with `workers_dev = false`.
2. Create the bucket and set the secrets:
   ```sh
   npx wrangler r2 bucket create agent-worker-state
   npx wrangler secret put TELEGRAM_BOT_TOKEN
   npx wrangler secret put TELEGRAM_WEBHOOK_SECRET   # any random string
   npx wrangler secret put LLM_API_KEY
   npx wrangler secret put SESSION_TOKEN_SECRET      # a random string of 16 characters or more
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

The `/llm` proxy passes on `POST /v1/messages` and `POST /v1/messages/count_tokens` and nothing else, which is all Claude Code asks for; any other request is answered with a 404 of the Worker's own. With `MODEL` and `SMALL_MODEL` both set, a request for any other model is refused with a 403. Both refusals are logged with the chat they came from, which is where to look if a later version of Claude Code needs something more (`crates/gateway/src/calls.rs`).

### Your MCP server

The MCP container's entry point is `mcp-front`, not the MCP server. It starts the server given as its command line, and is the only thing listening on the container's open port, `0.0.0.0:8080`. Behind it, on loopback:

| | Default | Reached by |
|---|---|---|
| your MCP server | `127.0.0.1:8081` (`MCP_FRONT_MCP_ADDR`) | every request except `/file` |
| the file server | `127.0.0.1:8082` (`MCP_FRONT_FILES_ADDR`) | `/file` |

To run your own server, put its binary in the image built by `containers/mcp/Dockerfile` and name it as the command after `mcp-front`. It must serve streamable HTTP at `/mcp` on the loopback address above; `mcp-front` sets `MCP_FRONT_MCP_ADDR` in its environment for servers that can read it. Requests arrive with `Host: localhost:8080` and an `x-agent-session` header naming the chat, set by the Worker from the verified token.

The port opens only once the MCP server accepts connections, and if the MCP server exits so does `mcp-front`, which stops the container.

The container's disk and memory are lost when it stops (`MCP_IDLE_TTL_SECS` after the last request). If your server's shared state cannot be rebuilt, persist it elsewhere or keep the container up.

#### Files

The MCP container and the agent containers do not share a disk, so the path of a file your server saved means nothing to an agent as it is. The file server turns that path into a download. A tool that makes a file has to do two things: save it under `MCP_FRONT_FILES_DIR` (also set in the server's environment), and return its path.

```
GET /file?path=<the path the tool returned>
```

Only regular files inside that directory are served, wherever symlinks or `..` in the path lead; everything else is a 404. The Worker forwards `/file` like `/mcp`, after checking the session token. Every chat can fetch every file in the directory.

The agent does not build that request itself. Its image has `mcp-file`, pre-approved like `md2pdf`, and `skills/mcp-files` tells it when to use it:

```sh
mcp-file get '<path>'       # /work/in/<file name>, for the agent to read or use
mcp-file deliver '<path>'   # /work/out/<file name>, which is sent to the user
```

Files over 20 MB are refused (`MCP_FILE_MAX_BYTES` in the agent image's environment), because everything under `/work` is saved with the conversation after each turn. Only fetching exists: there is no way yet for an agent to hand a file to the MCP server.

### What the agent may do

`containers/agent/managed-settings.json` is the permission policy and `crates/runner/src/config.rs` (`DEFAULT_TOOLS`) the list of built-in tools. As shipped: files under `/work` can be edited freely, and the `domain` MCP server, `md2pdf` and `mcp-file` are pre-approved. Reading needs no approval anywhere in the container (`Read(//**)`), with the Read tool or a read-only shell command such as `ls`, `head` or `wc`; without that rule Claude Code asks about every read outside `/work`, which includes the files a skill brings with it. Every other shell command needs a button press, `python3` included. Web access, subagents, background tasks and scheduling are off, and so are the skills that ship with Claude Code (`containers/agent/settings.json`): the agent sees only the ones in `skills/`.

A prompt can be answered with **Always**. For a shell command that covers the identical command only; for any other tool it covers every later use of that tool, and the button says which. The session keeps these answers, so they last until `/new` or `/resume` even though the container is replaced after every idle period.

A prompt shows the first 1,500 characters of what is asked for: the command, or the tool's input with its shortest fields first, so that a path or an address comes before the content sent to it. A request longer than that is also sent whole, as `request.txt` just above the prompt, because the answer covers all of it. If that file cannot be sent, the request is declined without asking.

### PDFs

The agent never writes a typesetting language. It writes Markdown and runs `md2pdf report.md`, which renders `/work/out/report.pdf` with pandoc and Typst, so the result does not depend on how capable the model is. Because it runs without asking, its arguments can do nothing else: the PDF goes to `/work/out` whatever directory the output name is given with, and a Markdown file whose name begins with a dash is refused, since pandoc would read the name as an option. Everything about the look is in `containers/agent/md2pdf/`:

- `header.typ`: fonts and styling. Put the Noto CJK variant for your users first in the fallback list (`SC`, `TC`, `JP` or `KR`). It also lets a table that is taller than a third of the page run on over the following pages, with its header row repeated, and scales a picture down to the width of the text and a third of the page's height; smaller pictures keep their size.
- `defaults.yaml`: paper size, margins, font size, link colour.
- `tidy.lua`: repairs for Markdown as models tend to write it. `<br>` becomes a line break, real HTML tags are removed, other text in angle brackets such as `<customer name>` is printed, and rows of `| a | b |` written without a header become a table instead of text full of pipes.

A line break in the Markdown is a line break in the PDF (`hard_line_breaks`), as in a chat window, so an address block written one item per line stays that way.

Nothing asks the user before a file in `/work/out` is sent, and it does not matter how it got there: a prompt for `mv notes.md /work/out` is a prompt for a shell command, and says nothing about delivery. So the agent is told to keep its working files in `/work`, by the system prompt, by the skill and by `md2pdf` itself, which says when it has written a PDF that the user is about to receive it and that nothing is left to move.

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
| `DAILY_TOKEN_BUDGET` | 0 (off) | Fresh tokens per chat per UTC day, counted at the `/llm` proxy from what each model call reports. Checked when a message arrives and before every model call, so a turn that uses it up ends there |
| `MCP_IDLE_TTL_SECS` | 900 | Idle time before the MCP container stops |

## Security model

- A container holds one credential: a session token that is only accepted by this Worker's `/llm`, `/mcp` and `/file` routes. The gateway key and bot token never leave the Worker.
- The agent can read that token, so holding it is not enough. Before acting on one, the proxy asks the chat's session, which refuses a token that is not the one its running container was started with: a token stops working when its container is stopped or replaced, well before its 24 hours are up. For a model call the session also refuses once the day's budget is used up, and the proxy passes on only the two requests Claude Code makes, for the two configured models. What is left to someone who copies a token out is what the agent itself could do: model calls on the chat's budget and the chat's MCP calls, while the container runs.
- The runner's control API needs a secret the session sets before any agent process exists, and the runner makes its own memory unreadable to the agent, so the agent cannot approve its own prompts.
- The permission policy is a root-owned managed settings file. Settings the agent writes in its working directory are not loaded.
- Containers currently have outbound internet access. An approved shell command can therefore send data anywhere; keep the pre-approved list short. A command on that list runs with whatever arguments the agent gives it, so it has to be safe with all of them, as `md2pdf` and `mcp-file` are meant to be. The `domain` MCP server is pre-approved as a whole: if its tools return text written by third parties, that text can steer the agent into calling them, and no prompt stands in between.
- A permission prompt shows what is being approved, and all of it: a request too long for the message is attached as a file. The file of an output document is uploaded to Telegram under a boundary made up for that upload, so its content cannot add to the request.
- `SESSION_TOKEN_SECRET` must be at least 16 characters and no secret may be empty; the Worker answers 500 to everything until that is so. A short signing key could be found by trying keys against a token, and an empty webhook secret is what a request without the header presents.
- Nothing a failed model call says reaches a container or the chat. A gateway's error names its site and quotes the ids it knows the request by, and Claude Code prints such an error into its answer word for word. So the `/llm` proxy rebuilds every error from its status and its kind (`crates/gateway/src/scrub.rs`): an error status, an `error` event inside a stream, and a status 200 whose body is an error or a page of the gateway's. It also drops every response header but the five that say what the body is and when to retry. The gateway's own words go to the Worker's log, with the chat they belong to, which is where to look when a user reports "The model service had an internal error". The bot, for its part, never shows this Worker's host, which Claude Code adds to some of its messages.
- What a successful model call returns is passed on as it is, ids and model name included. It is not shown in the chat, but it is in the transcript inside the container, which the agent can read.

## Upgrading Claude Code

The runner relies on `--permission-prompt-tool`, `--input-format stream-json`, `--replay-user-messages`, `--resume`, `--effort` and the `apply_flag_settings` control request. After changing `CLAUDE_CODE_VERSION`, run the runner against the new CLI and check with `drive` that: a non-approved Bash command produces a permission request and runs when allowed; a second prompt in the same run gets its own result; an interrupt ends the turn and the next prompt still works; restarting the runner resumes the conversation; a `drive --raw --effort high` after a run without `--effort` shows a `control_response` whose `subtype` is `success`.

Check that `claude -p /context --settings containers/agent/settings.json` still reports a window of 128k and an autocompact buffer: the two settings that keep long conversations compacted are read from that file, and a setting Claude Code no longer knows is ignored without a word.

Check as well that `claude --help` still lists the levels of `Effort` in `crates/proto` for `--effort`. The CLI accepts a level it does not know without an error and goes on at its default, so a renamed level would not show anywhere else.

## Not built yet

- Web UI (Leptos) on the same session API.
- Locking containers down to the Worker only (`enableInternet: false` with outbound interception).
- Sending a file from an agent to the MCP server. Files only travel the other way.
- Letting a container sleep while a prompt waits (`defer`), and filesystem snapshots instead of the tar archive.
