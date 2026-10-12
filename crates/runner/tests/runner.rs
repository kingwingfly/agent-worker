//! Drives a real runner over HTTP and WebSocket, with `fake_claude.sh` in place of the CLI.

use std::{fs, path::PathBuf, time::Duration};

use agent_proto::{Decision, Effort, FromRunner, ToRunner, TurnResult, Usage};
use agent_runner::{Config, Runner};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::header::AUTHORIZATION},
};

const SECRET: &str = "0123456789abcdef0123456789abcdef";
const SESSION_ID: &str = "11111111-2222-4333-8444-555555555555";
const TIMEOUT: Duration = Duration::from_secs(10);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Harness {
    runner: Runner,
    cfg: Config,
    http: reqwest::Client,
    _dir: TempDir,
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    async fn start_with(customize: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().join("work");
        let mut cfg = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            approver_listen: "127.0.0.1:0".parse().unwrap(),
            claude_bin: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fake_claude.sh"),
            out_dir: work_dir.join("out"),
            work_dir,
            claude_config_dir: dir.path().join("claude"),
            runtime_dir: dir.path().join("run"),
            settings: None,
            system_prompt: None,
            max_turns: 5,
            tools: agent_runner::config::DEFAULT_TOOLS.into(),
            session_id: SESSION_ID.into(),
            upstream: None,
            model: None,
            small_model: None,
            state_max_bytes: 1 << 20,
        };
        customize(&mut cfg);
        let runner = agent_runner::start(cfg.clone()).await.unwrap();
        Self {
            runner,
            cfg,
            http: reqwest::Client::new(),
            _dir: dir,
        }
    }

    async fn claimed() -> Self {
        Self::claim(Self::start().await).await
    }

    async fn claim(harness: Self) -> Self {
        let response = harness.post("/claim").body(SECRET).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        harness
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.runner.addr)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(self.url(path)).bearer_auth(SECRET)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.post(self.url(path)).bearer_auth(SECRET)
    }

    /// Opens the control socket and consumes the `Hello` frame.
    async fn connect(&self) -> (Socket, FromRunner) {
        let mut request = format!("ws://{}/ws", self.runner.addr)
            .into_client_request()
            .unwrap();
        let bearer = format!("Bearer {SECRET}").parse().unwrap();
        request.headers_mut().insert(AUTHORIZATION, bearer);
        let (mut socket, _) = connect_async(request).await.unwrap();
        let hello = next(&mut socket).await;
        assert!(matches!(hello, FromRunner::Hello { .. }), "{hello:?}");
        (socket, hello)
    }

    fn claude_args(&self) -> Vec<String> {
        fs::read_to_string(self.cfg.claude_config_dir.join("args"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

async fn send(socket: &mut Socket, frame: ToRunner) {
    let text = serde_json::to_string(&frame).unwrap();
    socket.send(Message::Text(text.into())).await.unwrap();
}

/// Next frame that is not a heartbeat.
async fn next(socket: &mut Socket) -> FromRunner {
    loop {
        let message = tokio::time::timeout(TIMEOUT, socket.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("socket closed")
            .unwrap();
        if let Message::Text(text) = message {
            match serde_json::from_str(&text).unwrap() {
                FromRunner::Heartbeat => continue,
                frame => return frame,
            }
        }
    }
}

/// Next permission request, skipping the Claude events streamed around it.
async fn next_permission_request(socket: &mut Socket) -> FromRunner {
    loop {
        match next(socket).await {
            request @ FromRunner::PermissionRequest { .. } => return request,
            FromRunner::Event { .. } => continue,
            other => panic!("expected a permission request, got {other:?}"),
        }
    }
}

/// Asserts that nothing but heartbeats arrives for a little while.
async fn expect_silence(socket: &mut Socket) {
    let quiet = tokio::time::timeout(Duration::from_millis(300), next(socket)).await;
    assert!(quiet.is_err(), "unexpected frame: {quiet:?}");
}

async fn user_message(socket: &mut Socket, turn_id: &str, text: &str) {
    user_message_at(socket, turn_id, text, None).await;
}

async fn user_message_at(socket: &mut Socket, turn_id: &str, text: &str, effort: Option<Effort>) {
    let frame = ToRunner::UserMessage {
        turn_id: turn_id.into(),
        text: text.into(),
        effort,
    };
    send(socket, frame).await;
}

/// Collects frames up to and including the turn's `TurnDone`.
async fn finish_turn(socket: &mut Socket) -> (Vec<FromRunner>, TurnResult) {
    let mut frames = Vec::new();
    loop {
        let frame = next(socket).await;
        frames.push(frame.clone());
        if let FromRunner::TurnDone { result, .. } = frame {
            return (frames, result);
        }
    }
}

#[tokio::test]
async fn control_endpoints_need_the_claimed_secret() {
    let harness = Harness::start().await;
    let status =
        |request: reqwest::RequestBuilder| async { request.send().await.unwrap().status() };

    assert_eq!(
        status(harness.http.get(harness.url("/healthz"))).await,
        StatusCode::OK
    );
    assert_eq!(
        status(harness.get("/state")).await,
        StatusCode::SERVICE_UNAVAILABLE
    );

    assert_eq!(
        status(harness.post("/claim").body("short")).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        status(harness.post("/claim").body(SECRET)).await,
        StatusCode::CREATED
    );
    // Retrying with the same secret is fine, and reported as such; a different one is not.
    assert_eq!(
        status(harness.post("/claim").body(SECRET)).await,
        StatusCode::NO_CONTENT
    );
    let other = "f".repeat(32);
    assert_eq!(
        status(harness.post("/claim").body(other.clone())).await,
        StatusCode::CONFLICT
    );

    assert_eq!(status(harness.get("/state")).await, StatusCode::OK);
    assert_eq!(
        status(harness.http.get(harness.url("/state"))).await,
        StatusCode::UNAUTHORIZED
    );
    let wrong = harness.http.get(harness.url("/state")).bearer_auth(other);
    assert_eq!(status(wrong).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_turn_streams_events_and_ends_with_the_result() {
    let harness = Harness::claimed().await;
    let (mut socket, hello) = harness.connect().await;
    assert!(matches!(
        hello,
        FromRunner::Hello {
            last_seq: 0,
            running_turn: None,
            ..
        }
    ));

    user_message(&mut socket, "t1", "hello").await;
    let (frames, result) = finish_turn(&mut socket).await;

    let seqs: Vec<_> = frames.iter().filter_map(FromRunner::seq).collect();
    assert_eq!(
        seqs,
        [1, 2, 3, 4, 5],
        "init, replay, assistant, result, turn_done"
    );
    assert!(
        matches!(&frames[2], FromRunner::Event { turn_id, event, .. }
        if turn_id == "t1" && event["type"] == "assistant")
    );
    assert_eq!(
        result,
        TurnResult {
            is_error: false,
            subtype: "success".into(),
            text: Some("echo: hello".into()),
            usage: Usage {
                input_tokens: 3,
                output_tokens: 5,
                ..Usage::default()
            },
            artifacts: vec![],
        }
    );

    let args = harness.claude_args();
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--session-id", SESSION_ID]),
        "{args:?}"
    );
    assert!(args.contains(&"mcp__approver__permission_prompt".to_owned()));
    assert!(args.contains(&"--include-partial-messages".to_owned()));
    // The MCP servers are given inline, with the approver at the address really bound.
    let position = args.iter().position(|arg| arg == "--mcp-config").unwrap();
    let servers: serde_json::Value = serde_json::from_str(&args[position + 1]).unwrap();
    assert_eq!(
        servers["mcpServers"]["approver"]["url"],
        format!("http://{}/mcp", harness.runner.approver_addr)
    );
    assert!(!harness.cfg.runtime_dir.join("mcp.json").exists());

    // The process stays up for the next turn; a re-sent message is not run twice.
    user_message(&mut socket, "t1", "hello").await;
    user_message(&mut socket, "t2", "again").await;
    let (frames, result) = finish_turn(&mut socket).await;
    assert_eq!(result.text.as_deref(), Some("echo: again"));
    assert!(frames.iter().all(|frame| match frame {
        FromRunner::Event { turn_id, .. } | FromRunner::TurnDone { turn_id, .. } => turn_id == "t2",
        _ => false,
    }));
}

#[tokio::test]
async fn the_text_being_written_is_passed_on_now_and_then_and_its_pieces_are_not() {
    let harness = Harness::claimed().await;
    let (mut socket, _) = harness.connect().await;

    user_message(&mut socket, "t1", "typed words").await;
    let (frames, result) = finish_turn(&mut socket).await;

    // The first piece at once, the second not so soon after it, and the third with all
    // that came before. Thinking, a sub-agent's text and what was written before the
    // message was picked up are in none of them.
    let partials: Vec<&FromRunner> = frames
        .iter()
        .filter(|frame| matches!(frame, FromRunner::Partial { .. }))
        .collect();
    let partial = |text: &str| FromRunner::Partial {
        turn_id: "t1".into(),
        text: text.into(),
    };
    assert_eq!(
        partials,
        [&partial("echo: "), &partial("echo: typed words")]
    );

    // The pieces themselves are no events: the numbered frames are the ones of a turn
    // that came in whole messages.
    let events: Vec<&str> = frames
        .iter()
        .filter_map(|frame| match frame {
            FromRunner::Event { event, .. } => event["type"].as_str(),
            _ => None,
        })
        .collect();
    assert_eq!(events, ["system", "user", "assistant", "result"]);
    let seqs: Vec<_> = frames.iter().filter_map(FromRunner::seq).collect();
    assert_eq!(seqs, [1, 2, 3, 4, 5]);
    assert_eq!(result.text.as_deref(), Some("echo: typed words"));

    // Nothing of them is kept for a reconnecting Durable Object either.
    drop(socket);
    let (mut socket, _) = harness.connect().await;
    send(&mut socket, ToRunner::Resume { after_seq: 0 }).await;
    for seq in 1..=5 {
        assert_eq!(next(&mut socket).await.seq(), Some(seq));
    }
    expect_silence(&mut socket).await;
}

#[tokio::test]
async fn a_result_claude_produced_on_its_own_does_not_end_the_turn() {
    let harness = Harness::claimed().await;
    let (mut socket, _) = harness.connect().await;

    user_message(&mut socket, "t1", "stale result first").await;
    let (frames, result) = finish_turn(&mut socket).await;

    assert_eq!(result.text.as_deref(), Some("echo: stale result first"));
    let results = frames
        .iter()
        .filter(
            |frame| matches!(frame, FromRunner::Event { event, .. } if event["type"] == "result"),
        )
        .count();
    assert_eq!(
        results, 2,
        "the stray result is forwarded, just not treated as the answer"
    );
}

#[tokio::test]
async fn output_files_are_reported_and_downloadable() {
    let harness = Harness::claimed().await;
    let (mut socket, _) = harness.connect().await;

    user_message(&mut socket, "t1", "make a pdf").await;
    let (_, result) = finish_turn(&mut socket).await;
    let [artifact] = result.artifacts.as_slice() else {
        panic!("expected one artifact, got {:?}", result.artifacts);
    };
    assert_eq!(
        (artifact.path.as_str(), artifact.size),
        ("reports/summary.pdf", 3)
    );

    let response = harness
        .get("/files/out/reports/summary.pdf")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"PDF");
    let escape = harness
        .get("/files/out/..%2F..%2Fclaude%2Fargs")
        .send()
        .await
        .unwrap();
    assert_eq!(escape.status(), StatusCode::NOT_FOUND);

    // An untouched file is not reported again.
    user_message(&mut socket, "t2", "nothing new").await;
    let (_, result) = finish_turn(&mut socket).await;
    assert!(result.artifacts.is_empty());
}

#[tokio::test]
async fn unacknowledged_frames_are_replayed_after_a_reconnect() {
    let harness = Harness::claimed().await;
    let (mut socket, _) = harness.connect().await;
    user_message(&mut socket, "t1", "hello").await;
    let (frames, _) = finish_turn(&mut socket).await;
    drop(socket);

    let (mut socket, hello) = harness.connect().await;
    assert!(matches!(
        hello,
        FromRunner::Hello {
            last_seq: 5,
            running_turn: None,
            ..
        }
    ));
    expect_silence(&mut socket).await;

    send(&mut socket, ToRunner::Resume { after_seq: 3 }).await;
    assert_eq!(next(&mut socket).await, frames[3]);
    assert_eq!(next(&mut socket).await, frames[4]);

    send(&mut socket, ToRunner::Ack { seq: 5 }).await;
    send(&mut socket, ToRunner::Resume { after_seq: 0 }).await;
    expect_silence(&mut socket).await;
}

#[tokio::test]
async fn permission_prompts_wait_for_the_remote_decision() {
    let harness = Harness::claimed().await;
    let (mut socket, _) = harness.connect().await;
    let session = harness.runner.session.clone();
    let ask = |command: &'static str| {
        let session = session.clone();
        tokio::spawn(async move {
            session
                .request_permission("Bash".into(), json!({ "command": command }))
                .await
        })
    };

    // Outside a turn there is nobody to ask.
    assert!(matches!(ask("ls").await.unwrap(), Decision::Deny { .. }));

    user_message(&mut socket, "t1", "slow work").await;
    let init = next(&mut socket).await;
    assert!(matches!(init, FromRunner::Event { .. }), "{init:?}");

    let pending = ask("ls");
    let FromRunner::PermissionRequest {
        request_id,
        turn_id,
        tool_name,
        input,
        ..
    } = next_permission_request(&mut socket).await
    else {
        unreachable!();
    };
    assert_eq!((turn_id.as_str(), tool_name.as_str()), ("t1", "Bash"));
    assert_eq!(input, json!({ "command": "ls" }));
    assert!(!pending.is_finished());

    let allow = Decision::Allow {
        updated_input: None,
    };
    let frame = ToRunner::PermissionDecision {
        request_id,
        decision: allow.clone(),
    };
    send(&mut socket, frame).await;
    assert_eq!(pending.await.unwrap(), allow);

    // The runner remembers nothing: the same command is asked about again, and it is
    // for the Durable Object to answer from what the user said before.
    let other = ask("ls");
    next_permission_request(&mut socket).await;

    // Interrupting the turn answers whatever is still open.
    send(&mut socket, ToRunner::Interrupt).await;
    assert!(matches!(other.await.unwrap(), Decision::Deny { .. }));
    let (_, result) = finish_turn(&mut socket).await;
    assert_eq!(result.subtype, "error_during_execution");
}

#[tokio::test]
async fn a_crash_ends_the_turn_with_an_error_and_the_next_turn_restarts_claude() {
    let harness = Harness::claimed().await;
    let (mut socket, _) = harness.connect().await;

    user_message(&mut socket, "t1", "crash now").await;
    let (_, result) = finish_turn(&mut socket).await;
    assert!(result.is_error);
    assert_eq!(result.subtype, "runner_error");
    let detail = result.text.unwrap();
    assert!(
        detail.contains("exit status: 3") && detail.contains("simulated failure"),
        "{detail}"
    );

    user_message(&mut socket, "t2", "back").await;
    let (_, result) = finish_turn(&mut socket).await;
    assert_eq!(result.text.as_deref(), Some("echo: back"));
}

#[tokio::test]
async fn a_message_sent_to_an_exiting_claude_is_delivered_to_its_replacement() {
    let harness = Harness::claimed().await;
    let (mut socket, _) = harness.connect().await;

    user_message(&mut socket, "t1", "linger then leave").await;
    finish_turn(&mut socket).await;
    // The first process is still alive here but will never read this.
    user_message(&mut socket, "t2", "still there?").await;
    let (_, result) = finish_turn(&mut socket).await;
    assert_eq!(result.text.as_deref(), Some("echo: still there?"));
}

#[tokio::test]
async fn a_change_of_effort_reaches_the_running_claude_and_a_new_one_starts_at_it() {
    let harness = Harness::claimed().await;
    let (mut socket, _) = harness.connect().await;
    // What the fake CLI wrote to `file` since this was last asked.
    let recorded = |file: &str| {
        let path = harness.cfg.claude_config_dir.join(file);
        let lines = fs::read_to_string(&path).unwrap_or_default();
        let _ = fs::remove_file(&path);
        lines.lines().map(str::to_owned).collect::<Vec<_>>()
    };
    // `--effort` at each launch, where `[]` means that the process stayed and `[None]`
    // that one was launched with the level left to the CLI.
    let launches = || match recorded("args").as_slice() {
        [] => Vec::new(),
        args => {
            let effort = args.windows(2).find(|pair| pair[0] == "--effort");
            vec![effort.map(|pair| pair[1].clone())]
        }
    };
    let changes = || recorded("efforts");
    let mut turns = 0;
    let mut answer = async |text: &str, effort| {
        turns += 1;
        user_message_at(&mut socket, &format!("t{turns}"), text, effort).await;
        finish_turn(&mut socket).await.1
    };

    let result = answer("hello", None).await;
    assert_eq!(result.text.as_deref(), Some("echo: hello"));
    assert_eq!((launches(), changes()), (vec![None], vec![]));

    // The process is told, not replaced, and the message after it is answered as ever.
    let result = answer("hello", Some(Effort::High)).await;
    assert_eq!(result.text.as_deref(), Some("echo: hello"));
    assert_eq!((launches(), changes()), (vec![], vec!["high".to_owned()]));
    // Once is enough.
    answer("hello", Some(Effort::High)).await;
    assert_eq!((launches(), changes()), (vec![], vec![]));
    // Back to the CLI's own choice.
    answer("hello", None).await;
    assert_eq!((launches(), changes()), (vec![], vec!["null".to_owned()]));

    // A process launched while a level is set starts at it and needs no telling.
    assert_eq!(answer("crash now", None).await.subtype, "runner_error");
    let result = answer("hello", Some(Effort::Max)).await;
    assert_eq!(result.text.as_deref(), Some("echo: hello"));
    assert_eq!(
        (launches(), changes()),
        (vec![Some("max".to_owned())], vec![])
    );
}

#[tokio::test]
async fn a_turn_interrupted_before_claude_picked_it_up_is_not_run_again() {
    let harness = Harness::claimed().await;
    let (mut socket, _) = harness.connect().await;

    user_message(&mut socket, "t1", "sluggish start").await;
    let init = next(&mut socket).await;
    assert!(matches!(init, FromRunner::Event { .. }), "{init:?}");
    send(&mut socket, ToRunner::Interrupt).await;

    let (frames, result) = finish_turn(&mut socket).await;
    assert_eq!(result.subtype, "interrupted");
    assert!(
        !frames
            .iter()
            .any(|frame| matches!(frame, FromRunner::Event { event, .. }
            if event["type"] == "assistant")),
        "the cancelled message must not be answered: {frames:?}"
    );

    // And the session is still usable.
    user_message(&mut socket, "t2", "hello").await;
    let (_, result) = finish_turn(&mut socket).await;
    assert_eq!(result.text.as_deref(), Some("echo: hello"));
}

#[tokio::test]
async fn a_claude_that_keeps_dying_fails_the_turn_after_one_retry() {
    let harness = Harness::start_with(|cfg| cfg.claude_bin = "/bin/false".into()).await;
    let harness = Harness::claim(harness).await;
    let (mut socket, _) = harness.connect().await;

    user_message(&mut socket, "t1", "hello").await;
    let (frames, result) = finish_turn(&mut socket).await;
    assert_eq!(frames.len(), 1, "no output, just the failure");
    assert_eq!(result.subtype, "runner_error");
    assert!(result.text.unwrap().contains("exit status: 1"));
}

#[tokio::test]
async fn a_second_message_during_a_turn_is_rejected() {
    let harness = Harness::claimed().await;
    let (mut socket, _) = harness.connect().await;

    user_message(&mut socket, "t1", "slow work").await;
    user_message(&mut socket, "t2", "impatient").await;
    let busy = loop {
        if let FromRunner::TurnDone {
            turn_id, result, ..
        } = next(&mut socket).await
        {
            break (turn_id, result.subtype);
        }
    };
    assert_eq!(busy, ("t2".to_owned(), "runner_busy".to_owned()));

    let (_, hello) = harness.connect().await;
    assert!(matches!(hello, FromRunner::Hello { running_turn: Some(turn), .. } if turn == "t1"));
}

#[tokio::test]
async fn state_moves_to_a_fresh_runner_which_then_resumes() {
    let old = Harness::claimed().await;
    let project = old.cfg.projects_dir().join("-work");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join(format!("{SESSION_ID}.jsonl")), b"{}\n").unwrap();
    fs::write(old.cfg.work_dir.join("notes.txt"), b"remember me").unwrap();
    std::os::unix::fs::symlink("/etc/hostname", old.cfg.work_dir.join("link")).unwrap();

    let archive = old.get("/state").send().await.unwrap();
    assert_eq!(archive.status(), StatusCode::OK);
    let archive = archive.bytes().await.unwrap();

    let new = Harness::claimed().await;
    assert!(!new.cfg.has_transcript());
    let restored = new
        .post("/bootstrap")
        .body(archive.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::NO_CONTENT);

    assert_eq!(
        fs::read(new.cfg.work_dir.join("notes.txt")).unwrap(),
        b"remember me"
    );
    let link = fs::symlink_metadata(new.cfg.work_dir.join("link")).unwrap();
    assert!(
        link.file_type().is_symlink(),
        "symlinks are restored as symlinks"
    );
    assert!(new.cfg.has_transcript());

    let (mut socket, _) = new.connect().await;
    user_message(&mut socket, "t1", "hello").await;
    finish_turn(&mut socket).await;
    let args = new.claude_args();
    assert!(
        args.windows(2).any(|pair| pair == ["--resume", SESSION_ID]),
        "{args:?}"
    );

    // Too late to restore once Claude has read its transcript.
    let late = new.post("/bootstrap").body(archive).send().await.unwrap();
    assert_eq!(late.status(), StatusCode::CONFLICT);
}
