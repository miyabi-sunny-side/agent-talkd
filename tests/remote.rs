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
    let body = body.map_or_else(String::new, Value::to_string);
    raw(
        port,
        method,
        path,
        "application/json",
        body.as_bytes(),
        extra,
    )
}

fn raw(
    port: u16,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
    extra: &str,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to daemon");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    write!(stream,"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",body.len()).expect("write request head");
    // The server may answer and close before an oversized body is fully sent.
    let _ = stream.write_all(body);
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read UTF-8 response");
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .expect("response has a header terminator");
    let status = headers
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("status line has a numeric code: {headers}"));
    for expected in [
        "content-type: application/json; charset=utf-8",
        "cache-control: no-store",
        "x-content-type-options: nosniff",
        "referrer-policy: same-origin",
    ] {
        assert!(
            headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case(expected)),
            "{method} {path} lacks `{expected}`: {headers}"
        );
    }
    (
        status,
        serde_json::from_str(body)
            .unwrap_or_else(|error| panic!("{method} {path} body is not JSON ({error}): {body}")),
    )
}

fn start(fixture: &support::CliFixture, home: &std::path::Path) -> (Process, u16) {
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("reserve a free local port")
        .port();
    let process = Process(
        Command::new(env!("CARGO_BIN_EXE_agent-talk"))
            .arg("daemon")
            .env("HOME", home)
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(fixture.directory.path().to_path_buf()).chain(
                        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
                    ),
                )
                .expect("PATH entries contain no separator"),
            )
            .env_remove("AGENT_TALK_HERDR_SOCKET")
            .env_remove("HERDR_SOCKET_PATH")
            .env("PORT", port.to_string())
            .env("AGENT_TALK_HTTP_ADDR", "invalid-legacy-address")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn agent-talk daemon"),
    );
    for attempt in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(attempt < 99, "daemon did not start");
        thread::sleep(Duration::from_millis(20));
    }
    (process, port)
}

#[test]
fn browser_api_routes_original_text_to_the_verified_session_and_reads_native_output() {
    for pane in ["w1:p2", "w2N:p3", "w2J:p9", "w2E:p6", "w10:p2N", "w2N:p3J"] {
        verified_session_lifecycle(pane);
    }
}

