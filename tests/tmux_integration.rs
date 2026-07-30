use std::{
    fs,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
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
#[allow(clippy::too_many_lines)]
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
    tmux(
        &name,
        &[
            "set-option",
            "-g",
            "@agent_talkd_allowed_skills",
            "deliver,commit",
        ],
    );
    tmux(
        &name,
        &[
            "set-option",
            "-g",
            "@agent_talkd_allowed_sources",
            "mobile,human,system,claude",
        ],
    );
    tmux(
        &name,
        &[
            "set-option",
            "-g",
            "@agent_talkd_skill_syntax",
            "claude=slash,codex=dollar",
        ],
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
    let http = runtime
        .join("agent-talkd")
        .join(format!("{}.http.sock", socket_name.to_string_lossy()));
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
    let ensured = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["ensure-daemon"],
    ));
    assert!(ensured.contains(&format!("daemon {} ready", env!("CARGO_PKG_VERSION"))));
    let sessions = text(tmux(&name, &["list-sessions", "-F", "#{session_name}"]));
    assert_eq!(sessions, "test");
    let status = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["daemon-status"],
    ));
    assert!(status.contains("\"ready\":true"));
    let hello = http_request(&http, "GET", "/v1/hello");
    assert!(hello.starts_with("HTTP/1.1 200"), "{hello}");
    assert!(hello.contains("\"name\":\"agent-talk\""), "{hello}");
    let web_who = http_request(&http, "GET", "/v1/who");
    assert!(web_who.starts_with("HTTP/1.1 200"), "{web_who}");
    assert!(web_who.contains("\"name\":\"claude\""), "{web_who}");
    let empty_screen = http_request(&http, "GET", "/v1/agents/screen");
    assert!(empty_screen.starts_with("HTTP/1.1 400"), "{empty_screen}");
    let web_who_after_empty_screen = http_request(&http, "GET", "/v1/who");
    assert!(
        web_who_after_empty_screen.starts_with("HTTP/1.1 200"),
        "{web_who_after_empty_screen}"
    );
    tmux(
        &name,
        &["send-keys", "-t", &panes[1], "-l", "screen-capture-marker"],
    );
    let encoded_pane = panes[1].replace('%', "%25");
    let web_screen = http_request(&http, "GET", &format!("/v1/agents/{encoded_pane}/screen"));
    assert!(web_screen.starts_with("HTTP/1.1 200"), "{web_screen}");
    assert!(
        web_screen.contains(&format!("\"pane_id\":\"{}\"", panes[1])),
        "{web_screen}"
    );
    assert!(web_screen.contains("screen-capture-marker"), "{web_screen}");
    let unregistered_pane = panes[2].replace('%', "%25");
    let unknown_screen = http_request(
        &http,
        "GET",
        &format!("/v1/agents/{unregistered_pane}/screen"),
    );
    assert!(
        unknown_screen.starts_with("HTTP/1.1 404"),
        "{unknown_screen}"
    );
    let malformed_screen = http_request(&http, "GET", "/v1/agents/%2F/screen");
    assert!(
        malformed_screen.starts_with("HTTP/1.1 400"),
        "{malformed_screen}"
    );
    let invalid_screen = http_request(&http, "GET", "/v1/agents/%25bad/screen");
    assert!(
        invalid_screen.starts_with("HTTP/1.1 404"),
        "{invalid_screen}"
    );
    let mailboxes = http_request(&http, "GET", "/v1/mailboxes");
    assert!(mailboxes.starts_with("HTTP/1.1 200"), "{mailboxes}");
    assert!(mailboxes.contains("\"mobile\""), "{mailboxes}");
    let empty_mailbox = http_request(&http, "GET", "/v1/mailbox/mobile?limit=1");
    assert!(empty_mailbox.starts_with("HTTP/1.1 200"), "{empty_mailbox}");
    assert!(empty_mailbox.contains("\"events\":[]"), "{empty_mailbox}");
    let unknown_mailbox = http_request(&http, "GET", "/v1/mailbox/not-allowed");
    assert!(
        unknown_mailbox.starts_with("HTTP/1.1 404"),
        "{unknown_mailbox}"
    );
    let invalid_mailbox = http_request(&http, "GET", "/v1/mailbox/Bad");
    assert!(
        invalid_mailbox.starts_with("HTTP/1.1 404"),
        "{invalid_mailbox}"
    );
    let malformed_mailbox = http_request(&http, "GET", "/v1/mailbox/bad%2Fname");
    assert!(
        malformed_mailbox.starts_with("HTTP/1.1 400"),
        "{malformed_mailbox}"
    );
    let invalid_mailbox_query = http_request(&http, "GET", "/v1/mailbox/mobile?limit=0");
    assert!(
        invalid_mailbox_query.starts_with("HTTP/1.1 400"),
        "{invalid_mailbox_query}"
    );
    let web_static = http_request(&http, "GET", "/nested/spa/route");
    assert!(web_static.starts_with("HTTP/1.1 200"), "{web_static}");
    assert!(web_static.contains("agent talk · registry"), "{web_static}");
    let unknown_api = http_request(&http, "GET", "/v1/missing");
    assert!(unknown_api.starts_with("HTTP/1.1 404"), "{unknown_api}");
    for path in [
        "/v1/who",
        "/v1/letters",
        "/v1/recover",
        "/v1/mailbox/mobile",
    ] {
        let write_rejected = http_request(&http, "POST", path);
        assert!(
            write_rejected.starts_with("HTTP/1.1 405"),
            "POST {path}: {write_rejected}"
        );
        assert!(
            write_rejected.to_ascii_lowercase().contains("allow: get"),
            "{write_rejected}"
        );
    }
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["register", "codex"],
    );

    // run keeps the pane registered only for the child process lifetime.
    let ready = root.path().join("run-ready");
    let stop = root.path().join("run-stop");
    let binary = env!("CARGO_BIN_EXE_agent-talk");
    let mut runner = Command::new(binary)
        .args([
            "run",
            "runner",
            "sh",
            "-c",
            "touch \"$1\"; while [ ! -e \"$2\" ]; do sleep 0.05; done",
            "sh",
        ])
        .arg(&ready)
        .arg(&stop)
        .env("AGENT_TALK_TMUX_SOCKET", &socket)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_STATE_HOME", &state)
        .env("AGENT_TALK_DIR", &legacy_mail)
        .env("AGENT_TALK_RPC_SOCKET", &rpc)
        .env("TMUX", format!("{socket},1,0"))
        .env("TMUX_PANE", &panes[2])
        .spawn()
        .unwrap();
    wait_for(|| ready.exists());
    let during_run = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["who"],
    ));
    assert!(during_run.contains("runner"));
    fs::write(&stop, "stop").unwrap();
    assert!(runner.wait().unwrap().success());
    let after_run = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["who"],
    ));
    assert!(!after_run.contains("runner"));

    // A failed registration stays silent and does not block the child.
    let registration_failure = agent_raw(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["run", "bad/name", "sh", "-c", "exit 7"],
    );
    assert_eq!(registration_failure.status.code(), Some(7));
    assert!(registration_failure.stderr.is_empty());

    // A registered pane is cleaned up even when the executable cannot start.
    let spawn_failure = agent_raw(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["run", "runner", "agent-talk-command-that-does-not-exist"],
    );
    assert_eq!(spawn_failure.status.code(), Some(127));
    let after_spawn_failure = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["who"],
    ));
    assert!(!after_spawn_failure.contains("runner"));

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

    // External source labels and runtime-specific skill prefixes are fixed by the daemon.
    let mobile_send = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &[
            "send",
            "claude",
            "--from",
            "mobile",
            "--skill",
            "deliver",
            "mobile-only-journal-body",
        ],
    ));
    let mobile_id = message_id(&mobile_send);
    let claude_screen = text(tmux(&name, &["capture-pane", "-p", "-t", &panes[1]]));
    assert!(claude_screen.contains(&format!(
        "/deliver [agent-talk] mobile から依頼が届きました。agent-talk read {mobile_id}"
    )));
    assert!(!claude_screen.contains("mobile-only-journal-body"));
    let mobile_brief = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["read", &mobile_id.to_string()],
    ));
    assert!(mobile_brief.contains("- from: mobile (session: test, pane:"));
    assert!(mobile_brief.contains(&format!("agent-talk reply {mobile_id}")));
    assert!(mobile_brief.contains("mobile-only-journal-body"));
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["turn-end"],
    );

    let codex_skill = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["send", "codex", "--skill", "deliver", "codex skill body"],
    ));
    let codex_skill_id = message_id(&codex_skill);
    let codex_screen = text(tmux(&name, &["capture-pane", "-p", "-t", &panes[0]]));
    assert!(codex_screen.contains(&format!(
        "$deliver [agent-talk] human から依頼が届きました。agent-talk read {codex_skill_id}"
    )));
    assert!(!codex_screen.contains("codex skill body"));
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["read", &codex_skill_id.to_string()],
    );
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["turn-end"],
    );

    let claude_screen_before_peer_skill =
        text(tmux(&name, &["capture-pane", "-p", "-t", &panes[1]]));
    for args in [
        vec!["send", "claude", "--skill", "deliver", "peer body"],
        vec!["send", "claude", "--skill", "Deliver", "peer body"],
    ] {
        let rejected = agent_raw(&socket, &runtime, &state, &legacy_mail, &panes[0], &args);
        assert!(!rejected.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&rejected.stderr)
                .contains("登録agent paneから --skill は指定できません"),
            "{args:?}: {}",
            String::from_utf8_lossy(&rejected.stderr)
        );
    }
    let claude_screen_after_peer_skill =
        text(tmux(&name, &["capture-pane", "-p", "-t", &panes[1]]));
    assert_eq!(
        claude_screen_after_peer_skill, claude_screen_before_peer_skill,
        "rejected peer skill must not ring the target doorbell"
    );

    for (pane, args, expected) in [
        (
            panes[0].as_str(),
            vec!["send", "claude", "--from", "mobile", "body"],
            "登録agent paneから --from を上書きできません",
        ),
        (
            panes[2].as_str(),
            vec!["send", "claude", "--from", "system", "body"],
            "予約済み",
        ),
        (
            panes[2].as_str(),
            vec!["send", "claude", "--from", "claude", "body"],
            "予約済み",
        ),
        (
            panes[2].as_str(),
            vec!["send", "claude", "--skill", "Deliver", "body"],
            "skill名は64文字以内",
        ),
        (
            panes[2].as_str(),
            vec!["send", "claude", "--skill", "danger", "body"],
            "許可されていません",
        ),
    ] {
        let rejected = agent_raw(&socket, &runtime, &state, &legacy_mail, pane, &args);
        assert!(!rejected.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains(expected),
            "{args:?}: {}",
            String::from_utf8_lossy(&rejected.stderr)
        );
    }

    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["register", "cursor"],
    );
    let unmapped = agent_external_raw(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &["send", "cursor", "--skill", "deliver", "body"],
    );
    assert!(
        !unmapped.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&unmapped.stdout),
        String::from_utf8_lossy(&unmapped.stderr)
    );
    assert!(String::from_utf8_lossy(&unmapped.stderr).contains("skill記法"));
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["unregister"],
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

    let no_reply_send = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[2],
        &["send", panes[0].as_str(), "--no-reply", "one-way body"],
    ));
    let no_reply_id = message_id(&no_reply_send);
    let no_reply_brief = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["read", &no_reply_id.to_string()],
    ));
    assert!(
        no_reply_brief
            .contains("原則不要 (一方向の連絡。重大な実害を防ぐ異議がある場合のみ1通だけ返信可)")
    );
    let no_reply_bell = text(tmux(&name, &["capture-pane", "-p", "-t", &panes[0]]));
    assert!(
        no_reply_bell.contains("[agent-talk] claude から連絡が届きました。"),
        "pane={no_reply_bell}"
    );
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["turn-end"],
    );
    let rejected_external = agent_external_raw(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &[
            "send",
            panes[1].as_str(),
            "--from",
            "mobile",
            "--no-reply",
            "bad",
        ],
    );
    assert!(!rejected_external.status.success());
    assert!(
        String::from_utf8_lossy(&rejected_external.stderr).contains("--no-reply は外部mailbox送信")
    );

    // External mailbox events are journaled without exposing pane-to-pane traffic.
    let external = text(agent_external(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &[
            "send",
            panes[1].as_str(),
            "--from",
            "mobile",
            "external body",
        ],
    ));
    assert!(external.starts_with("sent -> "));
    let external_id = message_id(&external);
    let external_brief = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["read", &external_id.to_string()],
    ));
    assert!(external_brief.contains(&format!("agent-talk reply {external_id}")));
    let reply = text(agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["reply", &external_id.to_string(), "reply body"],
    ));
    assert!(reply.starts_with("replied: #"));
    let mailbox = text(agent_external(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &["mailbox-list-v1", "mobile"],
    ));
    assert!(mailbox.contains("\"version\":1"));
    assert!(mailbox.contains("external body"));
    assert!(mailbox.contains("reply body"));
    let mailbox_again = text(agent_external(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &[
            "mailbox-list-v1",
            "mobile",
            "--after",
            &external_id.to_string(),
            "--limit",
            "1",
        ],
    ));
    assert!(mailbox_again.contains("reply body"));
    let web_mailbox = http_request(
        &http,
        "GET",
        &format!("/v1/mailbox/mobile?after={external_id}&limit=1"),
    );
    assert!(web_mailbox.starts_with("HTTP/1.1 200"), "{web_mailbox}");
    assert!(web_mailbox.contains("reply body"), "{web_mailbox}");
    assert!(!web_mailbox.contains("external body"), "{web_mailbox}");
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

    // Queued content survives a graceful daemon restart and can be read after restart.
    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[1],
        &["busy"],
    );
    let queued = text(agent_external(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &["send", "claude", "--skill", "deliver", "first durable body"],
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

    agent(
        &socket,
        &runtime,
        &state,
        &legacy_mail,
        &panes[0],
        &["internal-daemon-shutdown"],
    );
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
    let recovered_screen = text(tmux(&name, &["capture-pane", "-p", "-t", &panes[1]]));
    assert!(recovered_screen.contains(&format!(
        "/deliver [agent-talk] human から依頼が届きました。agent-talk read {queued_id}"
    )));
    assert!(!recovered_screen.contains("first durable body"));

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

    let sessions = text(tmux(&name, &["list-sessions", "-F", "#{session_name}"]));
    assert_eq!(sessions, "test");

    // A replacement server on the same socket cannot inherit the old daemon.
    tmux(&name, &["kill-server"]);
    tmux_eventually(
        &name,
        &["new-session", "-d", "-s", "replacement", "sleep 30"],
    );
    wait_for(|| !rpc.exists());
    wait_for(|| !http.exists());
}

fn http_request(socket: &Path, method: &str, path: &str) -> String {
    let mut stream = UnixStream::connect(socket).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
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
        .env("AGENT_TALK_RPC_SOCKET", rpc_socket(runtime, socket))
        .env("TMUX", format!("{socket},1,0"))
        .env("TMUX_PANE", pane)
        .output()
        .unwrap()
}

fn agent_external(
    socket: &str,
    runtime: &Path,
    state: &Path,
    legacy_mail: &Path,
    args: &[&str],
) -> Output {
    let output = agent_external_raw(socket, runtime, state, legacy_mail, args);
    assert!(
        output.status.success(),
        "agent-talk external {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn agent_external_raw(
    socket: &str,
    runtime: &Path,
    state: &Path,
    legacy_mail: &Path,
    args: &[&str],
) -> Output {
    let binary = env!("CARGO_BIN_EXE_agent-talk");
    Command::new(binary)
        .args(args)
        .env("AGENT_TALK_TMUX_SOCKET", socket)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_STATE_HOME", state)
        .env("AGENT_TALK_DIR", legacy_mail)
        .env("AGENT_TALK_RPC_SOCKET", rpc_socket(runtime, socket))
        .env("TMUX", format!("{socket},1,0"))
        .env_remove("TMUX_PANE")
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
        .env("AGENT_TALK_RPC_SOCKET", rpc_socket(runtime, socket))
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

fn rpc_socket(runtime: &Path, tmux_socket: &str) -> std::path::PathBuf {
    runtime
        .join("agent-talkd")
        .join(Path::new(tmux_socket).file_name().unwrap())
        .with_extension("sock")
}

fn tmux_eventually(name: &str, args: &[&str]) -> Output {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = Command::new("tmux")
            .arg("-L")
            .arg(name)
            .args(args)
            .output()
            .unwrap();
        if output.status.success() {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "tmux failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn text(output: Output) -> String {
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "condition timed out");
        thread::sleep(Duration::from_millis(50));
    }
}
