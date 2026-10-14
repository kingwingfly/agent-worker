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
3. The runner feeds the message to a long-lived `claude -p --input-format stream-json` and streams events back. The session shows the tools Claude uses, and what it says before using them, in one progress message that it edits as it goes and leaves in the chat. When a prompt or a message of the user's has pushed it up the chat, it is sent again below them, so the newest message is the one that changes. In a private chat the text Claude is writing is shown as well, while it is written; see "Text as it is written".
4. A tool call that the policy does not pre-approve reaches the runner's `permission_prompt` MCP tool (`--permission-prompt-tool`). It becomes a Telegram message with **Allow / Always / Deny** buttons; Claude waits for the answer. `AskUserQuestion` arrives the same way and is shown as option buttons.
5. When the turn ends, the answer is sent, every file created or changed under `/work/out/` during the turn is delivered as a document, and the state is saved to R2. The container is stopped after `IDLE_TTL_SECS`.

### Conversations and going back

The state saved after each turn is a checkpoint of its own: the transcript and `/work` as they were then, at `sessions/<chat>/<conversation>/<time>.tar.gz`. Files, directories and links are in it; a socket or a pipe that a program left in `/work`, and a file its own user may not read, are left out and named in the runner's log. That is what the bot's three commands for conversations work on.

- `/rewind` lists your recent messages. Picking one puts the conversation and the files back to how they were before you sent it; that turn and the later ones are forgotten and their archives deleted.
- `/new` starts an empty conversation and keeps the one you leave.
- `/resume` lists the kept conversations, and picking one continues it where it was left.

All three replace the container, so the next message starts one from the checkpoint. A chat keeps its 10 most recently used conversations and each conversation its 10 newest checkpoints, which is 9 turns that can be undone (`CONVERSATIONS_KEPT` and `CHECKPOINTS_KEPT` in `session.rs`); older ones are deleted from R2. Nothing deletes a conversation on request.

An alarm re-checks every 30 seconds while a turn is open, so an evicted Durable Object or a crashed container is picked up and the turn is run again. A turn is run in three containers at most (`MAX_TURN_RUNS` in `session.rs`): when the third goes away too, as happens to a task that needs more memory than a container has, the turn ends and the chat is told. A new container is given the saved state before it runs anything; if that fails the turn is refused rather than run on an empty disk, and the saved copy is left as it is.

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

### Text as it is written

In a private chat the bot shows what Claude is writing as a draft (`sendMessageDraft`): the bubble Telegram draws for a message that is still being typed, with a Stop button on it. Telegram's Bot API has drafts for every bot since version 9.5 and the button since 10.3. A self-hosted server from before that refuses the call, and the turn then goes on without drafts.

- The runner starts `claude` with `--include-partial-messages`, which prints the model's response a few characters at a time. It does not pass those pieces on. It keeps the text so far and sends that, at most every 0.7 seconds, in a frame that is neither numbered nor replayed: a long answer comes in thousands of pieces, and each of them is worth nothing once the next has arrived.
- A draft is a preview that Telegram removes by itself. The answer is still sent as a message when the turn is over, exactly as without drafts, and that is what stays in the chat.
- Text that turns out to come before a tool call is not the answer. It moves into the progress message, as it always did, and the draft is left as Telegram's "Thinking…" placeholder.
- The Stop button does what `/stop` does. Telegram reports the press as a `stopped_message_generation` update, so the webhook has to be registered for it (step 4 under "Deploy").

Groups have no drafts, and neither has a chat the bot is a guest in.

### As a guest in other chats

With Guest Mode switched on for the bot in @BotFather, someone on the allow-list can summon it in any chat without adding it there: by mentioning it in a message, or by replying to one of its messages. Telegram hands the bot that one message (`guest_message`) and the message it replies to, and lets it answer with one message of its own, which it may change afterwards. It sees nothing else of the chat and can send nothing else there. Everything about a guest's turn follows from that.

