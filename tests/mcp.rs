//! `agent-talk-mcp` の process 境界テスト。
//!
//! 実 herdr を必要としない検証をここに置く: 起動時 contract の fail closed、
//! 導出した UDS への実接続、tool surface の固定 (docs/decisions/0001-*.md)。

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
};

use serde_json::{Value, json};

/// production 経路は `AGENT_TALK_RPC_SOCKET` を読まないため、socket は
/// `XDG_RUNTIME_DIR` + `HERDR_SOCKET_PATH` から導出される。テストもその規則だけに従う。
fn derived_socket(runtime: &Path) -> PathBuf {
    runtime.join("agent-talkd").join("herdr.sock")
}

fn mcp(env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-talk-mcp"));
    command
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_PANE_ID")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("HOME")
        .env_remove("AGENT_TALK_RPC_SOCKET");
    for (key, value) in env {
        command.env(key, value);
    }
    command
}

struct Session {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl Session {
    fn start(env: &[(&str, &str)]) -> Self {
        let mut child = mcp(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        Self { child, reader }
    }

    fn call(&mut self, request: &Value) -> Value {
        let mut line = serde_json::to_vec(request).unwrap();
        line.push(b'\n');
        self.child.stdin.as_mut().unwrap().write_all(&line).unwrap();
        let mut response = String::new();
        self.reader.read_line(&mut response).unwrap();
        assert!(!response.is_empty(), "MCP server closed stdout");
        serde_json::from_str(&response).unwrap()
    }

    fn notify(&mut self, request: &Value) {
        let mut line = serde_json::to_vec(request).unwrap();
        line.push(b'\n');
        self.child.stdin.as_mut().unwrap().write_all(&line).unwrap();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

/// fake daemon の共通 socket loop。受けた `Request` を記録しつつ、
/// response policy (request -> Response JSON) だけを差し替える。
fn scripted_daemon(
    socket: &Path,
    policy: impl Fn(&Value) -> Value + Send + 'static,
) -> mpsc::Receiver<Value> {
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(socket).unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            let response = policy(&request);
            let mut stream = stream;
            let mut encoded = serde_json::to_vec(&response).unwrap();
            encoded.push(b'\n');
            stream.write_all(&encoded).unwrap();
            stream.flush().unwrap();
            // 記録は best-effort。receiver を捨てた caller (記録不要の fake) でも
            // accept loop を止めない。
            let _ = tx.send(request);
        }
    });
    rx
}

/// 実 daemon の `Response::error("unknown command")` と同形の応答。
fn unknown_command() -> Value {
    json!({
        "code": 1,
        "stdout": "",
        "stderr": "agent-talk: unknown command\n",
    })
}

fn ok_response(stdout: &str) -> Value {
    json!({ "code": 0, "stdout": stdout, "stderr": "" })
}

/// canonical 名で応答する現行 daemon の代役。
fn fake_daemon(socket: &Path) -> mpsc::Receiver<Value> {
    scripted_daemon(socket, |request| {
        match request["command"].as_str().unwrap() {
            "list-peers" => ok_response(
                "{\"version\":1,\"self\":\"w1:p1\",\"pending_to_me\":[3],\"peers\":[{\"name\":\"codex\",\"state\":\"idle\",\"location\":\"work:0.0\",\"pane\":\"w1:p2\",\"cwd\":\"/tmp\",\"queued\":0,\"pending_from_me\":[9]}]}\n",
            ),
            "send-message" => ok_response(
                "{\"version\":1,\"id\":9,\"path\":\"sent\",\"to\":\"w1:p2\",\"name\":\"codex\"}\n",
            ),
            "read-message" => ok_response(
                "{\"version\":1,\"id\":3,\"from\":\"codex\",\"reply_to\":\"w1:p2\",\"body\":\"# agent-talk 連絡\\n本文\"}\n",
            ),
            "ack-message" => ok_response("{\"version\":1,\"id\":3,\"outcome\":\"acked\"}\n"),
            _ => unknown_command(),
        }
    })
}

/// canonical 名を知らない旧 daemon の代役。canonical 名には実 daemon と同形の
/// `unknown command` を返し、旧 `*-v1` 名にだけ成功応答を返す。
fn legacy_daemon(socket: &Path) -> mpsc::Receiver<Value> {
    scripted_daemon(socket, |request| {
        match request["command"].as_str().unwrap() {
            "peers-v1" => ok_response(
                "{\"version\":1,\"self\":\"w1:p1\",\"pending_to_me\":[],\"peers\":[]}\n",
            ),
            "read-v1" => ok_response(
                "{\"version\":1,\"id\":3,\"from\":\"codex\",\"reply_to\":null,\"body\":\"本文\"}\n",
            ),
            "ack-v1" => ok_response("{\"version\":1,\"id\":3,\"outcome\":\"acked\"}\n"),
            "send-message-v1" if request["send_options"].is_object() => ok_response(
                "{\"version\":1,\"id\":9,\"path\":\"sent\",\"to\":\"w1:p2\",\"name\":\"codex\"}\n",
            ),
            _ => unknown_command(),
        }
    })
}

/// どの command にも「登録されていない」型の (unknown ではない) エラーを返す fake daemon。
fn rejecting_daemon(socket: &Path) -> mpsc::Receiver<Value> {
    scripted_daemon(socket, |_| {
        json!({
            "code": 1,
            "stdout": "",
            "stderr": "agent-talk: この操作は登録済みのagent paneからのみ実行できます\n",
        })
    })
}

/// 契約外の「成功」応答だけを返す fake daemon。
/// MCP は暗黙に劣化させず、すべて `isError: true` にしなければならない。
fn degraded_daemon(socket: &Path) {
    let _requests = scripted_daemon(socket, |request| {
        let stdout = match request["command"].as_str().unwrap() {
            // 旧テキスト形式 (versioned JSON への移行前の形)
            "send-message" => "sent -> w1:p2 (codex): #9\n",
            // version フィールドが無い
            "read-message" => "{\"id\":3,\"from\":\"codex\"}\n",
            // object ではない
            "ack-message" => "[1,2,3]\n",
            // 空応答
            _ => "\n",
        };
        ok_response(stdout)
    });
}

#[test]
fn a_success_response_outside_the_contract_never_degrades_into_a_tool_success() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let herdr_socket = root.path().join("herdr.sock");
    degraded_daemon(&derived_socket(&runtime));

