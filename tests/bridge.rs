//! herdr 単独の daemon を端から端まで検証する。
//!
//! 偽 herdr socket を立て、
//! - RPC socket が開き、herdr の pane に居るクライアントが daemon に届くこと
//! - herdr pane 同士の会話が実際に配送される (agent.prompt が飛ぶ) こと
//! - hook を持たない agent が pull 登録で peer になること
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
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

mod common;
use tempfile::TempDir;

/// 偽 herdr。受け取った要求を記録し、可変の pane 構成を返す。
struct FakeHerdr {
    socket: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    /// `pane.get` が報告する `agent_status`。画面検出のラグを模すために可変。
    status: Arc<Mutex<String>>,
    /// `(pane_id, agent)` の一覧。稼働中の agent 出現・消滅を模すために可変。
    panes: Arc<Mutex<Vec<(String, String)>>>,
}

impl FakeHerdr {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(Mutex::new("idle".to_owned()));
        let panes = Arc::new(Mutex::new(vec![
            ("w1:p1".to_owned(), "codex".to_owned()),
            ("w1:p2".to_owned(), "claude".to_owned()),
        ]));
        let recorded = Arc::clone(&requests);
        let reported = Arc::clone(&status);
        let listing = Arc::clone(&panes);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let recorded = Arc::clone(&recorded);
                let reported = Arc::clone(&reported);
                let listing = Arc::clone(&listing);
                thread::spawn(move || serve_one(stream, &recorded, &reported, &listing));
            }
        });
        Self {
            socket,
            requests,
            status,
            panes,
        }
    }

    /// 稼働中の agent 出現を模す。
    fn add_pane(&self, pane_id: &str, agent: &str) {
        self.panes
            .lock()
            .unwrap()
            .push((pane_id.to_owned(), agent.to_owned()));
    }

    /// 稼働中の agent 消滅を模す。
    fn remove_pane(&self, pane_id: &str) {
        self.panes.lock().unwrap().retain(|(id, _)| id != pane_id);
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

fn serve_one(
    stream: UnixStream,
    recorded: &Arc<Mutex<Vec<Value>>>,
    status: &Arc<Mutex<String>>,
    panes: &Arc<Mutex<Vec<(String, String)>>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let Ok(request) = serde_json::from_str::<Value>(line.trim()) else {
        return;
    };
    let method = request["method"].as_str().unwrap_or_default().to_owned();
    let requested_pane = request["params"]["pane_id"]
        .as_str()
        .unwrap_or("w1:p1")
        .to_owned();
    recorded.lock().unwrap().push(request);
    let agent_status = status.lock().unwrap().clone();
    let pane_json = |pane_id: &str, agent: &str| {
        json!({
            "pane_id": pane_id,
            "terminal_id": format!("term_{pane_id}"),
            "workspace_id": "w1",
            "tab_id": "w1:t1",
            "cwd": "/tmp",
            "agent": agent,
            "agent_status": agent_status,
        })
    };
    let listing = panes.lock().unwrap().clone();
    let pane = listing
        .iter()
        .find(|(id, _)| *id == requested_pane)
        .map_or_else(
            || pane_json("w1:p1", "codex"),
            |(id, agent)| pane_json(id, agent),
        );
    let result = match method.as_str() {
        "ping" => json!({"type": "pong", "version": "0.7.5", "protocol": 17}),
        "pane.list" => json!({
            "type": "pane_list",
            "panes": listing
                .iter()
                .map(|(id, agent)| pane_json(id, agent))
                .collect::<Vec<_>>(),
        }),
        // 実機封筒 (2026-08-03 採取): workspace の人間向け名は label に入る。
        "workspace.list" => json!({
            "type": "workspace_list",
            "workspaces": [
                {"workspace_id": "w1", "number": 1, "label": "knowledge", "focused": true},
            ],
        }),
        // 実機封筒 (2026-08-14 採取): tab の label は required で、custom 名の
        // 無い tab は番号文字列が入る (→ 名前は runtime 検出名へ fallback)。
        // daemon は tab.list が取れない snapshot を fail-closed で捨てるため、
        // この分岐が無いと登録が一切進まない。
        "tab.list" => json!({
            "type": "tab_list",
            "tabs": [
                {"tab_id": "w1:t1", "workspace_id": "w1", "number": 1, "label": "1", "focused": true, "pane_count": 2, "agent_status": "idle"},
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
    herdr: FakeHerdr,
    runtime: PathBuf,
    state: PathBuf,
    http_port: u16,
}

impl Harness {
    fn start() -> Self {
        let root = TempDir::new().unwrap();
        let runtime = root.path().join("run");
        let state = root.path().join("state");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&state).unwrap();
        let herdr = FakeHerdr::start(root.path());

        // 空きポートを 1 つ確保して即座に手放し、その番号を daemon へ渡す。
        let http_port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        Self {
            _root: root,
            herdr,
            runtime,
            state,
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

    /// 指定の herdr pane に居るクライアントとして実行する。
    fn as_herdr_pane(&self, pane_id: &str, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-talk"));
        command
            .args(args)
            .envs(self.env())
            .env("HERDR_PANE_ID", pane_id)
            .env("HERDR_SOCKET_PATH", &self.herdr.socket)
            .env("HERDR_ENV", "1")
            .env_remove("AGENT_TALK_RPC_SOCKET");
        command.output().unwrap()
    }

    fn rpc_socket(&self) -> PathBuf {
        self.runtime.join("agent-talkd").join("herdr.sock")
    }

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

/// `sent -> ... : #N` / `queued (waiting) -> ... : #N` の末尾 ID。
fn trailing_id(output: &str) -> u64 {
    output
        .rsplit_once('#')
        .unwrap_or_else(|| panic!("id missing from {output:?}"))
        .1
        .trim()
        .parse()
        .unwrap()
}

/// 会話・画面・TCP 面を 1 本で通す実機形の検証。
#[test]
#[ignore = "spawns a background daemon; run explicitly"]
fn one_daemon_serves_herdr_panes_and_mobile_over_tcp() {
    let harness = Harness::start();

    // claude を登録する。ここで daemon が起動する。
    harness.ok(&harness.as_herdr_pane("w1:p2", &["register", "claude"]));
    wait_for(|| harness.rpc_socket().exists());
    assert!(
        UnixStream::connect(harness.rpc_socket()).is_ok(),
        "rpc socket に daemon が居ない"
    );

    // codex は hook を持たないが、pull 同期で数 tick 以内に peer になる。
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let who = harness.ok(&harness.as_herdr_pane("w1:p2", &["who"]));
        if common::has_agent(&who, "codex") {
            assert_eq!(common::agent_backend(&who, "codex"), Some("herdr"), "{who}");
            assert!(
                who.contains("knowledge:"),
                "location は workspace label で表示される: {who}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "codex discovery timed out: {who}"
        );
        thread::sleep(Duration::from_millis(200));
    }

    // 会話: claude → codex。呼び鈴は agent.prompt として届き、本文は運ばない。
    let sent = harness.ok(&harness.as_herdr_pane("w1:p2", &["send", "codex", "--", "hello codex"]));
    assert!(sent.contains("w1:p1"), "宛先が pane に解決される: {sent}");
    wait_for(|| !harness.herdr.prompts().is_empty());
    let (target, bell) = harness.herdr.prompts().remove(0);
    assert_eq!(target, "w1:p1");
    assert!(bell.contains("agent-talk"), "{bell:?}");
    assert!(bell.contains("read_message"), "{bell:?}");
    // 差出人は canonical full label (`<workspace label>/<name>`)。
    assert!(
        bell.contains("knowledge/claude から依頼が届きました"),
        "呼び鈴の差出人は workspace 付き: {bell:?}"
    );
    assert!(
        !bell.contains("hello codex"),
        "本文を呼び鈴に載せてはならない: {bell:?}"
    );

    // pane の Screen も HTTP から開ける。
    let screen = harness.http_get("/api/agents/w1%3Ap1/screen");
    assert!(screen.starts_with("HTTP/1.1 200"), "{screen}");
    assert!(screen.contains("fake herdr screen"), "{screen}");

    // スマホ向けの TCP 面が応答する。
    let hello = harness.http_get("/api/hello");
    assert!(hello.starts_with("HTTP/1.1 200"), "{hello}");
    assert!(hello.contains("agent-talk"), "{hello}");

    let _ = harness.as_herdr_pane("w1:p2", &["internal-daemon-shutdown"]);
}

/// 手紙 (POST /api/letters) の実機形 E2E: TCP から投函した手紙が既存の外部送信
/// 経路で pane に着弾し、mailbox 履歴に残る。allowlist 外の source と
/// JSON 以外の Content-Type は投函できない。
#[test]
#[ignore = "spawns a background daemon; run explicitly"]
fn a_letter_posted_over_tcp_reaches_a_pane() {
    let harness = Harness::start();
    harness.ok(&harness.as_herdr_pane("w1:p2", &["register", "claude"]));
    wait_for(|| harness.rpc_socket().exists());

    let letter = r#"{"source":"mobile","target":"w1:p2","body":"letter over tcp"}"#;
    let accepted = harness.http_post("/api/letters", "application/json", letter);
    assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
    assert!(accepted.contains(r#""path":"sent""#), "{accepted}");

    // 呼び鈴が agent.prompt として届く (本文は運ばない)。
    wait_for(|| {
        harness
            .herdr
            .prompts()
            .iter()
            .any(|(target, bell)| target == "w1:p2" && bell.contains("read_message"))
    });
    assert!(
        !harness
            .herdr
            .prompts()
            .iter()
            .any(|(_, bell)| bell.contains("letter over tcp")),
        "本文を呼び鈴に載せてはならない"
    );

    // mailbox 履歴に out event として残り、LettersPanel から見える。
    let history = harness.http_get("/api/mailbox/mobile?limit=10");
    assert!(history.starts_with("HTTP/1.1 200"), "{history}");
    assert!(history.contains("letter over tcp"), "{history}");

    // allowlist 外の source は 403 で、履歴も増えない。
    let rejected = harness.http_post(
        "/api/letters",
        "application/json",
        r#"{"source":"stranger","target":"w1:p2","body":"nope"}"#,
    );
    assert!(rejected.starts_with("HTTP/1.1 403"), "{rejected}");
    // JSON 以外の Content-Type は 415 (cross-site simple request の遮断)。
    let wrong_type = harness.http_post("/api/letters", "text/plain", letter);
    assert!(wrong_type.starts_with("HTTP/1.1 415"), "{wrong_type}");

    let _ = harness.as_herdr_pane("w1:p2", &["internal-daemon-shutdown"]);
}

/// 稼働中に herdr へ現れた agent (hook なし) が、数秒で peer になり、
/// 消えると次の成功 snapshot で登録が外れる。
#[test]
#[ignore = "spawns a background daemon; run explicitly"]
fn a_herdr_agent_appearing_mid_run_becomes_a_peer_without_hooks() {
    let harness = Harness::start();
    harness.ok(&harness.as_herdr_pane("w1:p2", &["register", "claude"]));
    wait_for(|| harness.rpc_socket().exists());

    // 稼働中の出現: 仕込みゼロで数 tick 以内に who へ載る。
    harness.herdr.add_pane("w1:p9", "grok");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let who = harness.ok(&harness.as_herdr_pane("w1:p2", &["who"]));
        if common::has_agent(&who, "grok") {
            assert_eq!(common::agent_backend(&who, "grok"), Some("herdr"), "{who}");
            break;
        }
        assert!(Instant::now() < deadline, "grok discovery timed out: {who}");
        thread::sleep(Duration::from_millis(200));
    }

    // 消滅: 次の成功 snapshot で即座に外れる。
    harness.herdr.remove_pane("w1:p9");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let who = harness.ok(&harness.as_herdr_pane("w1:p2", &["who"]));
        if !common::has_agent(&who, "grok") {
            break;
        }
        assert!(Instant::now() < deadline, "grok eviction timed out: {who}");
        thread::sleep(Duration::from_millis(200));
    }

    let _ = harness.as_herdr_pane("w1:p2", &["internal-daemon-shutdown"]);
}

/// blocked の間は queue に滞留し、idle の正の証拠が出た時点の health tick が
/// 同じ ID を FIFO のまま流す。新規 send は queue を追い越さない。
#[test]
#[ignore = "spawns a background daemon; run explicitly"]
fn a_blocked_pane_does_not_strand_queued_messages() {
    let harness = Harness::start();
    harness.ok(&harness.as_herdr_pane("w1:p2", &["register", "claude"]));
    wait_for(|| harness.rpc_socket().exists());
    harness.ok(&harness.as_herdr_pane("w1:p1", &["register", "codex"]));

    // 承認ダイアログ等で配達できない間を模す: herdr は blocked を報告し続ける。
    harness.herdr.report_status("blocked");
    let first =
        harness.ok(&harness.as_herdr_pane("w1:p2", &["send", "w1:p1", "--", "first letter"]));
    let first_id = trailing_id(&first);
    let second =
        harness.ok(&harness.as_herdr_pane("w1:p2", &["send", "w1:p1", "--", "second letter"]));
    let second_id = trailing_id(&second);
    assert!(second.contains("queued"), "{second}");
    // blocked の間は何度 tick が回っても prompt は出ない。
    thread::sleep(Duration::from_secs(3));
    assert!(
        harness.herdr.prompts().is_empty(),
        "blocked 中に prompt してはならない: {:?}",
        harness.herdr.prompts()
    );

    // 検出が idle に追いついた後、hook 無しで tick が流す。
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

    // hook は無い。herdr が idle のままなら次の reconcile tick が2通目を流す。
    wait_for(|| harness.herdr.prompts().len() >= 2);
    let prompts = harness.herdr.prompts();
    assert!(
        prompts[1].1.contains(&format!("read_message {second_id}")),
        "{prompts:?}"
    );

    let _ = harness.as_herdr_pane("w1:p2", &["internal-daemon-shutdown"]);
}

/// env を一切 forward されない MCP server が、daemon の peer PID 解決で
/// 自分の pane として会話できる (grok のような launcher の想定)。
///
/// 親 process (herdr が env を与えた agent に相当) だけが HERDR_* を持ち、
/// MCP 子 process は `env -i` で環境を落として起動する。
#[test]
#[ignore = "spawns a background daemon; run explicitly"]
fn an_env_free_mcp_child_is_identified_through_its_ancestor() {
    let harness = Harness::start();
    harness.ok(&harness.as_herdr_pane("w1:p2", &["register", "claude"]));
    wait_for(|| harness.rpc_socket().exists());

    // sh が「herdr が env を与えた agent」役 (HERDR_* を保持したまま生存)、
    // その子の mcp は XDG_RUNTIME_DIR 以外の env を持たない。
    // `sh -c '<単一 command>'` は shell が暗黙に exec して親が消えるため、
    // 後続 command を置いて sh を「HERDR_* を持つ祖先」として生存させる。
    let script = format!(
        "env -i XDG_RUNTIME_DIR={} {}; exit $?",
        harness.runtime.display(),
        env!("CARGO_BIN_EXE_agent-talk-mcp"),
    );
    let mut child = Command::new("sh")
        .args(["-c", &script])
        .env("HERDR_PANE_ID", "w1:p1")
        .env("HERDR_SOCKET_PATH", &harness.herdr.socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut call = |line: String| -> serde_json::Value {
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        let mut response = String::new();
        reader.read_line(&mut response).unwrap();
        assert!(!response.is_empty(), "MCP server closed stdout");
        serde_json::from_str(&response).unwrap()
    };

    // daemon が peer PID の祖先 (sh) から w1:p1 と解決し、codex として会話できる。
    let peers = call(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_peers","arguments":{}}}"#
            .to_owned(),
    );
    assert_eq!(
        peers["result"]["structuredContent"]["self"], "w1:p1",
        "{peers}"
    );
    let sent = call(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_message","arguments":{"to":"claude","body":"from env-free mcp"}}}"#
            .to_owned(),
    );
    assert_eq!(sent["result"]["structuredContent"]["to"], "w1:p2", "{sent}");

    drop(stdin);
    let _ = child.wait();
    let _ = harness.as_herdr_pane("w1:p2", &["internal-daemon-shutdown"]);
}

/// 非表示 tab の `done` バッジは配達を塞がない — user が tab を開かなくても
/// 呼び鈴が agent.prompt で届く。
#[test]
#[ignore = "spawns a background daemon; run explicitly"]
fn a_done_pane_receives_the_bell_without_the_user_opening_its_tab() {
    let harness = Harness::start();
    harness.ok(&harness.as_herdr_pane("w1:p2", &["register", "claude"]));
    wait_for(|| harness.rpc_socket().exists());

    // 全 pane が「ターン完了・未閲覧」の done を報告している状態。
    harness.herdr.report_status("done");
    let sent = harness.ok(&harness.as_herdr_pane("w1:p2", &["send", "w1:p1", "--", "for codex"]));
    assert!(
        sent.contains("sent"),
        "done は queue 行きにならず即配達: {sent}"
    );
    wait_for(|| !harness.herdr.prompts().is_empty());
    let (target, bell) = harness.herdr.prompts().remove(0);
    assert_eq!(target, "w1:p1");
    assert!(bell.contains("read_message"), "{bell:?}");

    let _ = harness.as_herdr_pane("w1:p2", &["internal-daemon-shutdown"]);
}