- **Whose conversation.** What one person asks the bot in one chat is a conversation of its own, with its own container, files, "Always allow" answers and daily budget: session `tg:guest:<chat>:<user>`. Other people in that chat have theirs, the same person has another in every other chat, and none of them is that person's private chat with the bot.
- **What the agent is given.** The message without the mention it starts with, and ahead of it the message it replies to, quoted, since that is usually what the question is about ("@bot summarise this"). A line in front tells the agent that its answer has to fit in one message.
- **One message.** The reply starts as "Working…" and shows the turn's progress. A permission prompt or a clarifying question takes its place, with its buttons, until it is answered, and several prompts are shown one after the other. Everyone in the chat sees what is asked for; only the person who summoned the bot can answer, and anyone else who presses is told so. At the end the answer replaces all of it. An answer too long for one message is cut there and says so, and a request too long to show whole in a prompt is declined, because the file it would be attached as cannot be sent.
- **Many people at once.** Each has a session, so their turns run side by side, and each turn changes its reply as it goes. Telegram refuses calls that come too often and says how long to wait. Progress is left out for that long. The answer is not: it is kept in the session and offered again when the wait is over, for up to 15 minutes, since a guest has nowhere else to say it. A prompt that cannot be shown is declined, as anywhere.
- **Files.** A file written to `/work/out/` goes to the private chat of the person who asked, and the reply says that it did. Telegram lets a bot write only to people who have opened a chat with it, so for anyone else the reply says that the file could not be sent.
- **Commands.** They are sent like anything else, in a message that mentions the bot: `@your_bot /new`, `/resume`, `/rewind`, `/stop`, `/status`, `/effort high`. The lists of `/resume` and `/rewind` are the reply to that message, and it turns into what was picked. They call each message by what the person wrote, without the quoted message and the line for the agent. Anyone in the chat can press such a list, and only the person it was made for gets anywhere: the others are told so. `/effort` on its own names the levels to send and has no buttons, because a level is a setting anyone's session takes, so a press by someone else would have closed the list with a level that person set for themselves.

A press on a guest's button reaches the Worker without the chat it happened in. The button's data names the chat for that reason, and the session is found from it and from who pressed. That costs up to 18 of the 64 bytes a button has, so a `/rewind` button names its conversation by the first 8 characters of the id, which is enough to tell a list of the conversation the chat is in from an older one.

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

`drive` approves permission requests (`--deny` to decline, `--interrupt-after-ms N` to interrupt, `--effort LEVEL` to answer at a level, `--raw` for every frame). While Claude writes, it prints how far the text has got. Without `AGENT_WORKER_ORIGIN` the CLI uses whatever credentials it already has.

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

`DEV_RUNNER_URL` is for development only: every session is sent to that one runner, and nothing replaces it. A runner belongs to the session that claimed it first, so to try another chat, or a guest's session next to a chat's, restart it; a second session is told that the runner belongs to an earlier start. `/new`, `/resume` and `/rewind` only take effect on it once you restart it with an empty working directory and `AGENT_CLAUDE_SESSION_ID` set to the conversation's id. `TELEGRAM_API_BASE` redirects Bot API calls, for a mock or a self-hosted Bot API server.

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
3. `npx wrangler deploy`. The first deploy builds and pushes both images, which takes a few minutes. The agent image sets the modes of the files it takes from the checkout itself, so you can close `containers/agent/system.md` or a skill to the other users of the machine you build on; the agent, which is not root in its container, reads them all the same.
4. Register the webhook with the same secret:
   ```sh
   curl "https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/setWebhook" \
     -d url="$PUBLIC_ORIGIN/tg/webhook" -d secret_token="$TELEGRAM_WEBHOOK_SECRET" \
     -d 'allowed_updates=["message","callback_query","guest_message","stopped_message_generation"]'
   ```
   Telegram sends only the kinds of update named here. A webhook registered before the last two existed has to be registered again, or the bot never hears a guest summons or a press on a draft's Stop button. The answer does not say which kinds are in force: for an address and secret it already has, it is "Webhook is already set" whether or not the list changed. `curl "https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/getWebhookInfo"` shows them under `allowed_updates`. If `curl` ends with `URL rejected` after printing the answer, a space got in behind one of the backslashes when the command was pasted, and whatever followed it was not sent.
5. Message the bot in a private chat. If you are not on the allow-list it replies with your user id; add it to `ALLOWED_TG_USERS` and deploy again. In a group it says nothing to a message of someone the list leaves out: there it may be handed every message, and would answer each of them in front of everybody.
6. To let the bot be summoned in chats it is not a member of, open its settings in @BotFather and switch on Guest Mode. Nothing has to be configured here for it; see "As a guest in other chats".

### The LLM gateway

