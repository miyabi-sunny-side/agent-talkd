//! tmux と herdr を 1 つの daemon が橋渡しすることの端から端までの検証。
//!
//! 実 tmux (隔離サーバー) と偽 herdr socket を同時に立て、
//! - 両 backend 分の RPC socket が開くこと
//! - herdr 側から繋いだクライアントが tmux 側の agent を見られること
//! - tmux 側から herdr 側の agent へ実際に配送されること
//! - HTTP が TCP でも応答すること (agent-terrace 相当)
//!
//! を確認する。

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

mod common;
use tempfile::TempDir;

/// 隔離した tmux サーバー。Drop で必ず落とす。
struct TmuxServer {
    name: String,
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.name, "kill-server"])
            .output();
    }
}

/// 偽 herdr。受け取った要求を記録し、固定の pane 構成を返す。
struct FakeHerdr {
    socket: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    alive: Arc<Mutex<bool>>,
}

impl FakeHerdr {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let alive = Arc::new(Mutex::new(true));
        let recorded = Arc::clone(&requests);
        let serving = Arc::clone(&alive);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if !*serving.lock().unwrap() {
                    // 落ちた herdr を模す: 接続は即座に切る。
                    continue;
                }
                let recorded = Arc::clone(&recorded);
                thread::spawn(move || serve_one(stream, &recorded));
            }
        });
        Self {
            socket,
            requests,
            alive,
        }
    }

    /// herdr が落ちた状態にする。socket file は残るが応答しなくなる。
    fn stop(&self) {
        *self.alive.lock().unwrap() = false;
    }

    fn sent_text(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "pane.send_text")
            .filter_map(|request| request["params"]["text"].as_str().map(str::to_owned))
            .collect()
    }
}

fn serve_one(stream: UnixStream, recorded: &Arc<Mutex<Vec<Value>>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let Ok(request) = serde_json::from_str::<Value>(line.trim()) else {
        return;
    };
    let method = request["method"].as_str().unwrap_or_default().to_owned();
    recorded.lock().unwrap().push(request);
    let pane = json!({
        "pane_id": "w1:p1",
        "terminal_id": "term_fake",
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "cwd": "/tmp",
        "agent": "codex",
        "agent_status": "idle",
    });
    let result = match method.as_str() {
        "ping" => json!({"type": "pong", "version": "0.7.5", "protocol": 17}),
        "pane.list" => json!({"type": "pane_list", "panes": [pane]}),
        "pane.get" => pane,
        _ => json!({}),
    };
    let mut stream = stream;
    let _ = stream.write_all(format!("{}\n", json!({"id": "x", "result": result})).as_bytes());
}

struct Harness {
    _root: TempDir,
    _tmux: TmuxServer,
    herdr: FakeHerdr,
    tmux_socket: String,
    runtime: PathBuf,
    state: PathBuf,
    tmux_pane: String,
    http_port: u16,
}

/// テストごとに別の tmux サーバーを与える連番。同名だと並列実行時に互いの
/// pane を壊し、Drop の `kill-server` が相手のサーバーまで落とす。
static NEXT_SERVER: AtomicUsize = AtomicUsize::new(0);

impl Harness {
    fn start() -> Self {
        let root = TempDir::new().unwrap();
        let name = format!(
            "agent-talkd-bridge-{}-{}",
            std::process::id(),
            NEXT_SERVER.fetch_add(1, Ordering::Relaxed)
        );
        let tmux = TmuxServer { name: name.clone() };
        let runtime = root.path().join("run");
        let state = root.path().join("state");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&state).unwrap();
        let herdr = FakeHerdr::start(root.path());

