use std::process::{Command, Stdio};

const COMMANDS: &[&str] = &[
    "update",
    "ensure-daemon",
    "daemon-status",
    "register",
    "unregister",
    "busy",
    "idle",
    "turn-end",
    "who",
    "gc",
    "watch",
    "resolve",
    "send",
    "read",
    "reply",
    "mailbox-list-v1",
    "daemon",
    "internal-daemon-status",
    "internal-daemon-shutdown",
    "internal-pane-exited",
    "internal-reconcile",
];

fn isolated() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-talk"));
    command
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("AGENT_TALK_TMUX_SOCKET")
        .env_remove("AGENT_TALK_RPC_SOCKET")
        .env_remove("XDG_RUNTIME_DIR")
        .stdin(Stdio::null());
    command
}

#[test]
fn every_command_has_side_effect_free_help() {
    for command in COMMANDS {
        let output = isolated().args([*command, "--help"]).output().unwrap();
        assert!(output.status.success(), "{command}: {:?}", output.status);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.starts_with(&format!("usage: agent-talk {command}")));
        assert!(output.stderr.is_empty(), "{command}: stderr is not empty");
    }
}

#[test]
fn global_help_and_no_args_keep_distinct_exit_behavior() {
    let help = isolated().arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("agent-talk <command> --help")
    );
    assert!(help.stderr.is_empty());

    let no_args = isolated().output().unwrap();
    assert!(!no_args.status.success());
    assert!(no_args.stdout.is_empty());
    assert!(!no_args.stderr.is_empty());
}

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