It must expose the Anthropic Messages format: `POST /v1/messages` with unbuffered SSE, the `anthropic-version` and `anthropic-beta` headers forwarded, and `cache_control` left in place (otherwise every turn is billed uncached). Claude Code is only supported with Claude models. If the gateway rejects beta fields, add `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1` to the agent image's environment.

The `/llm` proxy passes on `POST /v1/messages` and `POST /v1/messages/count_tokens` and nothing else, which is all Claude Code asks for; any other request is answered with a 404 of the Worker's own. With `MODEL` and `SMALL_MODEL` both set, a request for any other model is refused with a 403. Both refusals are logged with the chat they came from, which is where to look if a later version of Claude Code needs something more (`crates/gateway/src/calls.rs`).

Web search is the gateway's to run. For each `WebSearch` Claude Code sends one more `POST /v1/messages`, to `MODEL`, whose only tool is the Messages API's own search tool (`{"type": "web_search_20250305", "name": "web_search"}`), and reads the results out of the answer. When the gateway refuses that call, because it does not offer the tool for the model, the agent is told that the search failed and goes on without it. `WebFetch` asks nothing special of the gateway: the page is downloaded in the container and read by `SMALL_MODEL` in an ordinary call.

### Your MCP server

The MCP container's entry point is `mcp-front`, not the MCP server. It starts the server given as its command line, and is the only thing listening on the container's open port, `0.0.0.0:8080`. Behind it, on loopback:

| | Default | Reached by |
|---|---|---|
| your MCP server | `127.0.0.1:8081` (`MCP_FRONT_MCP_ADDR`) | every request except `/file` |
| the file server | `127.0.0.1:8082` (`MCP_FRONT_FILES_ADDR`) | `/file` |

To run your own server, put its binary in the image built by `containers/mcp/Dockerfile` and name it as the command after `mcp-front`. It must serve streamable HTTP at `/mcp` on the loopback address above; `mcp-front` sets `MCP_FRONT_MCP_ADDR` in its environment for servers that can read it. Requests arrive with `Host: localhost:8080` and an `x-agent-session` header naming the chat, set by the Worker from the verified token.

The port opens only once the MCP server accepts connections, and if the MCP server exits so does `mcp-front`, which stops the container.

The container's disk and memory are lost when it stops (`MCP_IDLE_TTL_SECS` after the last request). If your server's shared state cannot be rebuilt, persist it elsewhere or keep the container up.

Keep your server's credentials out of the image: whoever can pull an image has what was copied into it. Make each one a secret of this Worker, and list their names in `MCP_CONTAINER_ENV`:

```sh
npx wrangler secret put MY_SERVER_TOKEN
# wrangler.toml, under [vars]:
MCP_CONTAINER_ENV = "MY_SERVER_TOKEN"
```

The MCP container is then started with each of them as an environment variable of the same name, and `mcp-front` passes them on to your server. They are read anew at every start, so a changed secret is in the next container. If one is missing or empty, the container is not started at all: requests get a 503 and the log says which variable is missing. The agent containers get none of them. A credential that is a file has to become text for this, which is at most 5 KB a secret; your server then writes it back to a file when it starts.

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

`containers/agent/managed-settings.json` is the permission policy and `crates/runner/src/config.rs` (`DEFAULT_TOOLS`) the list of built-in tools. As shipped: files under `/work` can be edited freely, and the `domain` MCP server, `md2pdf` and `mcp-file` are pre-approved. Reading needs no approval anywhere in the container (`Read(//**)`), with the Read tool or a read-only shell command such as `ls`, `head` or `wc`; without that rule Claude Code asks about every read outside `/work`, which includes the files a skill brings with it. Every other shell command needs a button press, `python3` included.

The agent can search the web and read pages (`WebSearch`, `WebFetch`). Neither is pre-approved: every use asks until **Always** is pressed, which covers that tool for the rest of the conversation. The exception is a list of documentation sites that Claude Code fetches from without asking. Before `WebFetch` downloads from a host it has not seen in the session, Claude Code asks `api.anthropic.com` whether that host may be fetched, straight from the container and whatever the gateway is. Where containers cannot reach that address every fetch fails with "Unable to verify if domain … is safe to fetch", and `"skipWebFetchPreflight": true` in `containers/agent/settings.json` leaves the question out.

