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
    /// `pane.get` が報告する `agent_status`。画面検出のラグを模すために可変。
    status: Arc<Mutex<String>>,
}

impl FakeHerdr {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let alive = Arc::new(Mutex::new(true));
        let status = Arc::new(Mutex::new("idle".to_owned()));
        let recorded = Arc::clone(&requests);
        let serving = Arc::clone(&alive);
        let reported = Arc::clone(&status);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if !*serving.lock().unwrap() {
                    // 落ちた herdr を模す: 接続は即座に切る。
                    continue;
                }
                let recorded = Arc::clone(&recorded);
                let reported = Arc::clone(&reported);
                thread::spawn(move || serve_one(stream, &recorded, &reported));
            }
        });
        Self {
            socket,
            requests,
            alive,
            status,
        }
    }

    /// herdr が落ちた状態にする。socket file は残るが応答しなくなる。
    fn stop(&self) {
        *self.alive.lock().unwrap() = false;
    }

    /// 画面検出の報告値を切り替える (working = 検出ラグ中を模す)。
    fn report_status(&self, status: &str) {
        status.clone_into(&mut self.status.lock().unwrap());
    }

    fn prompts(&self) -> Vec<(String, String)> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["method"] == "agent.prompt")
            .filter_map(|request| {
                let target = request["params"]["target"].as_str()?;
                let text = request["params"]["text"].as_str()?;
                Some((target.to_owned(), text.to_owned()))
            })
            .collect()
    }
}

