//! Exercise the shipped HTTP binary against isolated Herdr and transcript fixtures.
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::net::UnixListener,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request(port: u16, method: &str, path: &str, body: Option<&Value>, extra: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let body = body.map_or_else(String::new, Value::to_string);
    write!(stream,"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

#[test]
#[allow(clippy::too_many_lines)] // One end-to-end lifecycle, with shared isolated sockets.
fn browser_api_routes_original_text_to_the_verified_session_and_reads_native_output() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let row = Arc::new(Mutex::new(
        json!({"pane_id":"w1:p2","terminal_id":"terminal-1","workspace_id":"w1","agent":"codex","agent_status":"working","agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"session-a"}}),
    ));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let server_row = row.clone();
    let received = prompts.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                break;
            };
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let row = server_row.lock().unwrap().clone();
            let result = match request["method"].as_str().unwrap() {
                "agent.list" => json!({"agents":[row]}),
                "workspace.list" => json!({"workspaces":[]}),
                "tab.list" => json!({"tabs":[]}),
                "agent.get" => json!({"agent":row}),
                "pane.process_info" => {
                    json!({"type":"pane_process_info","process_info":{"pane_id":"w1:p2","foreground_processes":[{"pid":123,"name":"codex"}]}})
                }
                "agent.prompt" => {
                    received.lock().unwrap().push(request["params"].clone());
                    json!({"type":"agent_prompted","agent":row})
                }
                method => panic!("unexpected Herdr operation {method}"),
            };
            writeln!(stream, "{}", json!({"id":request["id"],"result":result})).unwrap();
        }
    });
    let history_dir = directory.path().join(".codex/sessions");
    std::fs::create_dir_all(&history_dir).unwrap();
    std::fs::write(history_dir.join("rollout-session-a.jsonl"),concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-a\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"確認\"}]}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"実セッションの報告\"}]}}\n"
    )).unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _process = Process(
        Command::new(env!("CARGO_BIN_EXE_agent-talk"))
            .arg("daemon")
            .env("HOME", directory.path())
            .env("AGENT_TALK_HERDR_SOCKET", &socket)
            .env("AGENT_TALK_HTTP_ADDR", format!("127.0.0.1:{port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    for attempt in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(attempt < 99, "daemon did not start");
        thread::sleep(Duration::from_millis(20));
    }
    let (status, agents) = request(port, "GET", "/api/agents", None, "");
    assert_eq!(status, 200);
    let token = agents["agents"][0]["session_id"].as_str().unwrap();
    let body = json!({"pane_id":"w1:p2","session_id":token,"body":"  原文\n$(literal) `本文`  "});
    assert_eq!(
        request(port, "POST", "/api/messages", Some(&body), "").1,
        json!({"status":"submitted"})
    );
    assert_eq!(
        *prompts.lock().unwrap(),
        vec![json!({"target":"w1:p2","text":body["body"]})]
    );
    let path = format!("/api/conversation?pane=w1%3Ap2&session={token}");
    let (status, conversation) = request(port, "GET", &path, None, "");
    assert_eq!(status, 200);
    assert_eq!(conversation["messages"][1]["text"], "実セッションの報告");
    assert_eq!(
        request(
            port,
            "POST",
            "/api/messages",
            Some(&body),
            "Origin: https://unrelated.example\r\n"
        )
        .0,
        403
    );
    row.lock().unwrap()["agent_status"] = json!("blocked");
    assert_eq!(
        request(port, "POST", "/api/messages", Some(&body), "").1["error"]["code"],
        "blocked"
    );
    row.lock().unwrap()["agent_session"]["value"] = json!("session-b");
    assert_eq!(
        request(port, "POST", "/api/messages", Some(&body), "").1["error"]["code"],
        "session_changed"
    );
    assert_eq!(request(port, "GET", &path, None, "").0, 409);
    assert_eq!(prompts.lock().unwrap().len(), 1);
    for path in [
        "/api/letters",
        "/api/mailboxes",
        "/api/who",
        "/api/list-peers",
        "/api/agents/w1:p2/screen",
    ] {
        assert_eq!(request(port, "GET", path, None, "").0, 404);
    }
    assert_eq!(
        request(port, "GET", "/api/conversation?pane=%", None, "").0,
        400
    );
    assert_eq!(request(port, "GET", "/api/hello", None, "").0, 200);
}

#[test]
fn removed_peer_and_lifecycle_commands_fail_explicitly() {
    for command in [
        "send",
        "reply",
        "list-peers",
        "read",
        "ack",
        "ensure-daemon",
        "run",
    ] {
        assert!(
            !Command::new(env!("CARGO_BIN_EXE_agent-talk"))
                .arg(command)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }
}