Subagents, background tasks and scheduling are off. Of the skills that ship with Claude Code, the ones for working on code and on Claude Code itself are switched off by name in `containers/agent/settings.json`, so the agent is shown the skills in `skills/` and `dataviz`, which is about drawing charts. Its `disableBundledSkills` would switch off all of them, and no entry of `skillOverrides` brings one back. `deep-research` is left on and cannot run: it is a workflow, which takes the `Workflow` tool and subagents, and it is started by typing its name, which the bot does not pass on.

A prompt can be answered with **Always**. For a shell command that covers the identical command only; for any other tool it covers every later use of that tool, and the button says which. The session keeps these answers, so they last until `/new` or `/resume` even though the container is replaced after every idle period.

A prompt shows the first 1,500 characters of what is asked for: the command, or the tool's input with its shortest fields first, so that a path or an address comes before the content sent to it. A request longer than that is also sent whole, as `request.txt` just above the prompt, because the answer covers all of it. If that file cannot be sent, the request is declined without asking.

### PDFs

The agent never writes a typesetting language. It writes Markdown and runs `md2pdf report.md`, which turns it into a web page with Python's `markdown` library and has Chromium print that page as `/work/out/report.pdf`, so the result does not depend on how capable the model is. Everything about the look is in `containers/agent/md2pdf/style.css`: paper size and margins, the page number, fonts, tables and pictures. Put the Noto CJK variant for your users first in the two font lists at its top (`SC`, `TC`, `JP` or `KR`). As shipped, a table runs on over as many pages as it needs with its header row repeated, and a picture takes at most half the width of the text and 70 mm of its height, so two written on one line stand side by side and a phone screenshot does not take a page; smaller pictures keep their size.

`md2pdf` also reads Markdown as models write it, which is not how the library reads it (`Lines` and `Rows` in the script). A list or a table directly under a line of text is one, items indented by two or three spaces are nested, a code block inside a list item is code, and rows of `| a | b |` written without a header become a table instead of text full of pipes. A line break in the Markdown is a line break in the PDF, as in a chat window, so an address block written one item per line stays that way.

It runs without asking, and a browser can do a good deal more than print. Three things hold it to reading one Markdown file and writing one PDF:

- The page Chromium gets is not the library's. The library passes HTML in the Markdown on as it is written, scripts and frames included, and the agent writes Markdown out of what it has read. `md2pdf` writes the page again from a short list of tags (`Page` in the script), with attributes of its own making and all text escaped. `<br>` is a line break, plain formatting such as `<sub>` is kept, wrappers such as `<div align="center">` are dropped, and anything else is printed as written, so `<customer name>` stays in a letter and a script is so much text.
- A picture has to be a file in the container. One named by a web address is refused, with a message that tells the agent to save it first, and that download is a command the user is asked about. Fetching it from `md2pdf` would hand an address of the agent's choosing to a server of its choosing, which is what `WebFetch` asks for.
- The page carries a content security policy that allows local pictures and its own styles and nothing else, and Chromium is started with name lookup switched off. Either of the two alone keeps a page that got past the above from reaching the network.

Nothing of the agent's is on Chromium's command line: the page is a file `md2pdf` names itself, in a temporary directory, and the PDF is moved to `/work/out` only once it is complete, under the name asked for, whatever directory that name is given with.

Chromium's own sandbox needs the kernel to let an ordinary user make namespaces, which a container often does not. `md2pdf` starts Chromium with it and, if that fails, again with `--no-sandbox`; what holds the page in then is the container.

The image has Debian's `chromium-headless-shell` and not the whole browser. It is the same engine and prints the same PDF, in about a third of the memory: some 270 MB at its peak against 640 MB for a report of five pages. A container's memory is shared with Claude Code, which takes 300 to 450 MB of it, and `basic` is 1 GiB.

Pictures are what would not fit. Chromium keeps every picture of a page in memory unpacked, at four bytes to the pixel, until the PDF is written: a report with thirty pictures from a chat took 930 MB. So `md2pdf` gives Chromium a smaller copy of any picture that has more pixels than paper can show (`fitted` in the script, with Pillow). A picture gets two million pixels at most, and less in a document of more than sixteen, because all of them share 32 million. That report then takes 400 MB, and its PDF is 4 MB and not 35. The copies are temporary and the files in `/work` are only read. A photograph stays a JPEG and is turned the way the camera was held; anything else becomes a PNG and keeps what can be seen through. A picture that cannot be read at all in the memory there is, one of more than 40 million pixels that is not a JPEG, is refused with a message for the agent.

