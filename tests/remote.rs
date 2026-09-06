//! Exercise the shipped HTTP binary against isolated Herdr and transcript fixtures.
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::ffi::OsStringExt,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

mod support;

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
#[allow(clippy::too_many_lines)] // One end-to-end lifecycle, with shared isolated CLI state.
fn browser_api_routes_original_text_to_the_verified_session_and_reads_native_output() {
    let directory = tempfile::tempdir().unwrap();
    let row = json!({"pane_id":"w1:p2","terminal_id":"terminal-1","workspace_id":"w1","agent":"codex","agent_status":"working","agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"session-a"}});
    let fixture = support::CliFixture::new(&row);
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
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(fixture.directory.path().to_path_buf())
                        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .env_remove("AGENT_TALK_HERDR_SOCKET")
            .env_remove("HERDR_SOCKET_PATH")
            .env("PORT", port.to_string())
            .env("AGENT_TALK_HTTP_ADDR", "invalid-legacy-address")
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
        fixture.prompts(),
        vec![json!({"target":"w1:p2","text":body["body"]})]
    );
    let path = format!("/api/conversation?pane=w1%3Ap2&session={token}");
    let (status, conversation) = request(port, "GET", &path, None, "");
    assert_eq!(status, 200);
    assert_eq!(conversation["messages"][1]["text"], "実セッションの報告");
    assert_eq!(conversation["older_cursor"], Value::Null);
    assert_eq!(conversation["has_more"], false);
    let cursor = conversation["next_cursor"].as_str().unwrap();
    let newer_path = format!("{path}&after={cursor}");
    assert!(
        request(port, "GET", &newer_path, None, "").1["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(history_dir.join("rollout-session-a.jsonl"))
        .unwrap();
    for i in 0..503 {
        writeln!(transcript, "{}", json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":format!("追加{i}")}]}})).unwrap();
    }
    let (status, added) = request(port, "GET", &newer_path, None, "");
    assert_eq!(status, 200);
    assert_eq!(added["messages"].as_array().unwrap().len(), 500);
    assert_eq!(added["messages"][0]["text"], "追加0");
    assert_eq!(added["has_more"], true);
    let next = added["next_cursor"].as_str().unwrap();
    let tail = request(port, "GET", &format!("{path}&after={next}"), None, "").1;
    assert_eq!(tail["messages"].as_array().unwrap().len(), 3);
    assert_eq!(tail["has_more"], false);
    let before = request(port, "GET", &format!("{path}&before={cursor}"), None, "").1;
    assert_eq!(before["messages"].as_array().unwrap().len(), 2);
    for suffix in [
        "&before=x",
        "&after=",
        "&before=x&after=y",
        "&after=x&after=x",
        "&other=x",
        "&pane=another",
    ] {
        assert_eq!(
            request(port, "GET", &format!("{path}{suffix}"), None, "").0,
            400
        );
    }
    std::fs::write(history_dir.join("rollout-session-a.jsonl"), "").unwrap();
    assert_eq!(request(port, "GET", &newer_path, None, "").0, 409);
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
    fixture.update(|state| state["row"]["agent_status"] = json!("blocked"));
    assert_eq!(
        request(port, "POST", "/api/messages", Some(&body), "").1["error"]["code"],
        "blocked"
    );
    let screen_path = format!("/api/screen?pane=w1%3Ap2&terminal=terminal-1&session={token}");
    let (status, screen) = request(port, "GET", &screen_path, None, "");
    assert_eq!(status, 200, "blocked targets remain readable: {screen}");
    assert_eq!(screen["pane_id"], "w1:p2");
    assert_eq!(screen["session_id"], token);
    assert_eq!(screen["terminal_id"], "terminal-1");
    assert_eq!(screen["text"], "確認してください\n[許可] [拒否]");
    assert_eq!(screen["format"], "text");
    assert!(screen["captured_at"].as_u64().unwrap() > 1_700_000_000_000);
    for suffix in ["&pane=w1:p3", "&other=x", "&before=x"] {
        assert_eq!(
            request(port, "GET", &format!("{screen_path}{suffix}"), None, "").0,
            400
        );
    }
    assert_eq!(
        request(port, "GET", "/api/screen?pane=w1:p2", None, "").0,
        400
    );
    let terminal_path = "/api/screen?pane=w1:p2&terminal=terminal-1";
    for (mode, expected) in [
        ("oversized", 503),
        ("restart", 409),
        ("ended", 409),
        ("terminal-restart", 409),
    ] {
        fixture.update(|state| {
            state["row"]["agent_session"] =
                json!({"source":"herdr:codex","agent":"codex","kind":"id","value":"session-a"});
        });
        fixture.update(|state| state["screen_mode"] = json!(mode));
        let (status, failure) = request(port, "GET", terminal_path, None, "");
        assert_eq!(status, expected, "{mode}: {failure}");
        assert!(failure.get("text").is_none());
    }
    fixture.update(|state| {
        state["row"]["agent_session"] =
            json!({"source":"herdr:codex","agent":"codex","kind":"id","value":"session-a"});
    });
    fixture.update(|state| state["screen_mode"] = json!(""));
    fixture.update(|state| state["row"]["terminal_id"] = json!("terminal-1"));
    for harness in ["codex", "bash"] {
        fixture.update(|state| state["row"]["agent"] = json!(harness));
        fixture.update(|state| state["row"]["agent_session"] = Value::Null);
        let (status, screen) = request(port, "GET", terminal_path, None, "");
        assert_eq!(status, 200, "{harness}: {screen}");
        assert_eq!(screen["session_id"], Value::Null);
        assert_eq!(screen["terminal_id"], "terminal-1");
    }
    fixture.update(|state| state["row"]["terminal_id"] = json!("terminal-2"));
    assert_eq!(request(port, "GET", terminal_path, None, "").0, 409);
    fixture.update(|state| state["row"]["terminal_id"] = json!("terminal-1"));
    fixture.update(|state| state["row"]["agent"] = json!("codex"));
    fixture.update(|state| {
        state["row"]["agent_session"] =
            json!({"source":"herdr:codex","agent":"codex","kind":"id","value":"session-a"});
    });
    fixture.update(|state| state["row"]["agent_session"]["value"] = json!("session-b"));
    assert_eq!(
        request(port, "POST", "/api/messages", Some(&body), "").1["error"]["code"],
        "session_changed"
    );
    assert_eq!(request(port, "GET", &path, None, "").0, 409);
    assert_eq!(fixture.prompts().len(), 1);
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

#[test]
fn invalid_port_fails_before_starting_the_daemon() {
    let home = tempfile::tempdir().unwrap();
    for port in ["", "0", "65536", "-1", "+5002", " 5002", "5002 ", "bad"]
        .into_iter()
        .map(OsString::from)
        .chain([OsString::from_vec(vec![0xff])])
    {
        let output = Command::new(env!("CARGO_BIN_EXE_agent-talk"))
            .arg("daemon")
            .env("HOME", home.path())
            .env("PORT", &port)
            .env("AGENT_TALK_HTTP_ADDR", "127.0.0.1:5002")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "PORT={port:?}");
        assert!(stderr.contains("PORT"), "{stderr}");
    }
}
