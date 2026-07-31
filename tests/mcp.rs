//! `agent-talk-mcp` の process 境界テスト。
//!
//! 実 tmux を必要としない検証をここに置く: 起動時 contract の fail closed、
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
/// `XDG_RUNTIME_DIR` + `TMUX` から導出される。テストもその規則だけに従う。
fn derived_socket(runtime: &Path, tmux_socket: &str) -> PathBuf {
    runtime.join("agent-talkd").join(format!(
        "{}.sock",
        Path::new(tmux_socket)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
    ))
}

fn mcp(env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-talk-mcp"));
    command
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("HOME")
        .env_remove("AGENT_TALK_RPC_SOCKET")
        .env_remove("AGENT_TALK_TMUX_SOCKET");
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

/// 受け取った `Request` を記録しつつ、command ごとに定型の `Response` を返す fake daemon。
fn fake_daemon(socket: &Path) -> mpsc::Receiver<Value> {
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
            let response = match request["command"].as_str().unwrap() {
                "peers-v1" => json!({
                    "code": 0,
                    "stdout": "{\"version\":1,\"self\":\"%1\",\"pending_to_me\":[3],\"peers\":[{\"name\":\"codex\",\"state\":\"idle\",\"location\":\"work:0.0\",\"pane\":\"%2\",\"cwd\":\"/tmp\",\"queued\":0,\"pending_from_me\":[9]}]}\n",
                    "stderr": "",
                }),
                "send-message-v1" => json!({
                    "code": 0,
                    "stdout": "{\"version\":1,\"id\":9,\"path\":\"sent\",\"to\":\"%2\",\"name\":\"codex\"}\n",
                    "stderr": "",
                }),
                "read-v1" => json!({
                    "code": 0,
                    "stdout": "{\"version\":1,\"id\":3,\"from\":\"codex\",\"reply_to\":\"%2\",\"body\":\"# agent-talk 依頼書\\n本文\"}\n",
                    "stderr": "",
                }),
                "ack-v1" => json!({
                    "code": 0,
                    "stdout": "{\"version\":1,\"id\":3,\"outcome\":\"acked\"}\n",
                    "stderr": "",
                }),
                other => json!({
                    "code": 1,
                    "stdout": "",
                    "stderr": format!("agent-talk: unknown command {other}\n"),
                }),
            };
            let mut stream = stream;
            let mut encoded = serde_json::to_vec(&response).unwrap();
            encoded.push(b'\n');
            stream.write_all(&encoded).unwrap();
            stream.flush().unwrap();
            tx.send(request).unwrap();
        }
    });
    rx
}

/// 契約外の「成功」応答だけを返す fake daemon。
/// MCP は暗黙に劣化させず、すべて `isError: true` にしなければならない。
fn degraded_daemon(socket: &Path) {
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(socket).unwrap();
    thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            let stdout = match request["command"].as_str().unwrap() {
                // 旧テキスト形式 (versioned JSON への移行前の形)
                "send-message-v1" => "sent -> %2 (codex): #9\n",
                // version フィールドが無い
                "read-v1" => "{\"id\":3,\"from\":\"codex\"}\n",
                // object ではない
                "ack-v1" => "[1,2,3]\n",
                // 空応答
                _ => "\n",
            };
            let response = json!({ "code": 0, "stdout": stdout, "stderr": "" });
            let mut stream = stream;
            let mut encoded = serde_json::to_vec(&response).unwrap();
            encoded.push(b'\n');
            stream.write_all(&encoded).unwrap();
            stream.flush().unwrap();
        }
    });
}