    let mut session = Session::start(&[
        ("HERDR_SOCKET_PATH", herdr_socket.to_str().unwrap()),
        ("HERDR_PANE_ID", "w1:p1"),
        ("XDG_RUNTIME_DIR", runtime.to_str().unwrap()),
    ]);
    for (index, (name, arguments)) in [
        ("list_peers", json!({})),
        ("send_message", json!({"to": "codex", "body": "本文"})),
        ("read_message", json!({"id": 3})),
        ("ack_message", json!({"id": 3})),
    ]
    .into_iter()
    .enumerate()
    {
        let response = session.call(&json!({
            "jsonrpc": "2.0", "id": index + 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }));
        assert_eq!(
            response["result"]["isError"], true,
            "{name} must not degrade silently: {response}"
        );
        assert!(
            response["result"].get("structuredContent").is_none(),
            "{name} must not present a structured result it could not build: {response}"
        );
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("応答を解釈できません"),
            "{name}: {response}"
        );
    }
}

#[test]
fn version_flag_answers_without_any_environment_contract() {
    // daemon との世代ずれの機械検出用。HERDR_* も XDG も無い環境で成立すること。
    let output = mcp(&[]).arg("--version").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("agent-talk-mcp {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unexpected_arguments_are_rejected_before_the_server_starts() {
    for args in [&["--help"][..], &["--version", "extra"][..]] {
        let mut command = mcp(&[]);
        command.args(args).stdin(Stdio::null());
        let output = command.output().unwrap();
        assert!(!output.status.success(), "{args:?} must be rejected");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage"),
            "server 起動 (環境 contract 検査) へ落とさず usage で拒否する: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn only_a_broken_runtime_root_or_socket_path_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let runtime = runtime.to_str().unwrap();
    // HERDR_* は無くてよい (接続先は既定 path、identity は daemon 側で解決)。
    // 起動を拒むのは「設定されているのに壊れている」入力だけ。
    let cases: Vec<Vec<(&str, &str)>> = vec![
        // HERDR_SOCKET_PATH が書式違反 (相対 path)
        vec![
            ("HERDR_SOCKET_PATH", "relative/herdr.sock"),
            ("XDG_RUNTIME_DIR", runtime),
        ],
        // XDG_RUNTIME_DIR が不正 (相対 path) — HOME があっても fallback しない
        vec![
            ("XDG_RUNTIME_DIR", "relative/run"),
            ("HOME", "/home/tester"),
        ],
        // XDG_RUNTIME_DIR も HOME も無い
        vec![("HERDR_SOCKET_PATH", "/run/herdr/herdr.sock")],
    ];
    for case in cases {
        let output = mcp(&case)
            .stdin(Stdio::null())
            .output()
            .expect("mcp should start");
        assert!(
            !output.status.success(),
            "{case:?} must fail closed, got {output:?}"
        );
        assert!(
            output.stdout.is_empty(),
            "{case:?} must not serve anything on stdout"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("agent-talk-mcp:"),
            "{case:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // HERDR_* が一切無くても起動し、tool を公開する (env forward 不要)。
    let output = mcp(&[("XDG_RUNTIME_DIR", runtime)])
        .stdin(Stdio::null())
        .output()
        .expect("mcp should start");
    assert!(
        output.status.success(),
        "HERDR_* 無しでも fail closed してはならない: {output:?}"
    );
}

#[test]
fn home_fallback_reaches_the_same_socket_layout_as_the_daemon() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let runtime = home.join(".cache/agent-talkd/run");
    let herdr_socket = root.path().join("herdr.sock");
    let requests = fake_daemon(&derived_socket(&runtime));

    let mut session = Session::start(&[
        ("HERDR_SOCKET_PATH", herdr_socket.to_str().unwrap()),
        ("HERDR_PANE_ID", "w1:p7"),
        ("HOME", home.to_str().unwrap()),
    ]);
    let response = session.call(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "list_peers", "arguments": {}}
    }));
    assert_eq!(response["result"]["isError"], false, "{response}");
    let request = requests.recv().unwrap();
    assert_eq!(request["command"], "list-peers");
    assert_eq!(request["pane"], "w1:p7");
}

