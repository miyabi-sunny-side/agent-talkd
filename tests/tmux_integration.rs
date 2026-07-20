use std::{
    fs,
    io::Write,
    os::unix::net::UnixListener,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
fn daemon_journal_read_recovery_and_pane_exit() {
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
    let legacy_mail = root.path().join("mail");
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

    // A stale shell-era busy mirror must be adopted as idle on daemon startup.
    tmux(
        &name,
        &["set-option", "-p", "-t", &panes[1], "@agent", "claude"],
    );
    tmux(
        &name,
        &["set-option", "-p", "-t", &panes[1], "@agent_state", "busy"],
    );
    let startup = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["who"],
    ));
    assert!(startup.contains("claude     idle"));
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["register", "codex"],
    );

    // Immediate messages are journal-backed and read is non-destructive.
    let human_send = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["send", "claude", "human request"],
    ));
    assert!(human_send.starts_with("sent -> "));
    let human_id = message_id(&human_send);
    let wrong_recipient = agent_raw(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["read", &human_id.to_string()],
    );
    assert!(!wrong_recipient.status.success());
    assert!(String::from_utf8_lossy(&wrong_recipient.stderr).contains("このpane宛ではありません"));
    let human_brief = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["read", &human_id.to_string()],
    ));
    assert!(human_brief.contains("- from: human (session: test, pane:"));
    assert!(
        human_brief
            .contains("- reply: 不要 (人間からの依頼。結果は自分の画面に表示すれば読まれる)")
    );
    let reread = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["read", &format!("#{human_id}")],
    ));
    assert_eq!(reread, human_brief);
    assert!(
        !legacy_mail.exists(),
        "AGENT_TALK_DIR must no longer receive Markdown files"
    );
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["turn-end"],
    );
    let oversized = agent_raw_stdin(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["send", "claude"],
        &vec![b'x'; 1024 * 1024 + 1],
    );
    assert!(!oversized.status.success());
    assert!(String::from_utf8_lossy(&oversized.stderr).contains("サイズ上限"));

    let missing = agent_raw(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
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
        &legacy_mail,
        &panes[2],
        &["register", "claude"],
    );
    let ambiguous = agent_raw(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["resolve", "claude"],
    );
    assert!(!ambiguous.status.success());
    assert!(
        String::from_utf8_lossy(&ambiguous.stderr)
            .contains("agent-talk: 宛先 'claude' の候補が複数あります。")
    );
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["unregister"],
    );

    // Queued content survives SIGKILL and can be read after restart.
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["busy"],
    );
    let queued = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["send", "claude", "first durable body"],
    ));
    assert!(queued.starts_with("queued (busy) -> "));
    let queued_id = message_id(&queued);
    let overflow = agent_raw(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["send", "claude", "overflow"],
    );
    assert!(!overflow.status.success());
    assert!(String::from_utf8_lossy(&overflow.stderr).contains("キュー保持上限 (1)"));
    assert!(
        fs::read_to_string(&journal)
            .unwrap()
            .contains("first durable body")
    );

    kill_daemon(&rpc);
    thread::sleep(Duration::from_millis(200));
    let recovered = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["who"],
    ));
    assert!(recovered.contains("claude     busy"));
    let recovered_brief = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["read", &queued_id.to_string()],
    ));
    assert!(recovered_brief.contains("first durable body"));
    let recovered_reread = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["read", &queued_id.to_string()],
    ));
    assert_eq!(recovered_reread, recovered_brief);
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["turn-end"],
    );

    // An unread message to an exited pane becomes a readable failure notice.
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["busy"],
    );
    let second = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["send", "claude", "second unread body"],
    ));
    assert!(second.starts_with("queued (busy) -> "));
    let second_id = message_id(&second);
    tmux(&name, &["kill-pane", "-t", &panes[1]]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let failure_id = loop {
        let who = text(agent(
            &socket,
            &runtime,
            &state,
            &legacy_mail,
            &panes[0],
            &["who"],
        ));
        let sender_state = text(tmux(
            &name,
            &["show-option", "-pqv", "-t", &panes[0], "@agent_state"],
        ));
        let sender_screen = text(tmux(&name, &["capture-pane", "-p", "-t", &panes[0]]));
        if who.contains("codex")
            && !who.contains("claude")
            && sender_state == "busy"
            && sender_screen.contains("[agent-talk] 配達失敗:")
        {
            break read_id_from_bell(&sender_screen);
        }
        assert!(
            Instant::now() < deadline,
            "failure notification timed out: who={who:?} state={sender_state:?}\ndaemon log:\n{}",
            fs::read_to_string(state.join("agent-talkd/agent-talkd.log")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(50));
    };
    let failure_brief = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["read", &failure_id.to_string()],
    ));
    assert!(failure_brief.contains("# agent-talk 配達失敗通知"));
    assert!(failure_brief.contains(&format!("- original: #{second_id}")));
    assert!(failure_brief.contains("second unread body"));
}

fn kill_daemon(rpc: &Path) {
    let owner = Command::new("fuser").arg(rpc).output().unwrap();
    assert!(owner.status.success(), "fuser could not find daemon");
    let daemon_pid: i32 = String::from_utf8(owner.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(daemon_pid, libc::SIGKILL) }, 0);
}

fn message_id(output: &str) -> u64 {
    output
        .rsplit_once(": #")
        .unwrap_or_else(|| panic!("message id missing from {output:?}"))
        .1
        .parse()
        .unwrap()
}

fn read_id_from_bell(screen: &str) -> u64 {
    screen
        .rsplit_once("read ")
        .unwrap_or_else(|| panic!("read command missing from {screen:?}"))
        .1
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn agent(
    socket: &str,
    runtime: &Path,
    state: &Path,
    legacy_mail: &Path,
    pane: &str,
    args: &[&str],
) -> Output {
    let output = agent_raw(socket, runtime, state, legacy_mail, pane, args);
    assert!(
        output.status.success(),
        "agent-talk {args:?} failed: {}\ndaemon log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(state.join("agent-talkd/agent-talkd.log")).unwrap_or_default()
    );
    output
}

fn agent_raw(
    socket: &str,
    runtime: &Path,
    state: &Path,
    legacy_mail: &Path,
    pane: &str,
    args: &[&str],
) -> Output {
    let binary = env!("CARGO_BIN_EXE_agent-talk");
    Command::new(binary)
        .args(args)
        .env("AGENT_TALK_TMUX_SOCKET", socket)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .env("AGENT_TALK_DIR", legacy_mail)
        .env("TMUX", format!("{socket},1,0"))
        .env("TMUX_PANE", pane)
        .output()
        .unwrap()
}

fn agent_raw_stdin(
    socket: &str,
    runtime: &Path,
    state: &Path,
    legacy_mail: &Path,
    pane: &str,
    args: &[&str],
    stdin: &[u8],
) -> Output {
    let binary = env!("CARGO_BIN_EXE_agent-talk");
    let mut child = Command::new(binary)
        .args(args)
        .env("AGENT_TALK_TMUX_SOCKET", socket)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .env("AGENT_TALK_DIR", legacy_mail)
        .env("TMUX", format!("{socket},1,0"))
        .env("TMUX_PANE", pane)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
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
