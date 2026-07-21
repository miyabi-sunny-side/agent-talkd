use std::process::Command;

#[test]
fn version_is_available_without_tmux_or_daemon() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-talk"))
        .arg("--version")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("AGENT_TALK_TMUX_SOCKET")
        .env_remove("AGENT_TALK_RPC_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("agent-talk {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}