#[test]
fn a_success_response_outside_the_contract_never_degrades_into_a_tool_success() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let tmux_socket = root.path().join("tmux/degraded");
    let tmux_socket = tmux_socket.to_str().unwrap();
    degraded_daemon(&derived_socket(&runtime, tmux_socket));

    let mut session = Session::start(&[
        ("TMUX", &format!("{tmux_socket},1,0")),
        ("TMUX_PANE", "%1"),
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
fn missing_or_malformed_startup_inputs_fail_closed_without_serving_tools() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let runtime = runtime.to_str().unwrap();
    let cases: Vec<Vec<(&str, &str)>> = vec![
        // TMUX 未 forward
        vec![("TMUX_PANE", "%1"), ("XDG_RUNTIME_DIR", runtime)],
        // TMUX_PANE 未 forward
        vec![
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("XDG_RUNTIME_DIR", runtime),
        ],
        // TMUX が空 / 書式違反
        vec![
            ("TMUX", ""),
            ("TMUX_PANE", "%1"),
            ("XDG_RUNTIME_DIR", runtime),
        ],
        vec![
            ("TMUX", "/tmp/tmux-1000/default"),
            ("TMUX_PANE", "%1"),
            ("XDG_RUNTIME_DIR", runtime),
        ],
        vec![
            ("TMUX", "relative,1,0"),
            ("TMUX_PANE", "%1"),
            ("XDG_RUNTIME_DIR", runtime),
        ],
        // TMUX_PANE が空 / 書式違反
        vec![
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("TMUX_PANE", ""),
            ("XDG_RUNTIME_DIR", runtime),
        ],
        vec![
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("TMUX_PANE", "pane-1"),
            ("XDG_RUNTIME_DIR", runtime),
        ],
        // TMUX に余分なフィールドがある (文法はちょうど3つ)
        vec![
            ("TMUX", "/tmp/tmux-1000/default,1,0,junk"),
            ("TMUX_PANE", "%1"),
            ("XDG_RUNTIME_DIR", runtime),
        ],
        // XDG_RUNTIME_DIR が不正 (相対 path) — HOME があっても fallback しない
        vec![
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("TMUX_PANE", "%1"),
            ("XDG_RUNTIME_DIR", "relative/run"),
            ("HOME", "/home/tester"),
        ],
        // XDG_RUNTIME_DIR も HOME も無い
        vec![("TMUX", "/tmp/tmux-1000/default,1,0"), ("TMUX_PANE", "%1")],
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
}

#[test]
fn home_fallback_reaches_the_same_socket_layout_as_the_daemon() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let runtime = home.join(".cache/agent-talkd/run");
    let tmux_socket = root.path().join("tmux/fallback");
    let tmux_socket = tmux_socket.to_str().unwrap();
    let requests = fake_daemon(&derived_socket(&runtime, tmux_socket));

    let mut session = Session::start(&[
        ("TMUX", &format!("{tmux_socket},1,0")),
        ("TMUX_PANE", "%7"),
        ("HOME", home.to_str().unwrap()),
    ]);
    let response = session.call(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "list_peers", "arguments": {}}
    }));
    assert_eq!(response["result"]["isError"], false, "{response}");
    let request = requests.recv().unwrap();
    assert_eq!(request["command"], "peers-v1");
    assert_eq!(request["pane"], "%7");
}

#[test]
#[allow(clippy::too_many_lines)]
fn the_four_tools_round_trip_over_the_derived_unix_socket() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let tmux_socket = root.path().join("tmux/round-trip");
    let tmux_socket = tmux_socket.to_str().unwrap();
    let requests = fake_daemon(&derived_socket(&runtime, tmux_socket));

    let mut session = Session::start(&[
        ("TMUX", &format!("{tmux_socket},4242,0")),
        ("TMUX_PANE", "%1"),
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
    assert_eq!(read["result"]["structuredContent"]["reply_to"], "%2");

    let acked = session.call(&json!({
        "jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": {"name": "ack_message", "arguments": {"id": 3}}
    }));
    assert_eq!(acked["result"]["structuredContent"]["outcome"], "acked");

    let observed: Vec<Value> = (0..4).map(|_| requests.recv().unwrap()).collect();
    assert_eq!(observed[0]["command"], "peers-v1");
    // 呼び出し元 identity は adapter が spawn 時の TMUX_PANE から導出する。
    for request in &observed {
        assert_eq!(request["pane"], "%1");
    }
    assert_eq!(observed[1]["command"], "send-message-v1");
    assert_eq!(observed[1]["args"], json!(["codex"]));
    assert_eq!(observed[1]["stdin"], "本文\n2行目");
    assert_eq!(observed[1]["send_options"]["no_reply"], true);
    assert_eq!(observed[1]["send_options"]["skill"], Value::Null);
    assert_eq!(observed[1]["send_options"]["from"], Value::Null);
    assert_eq!(observed[2]["command"], "read-v1");
    assert_eq!(observed[2]["args"], json!(["3"]));
    assert_eq!(observed[3]["command"], "ack-v1");
}

#[test]
fn daemon_errors_surface_as_tool_errors_and_unknown_methods_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let tmux_socket = root.path().join("tmux/errors");
    let tmux_socket = tmux_socket.to_str().unwrap();
    let _requests = fake_daemon(&derived_socket(&runtime, tmux_socket));

    let mut session = Session::start(&[
        ("TMUX", &format!("{tmux_socket},1,0")),
        ("TMUX_PANE", "%1"),
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