fn serve_one(stream: UnixStream, recorded: &Arc<Mutex<Vec<Value>>>, status: &Arc<Mutex<String>>) {
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
    let agent_status = status.lock().unwrap().clone();
    let pane = json!({
        "pane_id": "w1:p1",
        "terminal_id": "term_fake",
        "workspace_id": "w1",
        "tab_id": "w1:t1",
        "cwd": "/tmp",
        "agent": "codex",
        "agent_status": agent_status,
    });
    let result = match method.as_str() {
        "ping" => json!({"type": "pong", "version": "0.7.5", "protocol": 17}),
        "pane.list" => json!({"type": "pane_list", "panes": [pane]}),
        // 実機封筒 (2026-08-03 採取): workspace の人間向け名は label に入る。
        "workspace.list" => json!({
            "type": "workspace_list",
            "workspaces": [
                {"workspace_id": "w1", "number": 1, "label": "knowledge", "focused": true},
            ],
        }),
        // 実 herdr は method 別の封筒を持つ (2026-08-03 実機採取)。
        // pane.get の中身は result.pane にネストする。
        "pane.get" => json!({"type": "pane", "pane": pane}),
        "pane.read" => json!({
            "type": "read",
            "read": {
                "pane_id": "w1:p1",
                "workspace_id": "w1",
                "tab_id": "w1:t1",
                "source": "visible",
                "format": "text",
                "text": "fake herdr screen",
                "revision": 1,
                "truncated": false,
            },
        }),
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

    fn http_get(&self, path: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.http_port)).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn http_post(&self, path: &str, content_type: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.http_port)).unwrap();
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    /// 実 tmux pane の表示内容 (呼び鈴の着弾観測用)。
    fn capture_tmux_pane(&self) -> String {
        let output = Command::new("tmux")
            .args([
                "-S",
                &self.tmux_socket,
                "capture-pane",
                "-p",
                "-t",
                &self.tmux_pane,
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
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
    assert!(
        who.contains("knowledge:"),
        "location は workspace label で表示される: {who}"
    );

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
    // 宛先は pane id ではなく **workspace label** で引く (user 目的:
    // 「何処からでも knowledge/codex をすぐに見つけられる」)。
    let sent = harness.ok(&harness.as_tmux_pane(&[
        "send",
        "knowledge/codex",
        "--",
        "cross backend hello",
    ]));
    assert!(sent.contains("w1:p1"), "label が pane に解決される: {sent}");
    wait_for(|| !harness.herdr.prompts().is_empty());
    let (target, bell) = harness.herdr.prompts().remove(0);
    // 呼び鈴は agent.prompt として正しい pane の agent へ届く (send_text では
    // 入力欄に残るだけで turn が始まらない)。本文は運ばず ID だけを案内する。
    assert_eq!(target, "w1:p1");
    assert!(bell.contains("agent-talk"), "{bell:?}");
    assert!(bell.contains("read_message"), "{bell:?}");
    assert!(
        !bell.contains("cross backend hello"),
        "本文を呼び鈴に載せてはならない: {bell:?}"
    );

    // --- 達成条件 2 の逆向き: herdr → tmux も配送が実際に届く ---
    //
    // who の双方向性だけでは会話の双方向性を証明しない。herdr 側の
    // クライアントから tmux 側 agent へ送り、実 tmux pane に呼び鈴が
    // 打鍵されることを capture-pane で観測する。
    let sent_back = harness.ok(&harness.as_herdr_pane(&[
        "send",
        &harness.tmux_pane.clone(),
        "--",
        "reverse cross backend hello",
    ]));
    assert!(sent_back.contains('#'), "{sent_back}");
    wait_for(|| {
        let screen = harness.capture_tmux_pane();
        screen.contains("read_message")
    });
    assert!(
        !harness
            .capture_tmux_pane()
            .contains("reverse cross backend hello"),
        "本文は端末へ注入しない"
    );

    // herdr pane の Screen も HTTP から開ける (pane id 形式の両対応)。
    let herdr_screen = harness.http_get("/api/agents/w1%3Ap1/screen");
    assert!(herdr_screen.starts_with("HTTP/1.1 200"), "{herdr_screen}");
    assert!(herdr_screen.contains("fake herdr screen"), "{herdr_screen}");

    // --- 達成条件 3: スマホ向けの TCP 面が応答する ---
    let mut stream = TcpStream::connect(("127.0.0.1", harness.http_port)).unwrap();
    stream
        .write_all(
            b"GET /api/hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("agent-talk"), "{response}");

    let _ = harness.as_tmux_pane(&["internal-daemon-shutdown"]);
}

/// 手紙 (POST /api/letters) の実機 E2E: TCP から投函した手紙が既存の外部送信
/// 経路で実 pane に着弾し、mailbox 履歴に残る。allowlist 外の source と
/// JSON 以外の Content-Type は投函できない。
#[test]
#[ignore = "requires permission to create a real tmux server"]
fn a_letter_posted_over_tcp_reaches_a_real_pane() {
    let harness = Harness::start();
    harness.ok(&harness.as_tmux_pane(&["register", "claude"]));
    wait_for(|| harness.tmux_rpc_socket().exists());

    let letter = format!(
        r#"{{"source":"mobile","target":"{}","body":"letter over tcp"}}"#,
        harness.tmux_pane
    );
    let accepted = harness.http_post("/api/letters", "application/json", &letter);
    assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
    assert!(accepted.contains(r#""path":"sent""#), "{accepted}");

    // 実 tmux pane に呼び鈴が打鍵される (本文は運ばない)。
    wait_for(|| harness.capture_tmux_pane().contains("read_message"));
    assert!(
        !harness.capture_tmux_pane().contains("letter over tcp"),
        "本文を端末へ注入しない"
    );

    // mailbox 履歴に out event として残り、LettersPanel から見える。
    let history = harness.http_get("/api/mailbox/mobile?limit=10");
    assert!(history.starts_with("HTTP/1.1 200"), "{history}");
    assert!(history.contains("letter over tcp"), "{history}");

    // allowlist 外の source は 403 で、履歴も増えない。
    let rejected = harness.http_post(
        "/api/letters",
        "application/json",
        &format!(
            r#"{{"source":"stranger","target":"{}","body":"nope"}}"#,
            harness.tmux_pane
        ),
    );
    assert!(rejected.starts_with("HTTP/1.1 403"), "{rejected}");
    // JSON 以外の Content-Type は 415 (cross-site simple request の遮断)。
    let wrong_type = harness.http_post("/api/letters", "text/plain", &letter);
    assert!(wrong_type.starts_with("HTTP/1.1 415"), "{wrong_type}");

    let _ = harness.as_tmux_pane(&["internal-daemon-shutdown"]);
}

/// 検出ラグの回帰: turn-end の一瞬に herdr がまだ working を返しても、
/// queue は滞留せず、idle の正の証拠が出た時点の health tick が同じ ID を
/// FIFO のまま流す。新規 send は queue を追い越さない。
#[test]
#[ignore = "requires permission to create a real tmux server"]
fn a_lagging_herdr_detection_does_not_strand_queued_messages() {
    let harness = Harness::start();
    harness.ok(&harness.as_tmux_pane(&["register", "claude"]));
    wait_for(|| harness.tmux_rpc_socket().exists() && harness.herdr_rpc_socket().exists());
    harness.ok(&harness.as_herdr_pane(&["register", "codex"]));

    // 画面検出のラグを模す: herdr は working を報告し続ける。
    harness.herdr.report_status("working");
    let first = harness.ok(&harness.as_tmux_pane(&["send", "w1:p1", "--", "first letter"]));
    let first_id = trailing_id(&first);
    let second = harness.ok(&harness.as_tmux_pane(&["send", "w1:p1", "--", "second letter"]));
    let second_id = trailing_id(&second);
    assert!(second.contains("queued"), "{second}");
    // working の間は何度 tick が回っても prompt は出ない。
    thread::sleep(Duration::from_secs(3));
    assert!(
        harness.herdr.prompts().is_empty(),
        "working 中に prompt してはならない: {:?}",
        harness.herdr.prompts()
    );

    // 検出が idle に追いついた後、turn-end を発火させなくても tick が流す。
    harness.herdr.report_status("idle");
    wait_for(|| !harness.herdr.prompts().is_empty());
    let prompts = harness.herdr.prompts();
    assert!(
        prompts[0].1.contains(&format!("read_message {first_id}")),
        "先に送った ID が先に配達される: {prompts:?}"
    );
    assert_eq!(
        prompts.len(),
        1,
        "1 tick で流すのは先頭の1件だけ: {prompts:?}"
    );

    // 受信側の turn-end で2通目が続き、順序が送信順と一致する。
    harness.ok(&harness.as_herdr_pane(&["turn-end"]));
    wait_for(|| harness.herdr.prompts().len() >= 2);
    let prompts = harness.herdr.prompts();
    assert!(
        prompts[1].1.contains(&format!("read_message {second_id}")),
        "{prompts:?}"
    );

    let _ = harness.as_tmux_pane(&["internal-daemon-shutdown"]);
}

/// `sent -> ... : #N` / `queued (busy) -> ... : #N` の末尾 ID。
fn trailing_id(output: &str) -> u64 {
    output
        .rsplit_once('#')
        .unwrap_or_else(|| panic!("id missing from {output:?}"))
        .1
        .trim()
        .parse()
        .unwrap()
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