#[allow(clippy::too_many_lines)] // One end-to-end lifecycle, with shared isolated CLI state.
fn verified_session_lifecycle(pane: &str) {
    let workspace = pane.split_once(':').expect("pane ids are workspace:pane").0;
    let encoded_pane = pane.replace(':', "%3A");
    let directory = tempfile::tempdir().expect("create HOME directory");
    let row = json!({"pane_id":pane,"terminal_id":"terminal-1","workspace_id":workspace,"agent":"codex","agent_status":"working","agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"session-a"}});
    let fixture = support::CliFixture::new(&row);
    let mut numeric_row = row.clone();
    numeric_row["pane_id"] = json!("w10:p1");
    numeric_row["workspace_id"] = json!("w10");
    fixture.update(|state| state["extra_rows"] = json!([numeric_row]));
    let history_dir = directory.path().join(".codex/sessions");
    std::fs::create_dir_all(&history_dir).expect("create Codex history directory");
    std::fs::write(history_dir.join("rollout-session-a.jsonl"),concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session-a\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"確認\"}]}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"実セッションの報告\"}]}}\n"
    )).expect("write Codex transcript");
    let (_process, port) = start(&fixture, directory.path());
    let (status, agents) = request(port, "GET", "/api/agents", None, "");
    assert_eq!(status, 200, "{pane}: {agents}");
    assert_eq!(
        agents["agents"].as_array().map(Vec::len),
        Some(2),
        "{pane}: both panes are listed: {agents}"
    );
    assert_eq!(agents["agents"][0]["pane_id"], pane, "{pane}: first agent");
    assert_eq!(
        agents["agents"][1]["pane_id"], "w10:p1",
        "{pane}: numeric workspace is ordered after"
    );
    let token = agents["agents"][0]["session_id"]
        .as_str()
        .expect("listed agent has a session token");
    let body = json!({"pane_id":pane,"session_id":token,"body":"  原文\n$(literal) `本文`  "});
    assert_eq!(
        request(port, "POST", "/api/messages", Some(&body), "").1,
        json!({"status":"submitted"}),
        "{pane}: message is submitted"
    );
    assert_eq!(
        fixture.prompts(),
        vec![json!({"target":pane,"text":body["body"]})],
        "{pane}: original text reaches the verified pane"
    );
    let path = format!("/api/conversation?pane={encoded_pane}&session={token}");
    let (status, conversation) = request(port, "GET", &path, None, "");
    assert_eq!(status, 200, "{pane}: {conversation}");
    assert_eq!(
        conversation["messages"][1]["text"], "実セッションの報告",
        "{pane}: conversation[\"messages\"][1][\"text\"]"
    );
    assert_eq!(
        conversation["pane_id"], pane,
        "{pane}: conversation[\"pane_id\"]"
    );
    assert_eq!(
        conversation["session_id"], token,
        "{pane}: conversation[\"session_id\"]"
    );
    assert_eq!(
        conversation["messages"][0]["role"], "user",
        "{pane}: conversation[\"messages\"][0][\"role\"]"
    );
    assert_eq!(
        conversation["messages"][1]["role"], "assistant",
        "{pane}: conversation[\"messages\"][1][\"role\"]"
    );
    assert!(
        conversation["messages"][0].get("timestamp").is_none(),
        "{pane}: transcript without timestamps omits them"
    );
    assert_eq!(
        conversation["truncated"], false,
        "{pane}: conversation[\"truncated\"]"
    );
    assert_eq!(
        conversation["pending_tail"], false,
        "{pane}: conversation[\"pending_tail\"]"
    );
    assert_eq!(
        conversation["older_cursor"],
        Value::Null,
        "{pane}: conversation[\"older_cursor\"]"
    );
    assert_eq!(
        conversation["has_more"], false,
        "{pane}: conversation[\"has_more\"]"
    );
    let cursor = conversation["next_cursor"]
        .as_str()
        .expect("conversation has a next cursor");
    let newer_path = format!("{path}&after={cursor}");
    assert!(
        request(port, "GET", &newer_path, None, "").1["messages"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{pane}: no messages after the latest cursor"
    );
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(history_dir.join("rollout-session-a.jsonl"))
        .expect("open Codex transcript");
    for i in 0..503 {
        writeln!(transcript, "{}", json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":format!("追加{i}")}]}})).expect("append Codex transcript");
    }
    let (status, added) = request(port, "GET", &newer_path, None, "");
    assert_eq!(status, 200, "{pane}: {added}");
    assert_eq!(
        added["messages"].as_array().map(Vec::len),
        Some(500),
        "{pane}: newer page is capped"
    );
    assert_eq!(
        added["messages"][0]["text"], "追加0",
        "{pane}: newer page order"
    );
    assert_eq!(added["has_more"], true, "{pane}: capped page has more");
    let next = added["next_cursor"]
        .as_str()
        .expect("capped page has a next cursor");
    let tail = request(port, "GET", &format!("{path}&after={next}"), None, "").1;
    assert_eq!(
        tail["messages"].as_array().map(Vec::len),
        Some(3),
        "{pane}: tail page: {tail}"
    );
    assert_eq!(tail["has_more"], false, "{pane}: tail page is last");
    let before = request(port, "GET", &format!("{path}&before={cursor}"), None, "").1;
    assert_eq!(
        before["messages"].as_array().map(Vec::len),
        Some(2),
        "{pane}: older page: {before}"
    );
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
            400,
            "{pane}: conversation query {suffix} is rejected"
        );
    }
    std::fs::write(history_dir.join("rollout-session-a.jsonl"), "")
        .expect("truncate Codex transcript");
    assert_eq!(
        request(port, "GET", &newer_path, None, "").0,
        409,
        "{pane}: truncated transcript invalidates the cursor"
    );
    assert_eq!(
        request(
            port,
            "POST",
            "/api/messages",
            Some(&body),
            "Origin: https://unrelated.example\r\n"
        )
        .0,
        403,
        "{pane}: cross-origin POST is rejected"
    );
    fixture.update(|state| state["row"]["agent_status"] = json!("blocked"));
    assert_eq!(
        request(port, "POST", "/api/messages", Some(&body), "").1["error"]["code"],
        "blocked",
        "{pane}: blocked agent receives no input"
    );
    let screen_path =
        format!("/api/screen?pane={encoded_pane}&terminal=terminal-1&session={token}");
    let (status, screen) = request(port, "GET", &screen_path, None, "");
    assert_eq!(status, 200, "blocked targets remain readable: {screen}");
    assert_eq!(screen["pane_id"], pane, "{pane}: screen[\"pane_id\"]");
    assert_eq!(
        screen["session_id"], token,
        "{pane}: screen[\"session_id\"]"
    );
    assert_eq!(
        screen["terminal_id"], "terminal-1",
        "{pane}: screen[\"terminal_id\"]"
    );
    assert_eq!(
        screen["text"], "確認してください\n[許可] [拒否]",
        "{pane}: screen[\"text\"]"
    );
    assert_eq!(screen["format"], "text", "{pane}: screen[\"format\"]");
    assert!(
        screen["captured_at"]
            .as_u64()
            .is_some_and(|at| at > 1_700_000_000_000),
        "{pane}: screen has a millisecond capture time: {screen}"
    );
    for suffix in ["&pane=w1:p3", "&other=x", "&before=x"] {
        assert_eq!(
            request(port, "GET", &format!("{screen_path}{suffix}"), None, "").0,
            400,
            "{pane}: screen query {suffix} is rejected"
        );
    }
    assert_eq!(
        request(port, "GET", "/api/screen?pane=w1:p2", None, "").0,
        400,
        "screen requires a terminal id"
    );
    let terminal_path = &format!("/api/screen?pane={encoded_pane}&terminal=terminal-1");
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
        assert!(
            failure.get("text").is_none(),
            "{mode}: failed screen read returns no text"
        );
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
        assert_eq!(
            screen["session_id"],
            Value::Null,
            "{harness}: unregistered session has no token"
        );
        assert_eq!(
            screen["terminal_id"], "terminal-1",
            "{harness}: screen keeps terminal identity"
        );
    }
    fixture.update(|state| state["row"]["terminal_id"] = json!("terminal-2"));
    assert_eq!(
        request(port, "GET", terminal_path, None, "").0,
        409,
        "{pane}: terminal change is a conflict"
    );
    fixture.update(|state| state["row"]["terminal_id"] = json!("terminal-1"));
    fixture.update(|state| state["row"]["agent"] = json!("codex"));
    fixture.update(|state| {
        state["row"]["agent_session"] =
            json!({"source":"herdr:codex","agent":"codex","kind":"id","value":"session-a"});
    });
    fixture.update(|state| state["row"]["agent_session"]["value"] = json!("session-b"));
    assert_eq!(
        request(port, "POST", "/api/messages", Some(&body), "").1["error"]["code"],
        "session_changed",
        "{pane}: changed session receives no input"
    );
    assert_eq!(
        request(port, "GET", &path, None, "").0,
        409,
        "{pane}: conversation of a changed session is a conflict"
    );
    assert_eq!(
        fixture.prompts().len(),
        1,
        "{pane}: only one prompt was sent"
    );
    for path in [
        "/api/letters",
        "/api/mailboxes",
        "/api/who",
        "/api/list-peers",
        "/api/agents/w1:p2/screen",
    ] {
        assert_eq!(
            request(port, "GET", path, None, "").0,
            404,
            "removed route {path} stays absent"
        );
    }
    assert_eq!(
        request(port, "GET", "/api/conversation?pane=%", None, "").0,
        400,
        "malformed percent-encoding is rejected"
    );
    assert_eq!(
        request(port, "GET", "/api/hello", None, "").0,
        200,
        "daemon survives malformed requests"
    );
}