#[test]
#[allow(clippy::too_many_lines)]
fn the_four_tools_round_trip_over_the_derived_unix_socket() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let herdr_socket = root.path().join("herdr.sock");
    let requests = fake_daemon(&derived_socket(&runtime));

    let mut session = Session::start(&[
        ("HERDR_SOCKET_PATH", herdr_socket.to_str().unwrap()),
        ("HERDR_PANE_ID", "w1:p1"),
        ("XDG_RUNTIME_DIR", runtime.to_str().unwrap()),
        ("HOME", "/nonexistent-home"),
    ]);

    let initialize = session.call(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}
    }));
    assert_eq!(initialize["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(initialize["result"]["serverInfo"]["name"], "agent-talk");
    let instructions = initialize["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("ack_message"), "{instructions}");
    session.notify(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let listed = session.call(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let serialized = serde_json::to_string(&listed["result"]).unwrap();
    for forbidden in ["skill", "from", "pane"] {
        assert!(
            !serialized.contains(forbidden),
            "tools/list must not contain {forbidden:?}: {serialized}"
        );
    }
    let names: Vec<_> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        ["list_peers", "send_message", "read_message", "ack_message"]
    );

    let peers = session.call(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "list_peers", "arguments": {}}
    }));
    assert_eq!(peers["result"]["structuredContent"]["pending_to_me"][0], 3);
    assert_eq!(
        peers["result"]["structuredContent"]["peers"][0]["pending_from_me"][0],
        9
    );

    let sent = session.call(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "send_message", "arguments": {"to": "codex", "body": "本文\n2行目", "no_reply": true}}
    }));
    assert_eq!(sent["result"]["structuredContent"]["id"], 9);
    assert_eq!(sent["result"]["structuredContent"]["path"], "sent");

    let read = session.call(&json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {"name": "read_message", "arguments": {"id": 3}}
    }));
    assert_eq!(read["result"]["structuredContent"]["from"], "codex");
    assert_eq!(read["result"]["structuredContent"]["reply_to"], "w1:p2");

    let acked = session.call(&json!({
        "jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": {"name": "ack_message", "arguments": {"id": 3}}
    }));
    assert_eq!(acked["result"]["structuredContent"]["outcome"], "acked");

    let observed: Vec<Value> = (0..4).map(|_| requests.recv().unwrap()).collect();
    assert_eq!(observed[0]["command"], "list-peers");
    // 呼び出し元 identity は adapter が spawn 時の HERDR_PANE_ID から導出する。
    for request in &observed {
        assert_eq!(request["pane"], "w1:p1");
    }
    assert_eq!(observed[1]["command"], "send-message");
    assert_eq!(observed[1]["args"], json!(["codex"]));
    assert_eq!(observed[1]["stdin"], "本文\n2行目");
    assert_eq!(observed[1]["send_options"]["no_reply"], true);
    assert_eq!(observed[1]["send_options"]["skill"], Value::Null);
    assert_eq!(observed[1]["send_options"]["from"], Value::Null);
    assert_eq!(observed[2]["command"], "read-message");
    assert_eq!(observed[2]["args"], json!(["3"]));
    assert_eq!(observed[3]["command"], "ack-message");
}