        Command::new("tmux")
            .args(["-L", &name, "new-session", "-d", "-s", "bridge"])
            .output()
            .unwrap();
        let tmux_socket = String::from_utf8(
            Command::new("tmux")
                .args(["-L", &name, "display-message", "-p", "#{socket_path}"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let tmux_pane = String::from_utf8(
            Command::new("tmux")
                .args(["-L", &name, "list-panes", "-F", "#{pane_id}"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();

        // 空きポートを 1 つ確保して即座に手放し、その番号を daemon へ渡す。
        let http_port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        Self {
            _root: root,
            _tmux: tmux,
            herdr,
            tmux_socket,
            runtime,
            state,
            tmux_pane,
            http_port,
        }
    }

    fn env(&self) -> HashMap<&'static str, String> {
        HashMap::from([
            ("XDG_RUNTIME_DIR", self.runtime.display().to_string()),
            ("XDG_STATE_HOME", self.state.display().to_string()),
            (
                "AGENT_TALK_HERDR_SOCKET",
                self.herdr.socket.display().to_string(),
            ),
            (
                "AGENT_TALK_HTTP_ADDR",
                format!("127.0.0.1:{}", self.http_port),
            ),
        ])
    }

    /// tmux の pane に居るクライアントとして実行する。
    fn as_tmux_pane(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-talk"));
        command
            .args(args)
            .envs(self.env())
            .env("AGENT_TALK_TMUX_SOCKET", &self.tmux_socket)
            .env("TMUX", format!("{},1,0", self.tmux_socket))
            .env("TMUX_PANE", &self.tmux_pane)
            .env_remove("AGENT_TALK_RPC_SOCKET")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_SOCKET_PATH");
        command.output().unwrap()
    }

    /// herdr の pane に居るクライアントとして実行する。
    /// tmux の環境変数は一切持たない。
    fn as_herdr_pane(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-talk"));
        command
            .args(args)
            .envs(self.env())
            .env("HERDR_PANE_ID", "w1:p1")
            .env("HERDR_SOCKET_PATH", &self.herdr.socket)
            .env("HERDR_ENV", "1")
            .env_remove("AGENT_TALK_RPC_SOCKET")
            .env_remove("AGENT_TALK_TMUX_SOCKET")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        command.output().unwrap()
    }

    fn tmux_rpc_socket(&self) -> PathBuf {
        self.runtime
            .join("agent-talkd")
            .join(Path::new(&self.tmux_socket).file_name().unwrap())
            .with_extension("sock")
    }

    fn herdr_rpc_socket(&self) -> PathBuf {
        self.runtime.join("agent-talkd").join("herdr.sock")
    }
}

impl Harness {
    fn log(&self) -> String {
        fs::read_to_string(self.state.join("agent-talkd/agent-talkd.log")).unwrap_or_default()
    }

    fn ok(&self, output: &Output) -> String {
        assert!(
            output.status.success(),
            "command failed: {}\ndaemon log:\n{}",
            String::from_utf8_lossy(&output.stderr),
            self.log()
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(25));
    }
}

/// 達成条件 1〜3 を 1 本で通す。
///
/// spike なので個別に分けず、「実際に体験できる形」をそのまま検証する。
#[test]
#[ignore = "requires permission to create a real tmux server"]
fn one_daemon_bridges_tmux_and_herdr_and_serves_mobile_over_tcp() {
    let harness = Harness::start();

    // tmux 側の agent を登録する。ここで daemon が起動する。
    harness.ok(&harness.as_tmux_pane(&["register", "claude"]));

    // --- 達成条件 1: 両 backend 分の socket が開く ---
    wait_for(|| harness.tmux_rpc_socket().exists() && harness.herdr_rpc_socket().exists());

    // herdr 側の agent を登録する。tmux の環境変数を一切持たないクライアントが
    // herdr 由来の socket 経由で **同じ daemon** に届いていることの証明でもある。
    harness.ok(&harness.as_herdr_pane(&["register", "codex"]));

    // --- 達成条件 1: who に両方の pane が出る ---
    //
    // agent 名の列で判定する。cwd 列との偶然の一致で通ってしまわないように。
    let who = harness.ok(&harness.as_tmux_pane(&["who"]));
    assert!(
        common::has_agent(&who, "claude"),
        "tmux 側が見えない: {who}"
    );
    assert!(
        common::has_agent(&who, "codex"),
        "herdr 側が見えない: {who}"
    );
    // backend 列は「どの agent がどちらに居るか」まで含めて確かめる。
    assert_eq!(common::agent_backend(&who, "claude"), Some("tmux"), "{who}");
    assert_eq!(common::agent_backend(&who, "codex"), Some("herdr"), "{who}");

    // 逆向きも成立する: herdr 側のクライアントから tmux 側の agent が見える。
    let who_from_herdr = harness.ok(&harness.as_herdr_pane(&["who"]));
    assert!(
        common::has_agent(&who_from_herdr, "claude"),
        "herdr 側から tmux 側の agent が見えない: {who_from_herdr}"
    );

    // --- 達成条件 2: tmux → herdr の配送が実際に届く ---
    //
    // 宛先は pane id で明示する。tmux の session と herdr の workspace は別の
    // 名前空間であり、「セッションを跨ぐ暗黙の解決はしない」という既存契約が
    // そのまま効くため、backend をまたぐときは明示 scope が要る。
    let sent = harness.ok(&harness.as_tmux_pane(&["send", "w1:p1", "--", "cross backend hello"]));
    assert!(sent.contains("w1:p1"), "{sent}");
    wait_for(|| !harness.herdr.sent_text().is_empty());
    let delivered = harness.herdr.sent_text().join("");
    assert!(
        delivered.contains("agent-talk"),
        "herdr へ呼び鈴が届いていない: {delivered:?}"
    );

    // --- 達成条件 3: スマホ向けの TCP 面が応答する ---
    let mut stream = TcpStream::connect(("127.0.0.1", harness.http_port)).unwrap();
    stream
        .write_all(
            b"GET /v1/hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("agent-talk"), "{response}");

    let _ = harness.as_tmux_pane(&["internal-daemon-shutdown"]);
}

/// 達成条件 1 の後半: **片方が落ちても、もう片方の会話は続く**。
///
/// 移行期には「tmux は生きているが herdr を止めた」が普通に起きる。
/// そこで daemon ごと死ぬと、tmux 側の agent の会話まで巻き添えになる。
#[test]
#[ignore = "requires permission to create a real tmux server"]
fn losing_one_multiplexer_does_not_take_down_the_other() {
    let harness = Harness::start();
    harness.ok(&harness.as_tmux_pane(&["register", "claude"]));
    wait_for(|| harness.tmux_rpc_socket().exists() && harness.herdr_rpc_socket().exists());
    harness.ok(&harness.as_herdr_pane(&["register", "codex"]));

    // herdr を落とす。以降 pane.list も ping も失敗する。
    harness.herdr.stop();

    // health check (2 秒間隔) を数回跨いでも daemon は生きていること。
    thread::sleep(Duration::from_secs(5));
    let who = harness.ok(&harness.as_tmux_pane(&["who"]));
    assert!(
        common::has_agent(&who, "claude"),
        "herdr が落ちたら tmux 側まで見えなくなった: {who}"
    );

    // tmux 側だけで送受信が続けられること。
    harness.ok(&harness.as_tmux_pane(&["send", "%0", "--", "still alive"]));

    let _ = harness.as_tmux_pane(&["internal-daemon-shutdown"]);
}

/// 達成条件 1 の起動順: **herdr だけの daemon が先に居ても、
/// 最終的に 1 つの daemon が両方を監視する状態へ収束する**。
///
/// 収束しないと、後から tmux 側で起動したクライアントは永久に daemon を
/// 持てず (bind 済み socket file だけが残る)、backend をまたぐ会話が壊れる。
#[test]
#[ignore = "requires permission to create a real tmux server"]
fn a_herdr_only_daemon_is_replaced_by_one_that_serves_both() {
    let harness = Harness::start();

    // まず herdr だけを知る daemon を立てる。
    harness.ok(&harness.as_herdr_pane(&["register", "codex"]));
    wait_for(|| harness.herdr_rpc_socket().exists());
    assert!(
        !harness.tmux_rpc_socket().exists(),
        "herdr only の daemon が tmux socket まで開いている"
    );

    // 次に tmux 側から起動する。ここが収束点。
    harness.ok(&harness.as_tmux_pane(&["register", "claude"]));
    wait_for(|| harness.tmux_rpc_socket().exists());

    // 両方の socket が **生きている** こと。file が残っているだけでは駄目。
    assert!(
        UnixStream::connect(harness.tmux_rpc_socket()).is_ok(),
        "tmux socket に daemon が居ない (bind されただけの死んだ file)"
    );
    assert!(
        UnixStream::connect(harness.herdr_rpc_socket()).is_ok(),
        "herdr socket に daemon が居ない"
    );

    // 1 つの registry を共有していること。どちらの経路からも両方見える。
    let from_tmux = harness.ok(&harness.as_tmux_pane(&["who"]));
    assert!(common::has_agent(&from_tmux, "claude"), "{from_tmux}");
    assert!(common::has_agent(&from_tmux, "codex"), "{from_tmux}");
    let from_herdr = harness.ok(&harness.as_herdr_pane(&["who"]));
    assert!(common::has_agent(&from_herdr, "claude"), "{from_herdr}");
    assert!(common::has_agent(&from_herdr, "codex"), "{from_herdr}");

    let _ = harness.as_tmux_pane(&["internal-daemon-shutdown"]);
}
