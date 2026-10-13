//! Runs the real `mcp-front` binary with a stand-in MCP server behind it.

use std::{
    collections::BTreeSet,
    fs,
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode, header},
    routing::{get, post},
};
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(10);

/// The ports the tests were given so far. Whoever holds this may try a port, which is
/// done by listening on it for a moment, or start a process, and nobody does the other
/// meanwhile; see [`free_addr`].
fn ports() -> MutexGuard<'static, BTreeSet<u16>> {
    static PORTS: Mutex<BTreeSet<u16>> = Mutex::new(BTreeSet::new());
    // A test that failed while it held this is no reason for the others to fail.
    PORTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An address nothing listens on, and that no test here was given before.
///
/// The tests run side by side, which gets in the way of that twice. The kernel hands a
/// port that one test has just let go to the next that asks, so two fronts would take
/// each other's servers for their own: hence the record of what was given. And a
/// process started by another test at this very moment takes a copy of the listener the
/// port is tried with, and keeps it until it runs its own program, so that something
/// listens on the port a little longer and whoever was given it finds it taken, or
/// takes it for a front that is up: hence [`launch`].
fn free_addr() -> SocketAddr {
    let mut given = ports();
    loop {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        if given.insert(addr.port()) {
            return addr;
        }
    }
}

/// Starts a process while no port is being tried. It is running its own program, with
/// none of this one's sockets, by the time this returns.
fn launch(command: &mut Command) -> std::io::Result<Child> {
    let _ports = ports();
    command.spawn()
}

fn accepts_connections(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stands in for the MCP server. `POST /mcp` reports what it was sent; `GET /events`
/// sends one line, and a second only when `more` is notified.
async fn serve_mcp(addr: SocketAddr, more: Arc<Notify>) {
    async fn echo(headers: HeaderMap, body: String) -> String {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("-")
                .to_owned()
        };
        format!(
            "host={} session={} body={body}",
            header("host"),
            header("x-agent-session")
        )
    }
    let events = move || async move {
        let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(1);
        tokio::spawn(async move {
            let _ = sender.send(Bytes::from_static(b"one\n")).await;
            more.notified().await;
            let _ = sender.send(Bytes::from_static(b"two\n")).await;
        });
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(stream_of(receiver)),
        )
    };
    let app = Router::new()
        .route("/mcp", post(echo))
        .route("/events", get(events))
        .fallback(|| async { (StatusCode::NOT_FOUND, "from the mcp server") });
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
}

/// The receiver's items as a stream a response body can be made from.
fn stream_of(
    receiver: tokio::sync::mpsc::Receiver<Bytes>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|bytes| (Ok(bytes), receiver))
    })
}

struct Front {
    process: Child,
    addr: SocketAddr,
    mcp: SocketAddr,
    dir: tempfile::TempDir,
}

impl Front {
    /// Starts `mcp-front` running `command` as its MCP server, which is expected to be
    /// reachable at [`Front::mcp`]: the tests serve that address themselves.
    fn start(command: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (addr, mcp) = (free_addr(), free_addr());
        let mut front = Command::new(env!("CARGO_BIN_EXE_mcp-front"));
        front
            .args(command)
            .env("MCP_FRONT_LISTEN", addr.to_string())
            .env("MCP_FRONT_MCP_ADDR", mcp.to_string())
            .env("MCP_FRONT_FILES_ADDR", free_addr().to_string())
            .env("MCP_FRONT_FILES_DIR", dir.path().join("files"))
            .env("MCP_FRONT_START_TIMEOUT_SECS", "8")
            .env("RUST_LOG", "off")
            // Its MCP server inherits these, and must not hold on to the test's own.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let process = launch(&mut front).unwrap();
        Self {
            process,
            addr,
            mcp,
            dir,
        }
    }

    /// A front with its MCP server and its port already up.
    async fn ready(command: &[&str]) -> (Self, Arc<Notify>) {
        let front = Self::start(command);
        let more = Arc::new(Notify::new());
        serve_mcp(front.mcp, more.clone()).await;
        until("the front to listen", || accepts_connections(front.addr)).await;
        (front, more)
    }