#[test]
fn a_new_adapter_falls_back_to_legacy_names_only_on_unknown_command() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let herdr_socket = root.path().join("herdr.sock");
    let requests = legacy_daemon(&derived_socket(&runtime));

    let mut session = Session::start(&[
        ("HERDR_SOCKET_PATH", herdr_socket.to_str().unwrap()),
        ("HERDR_PANE_ID", "w1:p1"),
        ("XDG_RUNTIME_DIR", runtime.to_str().unwrap()),
    ]);
    // 旧 daemon 相手でも 4 tool すべて成功する (canonical → unknown → 旧名で成立)。
    let peers = session.call(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "list_peers", "arguments": {}}
    }));
    assert_eq!(peers["result"]["isError"], false, "{peers}");
    let read = session.call(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "read_message", "arguments": {"id": 3}}
    }));
    assert_eq!(read["result"]["structuredContent"]["id"], 3, "{read}");
    let sent = session.call(&json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "send_message", "arguments": {"to": "codex", "body": "本文"}}
    }));
    assert_eq!(
        sent["result"]["structuredContent"]["path"], "sent",
        "{sent}"
    );
    let acked = session.call(&json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "ack_message", "arguments": {"id": 3}}
    }));
    assert_eq!(
        acked["result"]["structuredContent"]["outcome"], "acked",
        "{acked}"
    );

    // 各 tool は canonical を先に送り、明示 unknown を見てから旧名で1回だけ再試行する。
    let observed: Vec<String> = (0..8)
        .map(|_| {
            requests.recv().unwrap()["command"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        observed,
        [
            "list-peers",
            "peers-v1",
            "read-message",
            "read-v1",
            "send-message",
            "send-message-v1",
            "ack-message",
            "ack-v1"
        ]
    );
    assert!(
        requests.try_recv().is_err(),
        "再試行は operation ごとに1回まで"
    );
}

#[test]
fn an_error_other_than_unknown_command_is_never_retried() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let herdr_socket = root.path().join("herdr.sock");
    let requests = rejecting_daemon(&derived_socket(&runtime));

    let mut session = Session::start(&[
        ("HERDR_SOCKET_PATH", herdr_socket.to_str().unwrap()),
        ("HERDR_PANE_ID", "w1:p1"),
        ("XDG_RUNTIME_DIR", runtime.to_str().unwrap()),
    ]);
    // unknown command 以外のエラーで旧名 fallback すると send が二重配送になり得る。
    let sent = session.call(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "send_message", "arguments": {"to": "codex", "body": "本文"}}
    }));
    assert_eq!(sent["result"]["isError"], true, "{sent}");
    let first = requests.recv().unwrap();
    assert_eq!(first["command"], "send-message");
    assert!(
        requests.try_recv().is_err(),
        "unknown 以外のエラーでは再試行しない (二重配送の禁止)"
    );
}

#[test]
fn daemon_errors_surface_as_tool_errors_and_unknown_methods_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let herdr_socket = root.path().join("herdr.sock");
    let _requests = fake_daemon(&derived_socket(&runtime));

    let mut session = Session::start(&[
        ("HERDR_SOCKET_PATH", herdr_socket.to_str().unwrap()),
        ("HERDR_PANE_ID", "w1:p1"),
        ("XDG_RUNTIME_DIR", runtime.to_str().unwrap()),
    ]);
    let unknown_method = session.call(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "resources/read", "params": {}
    }));
    assert_eq!(unknown_method["error"]["code"], -32601);

    let unknown_tool = session.call(&json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "run_shell", "arguments": {"cmd": "id"}}
    }));
    assert_eq!(unknown_tool["error"]["code"], -32602);
}

#[test]
fn the_mcp_sources_carry_no_subprocess_tcp_or_filesystem_capability() {
    let sources = [
        "src/bin/agent-talk-mcp.rs",
        "src/mcp.rs",
        "src/paths.rs",
        "src/protocol.rs",
    ];
    for source in sources {
        let text = std::fs::read_to_string(source).unwrap();
        let production: String = text
            .split("#[cfg(test)]")
            .next()
            .expect("source has a production section")
            .to_owned();
        for forbidden in [
            "Command",
            "TcpListener",
            "TcpStream",
            "std::fs",
            "fs::",
            "File::",
            "tokio::process",
        ] {
            assert!(
                !production.contains(forbidden),
                "{source} must not use {forbidden}"
            );
        }
    }
}

/// 片欠け env (`HERDR_PANE_ID` のみ) では pane を自己申告しない。
///
/// pane id は herdr session 間で衝突しうるため、socket で帰属を示せない申告は
/// 既定 session の同名 pane へ誤 bind しうる — adapter は既定 socket へ接続し、
/// identity は daemon の peer PID 解決 (socket 一致まで検証) に委ねる。
#[test]
fn a_pane_id_without_its_socket_is_never_self_reported() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let requests = fake_daemon(&derived_socket(&runtime));

    let mut session = Session::start(&[
        ("HERDR_PANE_ID", "w9:p9"),
        ("XDG_RUNTIME_DIR", runtime.to_str().unwrap()),
    ]);
    let peers = session.call(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "list_peers", "arguments": {}}
    }));
    assert_eq!(peers["result"]["isError"], false, "{peers}");
    let request = requests.recv().unwrap();
    assert_eq!(request["command"], "list-peers");
    assert_eq!(
        request["pane"],
        Value::Null,
        "socket 無しの HERDR_PANE_ID を申告してはならない: {request}"
    );
}
