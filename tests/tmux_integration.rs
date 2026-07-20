use std::{
    fs,
    os::unix::net::UnixListener,
    path::Path,
    process::{Command, Output},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Server {
    name: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.name, "kill-server"])
            .output();
    }
}

#[test]
#[ignore = "requires permission to create a real tmux server"]
fn daemon_queue_delivery_stale_socket_and_pane_exit() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("agent-talk-test-{}-{unique}", std::process::id());
    let _server = Server { name: name.clone() };
    tmux(
        &name,
        &[
            "new-session",
            "-d",
            "-s",
            "test",
            "-n",
            "agents",
            "sleep 30",
        ],
    );
    tmux(
        &name,
        &["split-window", "-d", "-t", "test:agents", "sleep 30"],
    );
    tmux(
        &name,
        &["split-window", "-d", "-t", "test:agents", "sleep 30"],
    );

    let socket = text(tmux(&name, &["display-message", "-p", "#{socket_path}"]));
    tmux(
        &name,
        &["set-option", "-g", "@agent_talkd_queue_limit", "1"],
    );
    let panes: Vec<_> = text(tmux(
        &name,
        &["list-panes", "-t", "test:agents", "-F", "#{pane_id}"],
    ))
    .lines()
    .map(str::to_owned)
    .collect();
    assert_eq!(panes.len(), 3);

    let root = tempfile::tempdir().unwrap();
    let runtime = root.path().join("run");
    let state = root.path().join("state");
    let mail = root.path().join("mail");
    let socket_name = Path::new(&socket).file_name().unwrap();
    let rpc = runtime
        .join("agent-talkd")
        .join(socket_name)
        .with_extension("sock");
    let journal = state
        .join("agent-talkd")
        .join(socket_name)
        .with_extension("journal");
    fs::create_dir_all(rpc.parent().unwrap()).unwrap();
    drop(UnixListener::bind(&rpc).unwrap());

    agent(
        &socket,
        &runtime,
        &state,
        &mail,
        &panes[0],
        &["register", "codex"],
    );
    agent(
        &socket,
        &runtime,
        &state,
        &mail,
        &panes[1],
        &["register", "claude"],
    );
    let human_send = text(agent(
        &socket,
        &runtime,
        &state,
        &mail,
        &panes[2],
        &["send", "claude", "human request"],
    ));
    assert!(human_send.starts_with("sent -> "));
    let human_brief_path = human_send.rsplit_once(": ").unwrap().1;
    let human_brief = fs::read_to_string(human_brief_path).unwrap();
    assert!(human_brief.contains("- from: human (session: test, pane:"));
    assert!(
        human_brief
            .contains("- reply: 不要 (人間からの依頼。結果は自分の画面に表示すれば読まれる)")
    );
    agent(&socket, &runtime, &state, &mail, &panes[1], &["turn-end"]);
    let missing = agent_raw(
        &socket,
        &runtime,
        &state,
        &mail,
        &panes[0],
        &["resolve", "missing"],
    );
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr)
            .contains("agent-talk: 宛先 'missing' がスコープ内に見つかりません。待受中:")
    );
    agent(
        &socket,
        &runtime,
        &state,
        &mail,
        &panes[2],
        &["register", "claude"],
    );
    let ambiguous = agent_raw(
        &socket,
        &runtime,
        &state,
        &mail,
        &panes[0],
        &["resolve", "claude"],
    );
    assert!(!ambiguous.status.success());
    assert!(
        String::from_utf8_lossy(&ambiguous.stderr)
            .contains("agent-talk: 宛先 'claude' の候補が複数あります。")
    );
    agent(&socket, &runtime, &state, &mail, &panes[2], &["unregister"]);
    agent(&socket, &runtime, &state, &mail, &panes[1], &["busy"]);
    let queued = text(agent(
        &socket,
        &runtime,
        &state,
        &mail,
        &panes[0],
        &["send", "claude", "first"],
    ));
    assert!(queued.starts_with("queued (busy) -> "));
    let agent_brief_path = queued.rsplit_once(": ").unwrap().1;
    let agent_brief = fs::read_to_string(agent_brief_path).unwrap();
    assert!(agent_brief.contains("- from: codex (session: test, pane:"));
    assert!(agent_brief.contains("- to: claude"));
    assert!(agent_brief.contains(&format!(
        "- reply: agent-talk send '{}' に返信本文を stdin で渡す",
        panes[0]
    )));
    let overflow = agent_raw(
        &socket,
        &runtime,
        &state,
        &mail,
        &panes[0],
        &["send", "claude", "overflow"],
    );
    assert!(!overflow.status.success());
    assert!(String::from_utf8_lossy(&overflow.stderr).contains("キュー保持上限 (1)"));
    assert_eq!(
        fs::read_to_string(&journal)
            .unwrap()
            .matches("\"type\":\"enqueue\"")
            .count(),
        1
    );

    let owner = Command::new("fuser").arg(&rpc).output().unwrap();
    assert!(owner.status.success(), "fuser could not find daemon");
    let daemon_pid: i32 = String::from_utf8(owner.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(daemon_pid, libc::SIGKILL) }, 0);
    thread::sleep(Duration::from_millis(200));
    let recovered = text(agent(&socket, &runtime, &state, &mail, &panes[0], &["who"]));
    assert!(recovered.contains("claude     busy"));

    agent(&socket, &runtime, &state, &mail, &panes[1], &["turn-end"]);
    agent(&socket, &runtime, &state, &mail, &panes[1], &["busy"]);
    agent(
        &socket,
        &runtime,
        &state,
        &mail,
        &panes[0],
        &["send", "claude", "second"],
    );
    tmux(&name, &["kill-pane", "-t", &panes[1]]);
    thread::sleep(Duration::from_millis(500));

    let who = text(agent(&socket, &runtime, &state, &mail, &panes[0], &["who"]));
    assert!(who.contains("codex"));
    assert!(!who.contains("claude"));
    let sender_state = text(tmux(
        &name,
        &["show-option", "-pqv", "-t", &panes[0], "@agent_state"],
    ));
    assert_eq!(sender_state, "busy");
    let sender_screen = text(tmux(&name, &["capture-pane", "-p", "-t", &panes[0]]));
    assert!(sender_screen.contains("[agent-talk] 配達失敗:"));
}

fn agent(
    socket: &str,
    runtime: &Path,
    state: &Path,
    mail: &Path,
    pane: &str,
    args: &[&str],
) -> Output {
    let output = agent_raw(socket, runtime, state, mail, pane, args);
    assert!(
        output.status.success(),
        "agent-talk failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn agent_raw(
    socket: &str,
    runtime: &Path,
    state: &Path,
    mail: &Path,
    pane: &str,
    args: &[&str],
) -> Output {
    let binary = env!("CARGO_BIN_EXE_agent-talk");
    Command::new(binary)
        .args(args)
        .env("AGENT_TALK_TMUX_SOCKET", socket)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .env("AGENT_TALK_DIR", mail)
        .env("TMUX", format!("{socket},1,0"))
        .env("TMUX_PANE", pane)
        .output()
        .unwrap()
}

fn tmux(name: &str, args: &[&str]) -> Output {
    let output = Command::new("tmux")
        .arg("-L")
        .arg(name)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "tmux failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn text(output: Output) -> String {
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