What is left is the text, at some 3 MB for a page of tables: 270 MB for five pages and 460 MB for seventy. A report of well over a hundred pages is therefore where `basic` ends; `instance_type = "standard-1"` in `wrangler.toml` has 4 GiB. Should memory run out all the same, Chromium has marked its renderer as the first process for the kernel to end, and `md2pdf` then tells the agent that the PDF could not be made. These numbers were measured with Debian's packages in a container on a desktop machine, where nothing limits its memory, not on Cloudflare.

A document gets 300 seconds, from its Markdown to its PDF (`PRINT_TIMEOUT` in the script), pictures and both starts of Chromium included. When they are over `md2pdf` says so, and that the agent is to tell the user and not to run it again. Claude Code has a limit of its own for a command, two minutes unless the agent names another, and what it says when that is up is only that the command timed out, which an agent answers by running it again. So the runner starts Claude Code with `BASH_DEFAULT_TIMEOUT_MS` at 330 seconds (`BASH_TIMEOUT_MS` in `crates/runner/src/claude.rs`), for every command: one that hangs holds a turn for five and a half minutes and not two, unless `/stop` ends it. A test keeps the second number above the first. With a quarter of one processor of a desktop machine, a report of 84 pages of tables took 25 seconds and one of thirty photographs 35; with a tenth, 62 and 90. How long a quarter of Cloudflare's takes, with a browser that is started for the first time in its container, was not measured.

Nothing asks the user before a file in `/work/out` is sent, and it does not matter how it got there: a prompt for `mv notes.md /work/out` is a prompt for a shell command, and says nothing about delivery. So the agent is told to keep its working files in `/work`, by the system prompt, by the skill and by `md2pdf` itself, which says when it has written a PDF that the user is about to receive it and that nothing is left to move.

To try a change without building the image, install Chromium or Chrome and Python's `markdown` and `Pillow` packages, and run `MD2PDF_HOME=containers/agent/md2pdf MD2PDF_OUT_DIR=. python3 containers/agent/md2pdf/md2pdf some.md`. `python3 containers/agent/md2pdf/test_md2pdf.py` runs its tests; the image build runs them too, with the versions the image has.

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
| `DAILY_TOKEN_BUDGET` | 0 (off) | Fresh tokens per session per UTC day, counted at the `/llm` proxy from what each model call reports. Checked when a message arrives and before every model call, so a turn that uses it up ends there. A session is a chat, or one person's summons of the bot as a guest in one chat, so someone who summons it in several chats has this much in each |
| `MCP_IDLE_TTL_SECS` | 900 | Idle time before the MCP container stops |
| `MCP_CONTAINER_ENV` | none | Comma separated names of secrets or variables the MCP container is started with (see "Your MCP server") |

## Security model