#[test]
fn images_are_stored_briefly_for_the_verified_session_only() {
    let home = tempfile::tempdir().unwrap();
    let row = json!({"pane_id":"w1:p2","terminal_id":"terminal-1","workspace_id":"w1","agent":"codex","agent_status":"idle","agent_session":{"source":"herdr:codex","agent":"codex","kind":"id","value":"session-a"}});
    let fixture = support::CliFixture::new(&row);
    let (_process, port) = start(&fixture, home.path());
    let agents = request(port, "GET", "/api/agents", None, "").1;
    let token = agents["agents"][0]["session_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/api/images?pane=w1%3Ap2&session={token}");
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-image";

    let (status, stored) = raw(port, "POST", &path, "image/png", png, "");
    assert_eq!(status, 200, "{stored}");
    let file = std::path::PathBuf::from(stored["path"].as_str().unwrap());
    let directory = home
        .path()
        .join(".cache/agent-talk/images")
        .canonicalize()
        .unwrap();
    assert!(file.is_absolute());
    assert_eq!(file.parent().unwrap(), directory);
    assert_eq!(file.extension().unwrap(), "png");
    assert_eq!(std::fs::read(&file).unwrap(), png);
    assert!(fixture.prompts().is_empty(), "uploading never sends input");

    for (content_type, body, extra, status, code) in [
        (
            "image/heic",
            &b"\0\0\0\x18ftypheic"[..],
            "",
            415,
            "unsupported_image",
        ),
        ("image/png", b"not an image", "", 415, "unsupported_image"),
        ("image/jpeg", b"%PDF-1.7", "", 415, "unsupported_image"),
        ("text/plain", png, "", 415, "unsupported_image"),
        (
            "image/png",
            png,
            "Origin: https://unrelated.example\r\n",
            403,
            "cross_origin",
        ),
        (
            "image/png",
            png,
            "Sec-Fetch-Site: cross-site\r\n",
            403,
            "cross_origin",
        ),
    ] {
        let (actual, failure) = raw(port, "POST", &path, content_type, body, extra);
        assert_eq!(actual, status, "{content_type}: {failure}");
        assert_eq!(failure["error"]["code"], code);
    }
    let oversized = vec![0_u8; 20 * 1024 * 1024 + 1];
    let (status, failure) = raw(port, "POST", &path, "image/png", &oversized, "");
    assert_eq!(status, 413, "{failure}");
    for target in [
        "/api/images",
        "/api/images?pane=w1%3Ap2",
        "/api/images?pane=w1%3Ap2&session=other",
        "/api/images?pane=w1%3Ap2&session=x&path=/tmp/x",
    ] {
        let (status, failure) = raw(port, "POST", target, "image/png", png, "");
        assert!(
            matches!(status, 400 | 404 | 409),
            "{target}: {status} {failure}"
        );
    }
    fixture.update(|state| state["row"]["agent_status"] = json!("blocked"));
    assert_eq!(raw(port, "POST", &path, "image/png", png, "").0, 200);
    let files = std::fs::read_dir(&directory).unwrap().count();
    assert_eq!(files, 2, "rejected uploads leave no files");
    assert!(fixture.prompts().is_empty());
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