    fn files(&self) -> std::path::PathBuf {
        self.dir.path().join("files")
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn file(&self, path: &Path) -> reqwest::Response {
        reqwest::Client::new()
            .get(self.url("/file"))
            .query(&[("path", path.to_str().unwrap())])
            .send()
            .await
            .unwrap()
    }

    async fn exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the front did not exit");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn stderr(&mut self) -> String {
        use std::io::Read;
        let mut text = String::new();
        if let Some(mut stderr) = self.process.stderr.take() {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    }
}

impl Front {
    /// Asks the front to stop the way a container runtime does.
    fn terminate(&self) {
        let pid = self.process.id().to_string();
        if let Ok(mut kill) = launch(Command::new("kill").args(["-TERM", &pid])) {
            let _ = kill.wait();
        }
    }
}

impl Drop for Front {
    fn drop(&mut self) {
        // Asked first, so that it takes its MCP server with it.
        self.terminate();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && matches!(self.process.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_request_reaches_the_server_it_is_for() {
    let (front, _more) = Front::ready(&["sleep", "60"]).await;
    let saved = front.files().join("-100123_45_行程 (final).pdf");
    fs::write(&saved, "%PDF-saved").unwrap();
    fs::write(front.dir.path().join("secret"), "no").unwrap();

    // The path the MCP server would report gets the file, from the file server.
    let response = front.file(&saved).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/pdf");
    assert_eq!(response.text().await.unwrap(), "%PDF-saved");
    // What is not in the directory does not exist, and the MCP server is not asked.
    let response = front.file(&front.dir.path().join("secret")).await;
    assert_eq!(response.status(), 404);
    assert_eq!(response.text().await.unwrap(), "");

    // Everything else is the MCP server's, with the request as the caller made it.
    let response = reqwest::Client::new()
        .post(front.url("/mcp"))
        .header(header::HOST, "localhost:8080")
        .header("x-agent-session", "tg:42")
        .body("{\"jsonrpc\":\"2.0\"}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.text().await.unwrap(),
        "host=localhost:8080 session=tg:42 body={\"jsonrpc\":\"2.0\"}"
    );
    for path in ["/", "/files", "/mcp/file"] {
        let response = reqwest::get(front.url(path)).await.unwrap();
        assert_eq!(response.status(), 404, "{path}");
        assert_eq!(
            response.text().await.unwrap(),
            "from the mcp server",
            "{path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_response_is_passed_on_while_it_is_still_being_produced() {
    let (front, more) = Front::ready(&["sleep", "60"]).await;
    let mut response = reqwest::get(front.url("/events")).await.unwrap();
    // The second line is not sent until this one has arrived here.
    let first = tokio::time::timeout(WAIT, response.chunk()).await;
    assert_eq!(first.unwrap().unwrap().unwrap(), "one\n");
    more.notify_one();
    let second = tokio::time::timeout(WAIT, response.chunk()).await;
    assert_eq!(second.unwrap().unwrap().unwrap(), "two\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_port_opens_only_once_the_mcp_server_listens() {
    let front = Front::start(&["sleep", "60"]);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!accepts_connections(front.addr));

    serve_mcp(front.mcp, Arc::new(Notify::new())).await;
    until("the front to listen", || accepts_connections(front.addr)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn everything_stops_when_the_mcp_server_does() {
    // Gone before it listened.
    let mut front = Front::start(&["sh", "-c", "exit 3"]);
    assert!(!front.exit().await.success());
    assert!(front.stderr().contains("before it was listening"));

    // Gone while serving.
    let (mut front, _more) = Front::ready(&["sleep", "1"]).await;
    assert!(!front.exit().await.success());
    assert!(front.stderr().contains("the MCP server stopped"));
    assert!(!accepts_connections(front.addr));

    // Never there at all.
    let mut front = Front::start(&["/nonexistent/mcp-server"]);
    assert!(!front.exit().await.success());
    assert!(front.stderr().contains("starting /nonexistent/mcp-server"));
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_front_stops_the_mcp_server() {
    let pid_file = tempfile::NamedTempFile::new().unwrap();
    let script = format!("echo $$ > {}; exec sleep 60", pid_file.path().display());
    let (mut front, _more) = Front::ready(&["sh", "-c", &script]).await;
    let pid = fs::read_to_string(pid_file.path())
        .unwrap()
        .trim()
        .to_owned();
    let running = || Path::new("/proc").join(&pid).exists();
    assert!(running());

    front.terminate();
    assert!(front.exit().await.success());
    until("the MCP server to be gone", || !running()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_front_stopped_while_its_mcp_server_is_starting_stops_it_too() {
    let pid_file = tempfile::NamedTempFile::new().unwrap();
    let script = format!("echo $$ > {}; exec sleep 60", pid_file.path().display());
    // Nothing ever listens where this MCP server is expected, so the front keeps waiting.
    let mut front = Front::start(&["sh", "-c", &script]);
    let pid = || {
        fs::read_to_string(pid_file.path())
            .unwrap()
            .trim()
            .to_owned()
    };
    until("the MCP server to start", || !pid().is_empty()).await;
    let pid = pid();
    let running = || Path::new("/proc").join(&pid).exists();
    assert!(running());

    front.terminate();
    assert!(front.exit().await.success());
    until("the MCP server to be gone", || !running()).await;
}