- An agent container holds one credential: a session token that is only accepted by this Worker's `/llm`, `/mcp` and `/file` routes. The gateway key and bot token never leave the Worker.
- The MCP container holds the MCP server's own credentials, if `MCP_CONTAINER_ENV` names any. They come from the Worker's secrets when it starts, not from its image, so a copy of the image does not carry them. Inside the container they are in the environment of `mcp-front` and the server, and whatever the server writes them to. An agent reaches that container only through the server's tools and `/file`, which serves the one directory the server saves files in. Don't list the Worker's own secrets there: the gateway key would then be in a container that runs someone else's code.
- The agent can read that token, so holding it is not enough. Before acting on one, the proxy asks the chat's session, which refuses a token that is not the one its running container was started with: a token stops working when its container is stopped or replaced, well before its 24 hours are up. For a model call the session also refuses once the day's budget is used up, and the proxy passes on only the two requests Claude Code makes, for the two configured models. What is left to someone who copies a token out is what the agent itself could do: model calls on the chat's budget and the chat's MCP calls, while the container runs.
- The runner's control API needs a secret the session sets before any agent process exists, and the runner makes its own memory unreadable to the agent, so the agent cannot approve its own prompts.
- The permission policy is a root-owned managed settings file. Settings the agent writes in its working directory are not loaded.
- The system prompt and the skills are not secret from the people who chat. They are in the agent image, the agent has to read them to work, and reading needs no approval, so it can be asked to show them. Closing those files on the machine that builds the image protects that copy and nothing else.
- Containers currently have outbound internet access. An approved shell command can therefore send data anywhere; keep the pre-approved list short. A command on that list runs with whatever arguments the agent gives it, so it has to be safe with all of them, as `md2pdf` and `mcp-file` are meant to be. The `domain` MCP server is pre-approved as a whole: if its tools return text written by third parties, that text can steer the agent into calling them, and no prompt stands in between. A web page or a search result is such text as well, and `WebFetch` sends whatever is in the address it is given to the host it names, so pre-approving it would let a page have the agent pass on what it has read.
- A permission prompt shows what is being approved, and all of it: a request too long for the message is attached as a file. The file of an output document is uploaded to Telegram under a boundary made up for that upload, so its content cannot add to the request.
- As a guest the bot answers only people on the allow-list, wherever they summon it, and each of them in each chat has a session nobody else reaches. A guest's prompts are in a message the whole chat reads, commands and addresses included, and are answered by the person who summoned the bot alone: a press leads to the session of whoever pressed, which for anyone else is one that never asked. The message a summons replies to is handed to the agent, and it is text by whoever wrote it, like a web page: it can try to steer the agent, and the prompts are what stands in between. Files leave a guest's turn only towards the private chat of the person who asked.
- `SESSION_TOKEN_SECRET` must be at least 16 characters and no secret may be empty; the Worker answers 500 to everything until that is so. A short signing key could be found by trying keys against a token, and an empty webhook secret is what a request without the header presents.
- Nothing a failed model call says reaches a container or the chat. A gateway's error names its site and quotes the ids it knows the request by, and Claude Code prints such an error into its answer word for word. So the `/llm` proxy rebuilds every error from its status and its kind (`crates/gateway/src/scrub.rs`): an error status, an `error` event inside a stream, and a status 200 whose body is an error or a page of the gateway's. It also drops every response header but the five that say what the body is and when to retry. The gateway's own words go to the Worker's log, with the chat they belong to, which is where to look when a user reports "The model service had an internal error". The bot, for its part, never shows this Worker's host, which Claude Code adds to some of its messages.
- What a successful model call returns is passed on as it is, ids and model name included. It is not shown in the chat, but it is in the transcript inside the container, which the agent can read.

## Upgrading Claude Code

The runner relies on `--permission-prompt-tool`, `--input-format stream-json`, `--replay-user-messages`, `--include-partial-messages`, `--resume`, `--effort` and the `apply_flag_settings` control request. After changing `CLAUDE_CODE_VERSION`, run the runner against the new CLI and check with `drive` that: a non-approved Bash command produces a permission request and runs when allowed; a second prompt in the same run gets its own result; an interrupt ends the turn and the next prompt still works; restarting the runner resumes the conversation; a `drive --raw --effort high` after a run without `--effort` shows a `control_response` whose `subtype` is `success`; an answer of a few sentences prints `writing: … characters so far` more than once, and `drive --raw` shows no event of the type `stream_event`.

Check that `claude -p /context --settings containers/agent/settings.json` still reports a window of 128k and an autocompact buffer: the two settings that keep long conversations compacted are read from that file, and a setting Claude Code no longer knows is ignored without a word.

Check which skills the new version brings. They are switched off one at a time, so one that is new is shown to the agent until it is added to `skillOverrides`. `drive --raw` prints the `init` message, whose `skills` should be those in `skills/`, `dataviz` and `deep-research`. Ask the agent which skills it can use as well: built-in commands such as `init` and `security-review` are listed to the model without being in `skills`.

Check that a command still gets the time the runner sets: with `BASH_DEFAULT_TIMEOUT_MS=3000` in its environment, Claude Code ends `sleep 8` after three seconds with "Command timed out after 3s". If the variable is no longer read, `md2pdf` is cut off at two minutes again.

Check as well that `claude --help` still lists the levels of `Effort` in `crates/proto` for `--effort`. The CLI accepts a level it does not know without an error and goes on at its default, so a renamed level would not show anywhere else.

## Not built yet

- Web UI (Leptos) on the same session API.
- Locking containers down to the Worker only (`enableInternet: false` with outbound interception).
- Sending a file from an agent to the MCP server. Files only travel the other way.
- Letting a container sleep while a prompt waits (`defer`), and filesystem snapshots instead of the tar archive.
- One daily budget for a person across the chats they summon the bot in.
- Drafts in the topics of a private chat. A draft is shown without a topic.
