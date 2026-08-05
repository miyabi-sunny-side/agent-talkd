use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    process::{Command, Stdio},
};

const COMMANDS: &[&str] = &[
    "update",
    "ensure-daemon",
    "daemon-status",
    "run",
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
    "send-message",
    "read-message",
    "ack-message",
    "list-peers",
    "reply",
    "mailbox-list",
    "daemon",
    "internal-daemon-status",
    "internal-daemon-shutdown",
    "internal-pane-exited",
    "internal-reconcile",
];

fn isolated() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-talk"));
    // herdr の pane 内で cargo test を実行しても、テスト対象の process が
    // 実環境の daemon / herdr へ届かないよう、環境を切り離す。
    command
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_ENV")
        .env_remove("AGENT_TALK_HERDR_SOCKET")
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
fn version_is_available_without_herdr_or_daemon() {
    let output = isolated().arg("--version").output().unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("agent-talk {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn run_transparently_executes_child_and_returns_its_status() {
    let output = isolated()
        .args([
            "run",
            "demo",
            "sh",
            "-c",
            "printf '<%s>|<%s>' \"$1\" \"$2\"; exit 7",
            "sh",
            "--literal",
            "two words",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "<--literal>|<two words>"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn run_preserves_child_sigint_behavior() {
    let output = isolated()
        .args(["run", "demo", "sh", "-c", "kill -INT $$"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(130));
}

#[test]
fn run_survives_foreground_sigint_and_returns_child_status() {
    let mut command = isolated();
    command
        .args([
            "run",
            "demo",
            "sh",
            "-c",
            "printf 'ready\\n'; while :; do sleep 1; done",
        ])
        .process_group(0)
        .stdout(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");

    let result = unsafe { libc::kill(-child.id().cast_signed(), libc::SIGINT) };
    assert_eq!(result, 0);
    assert_eq!(child.wait().unwrap().code(), Some(130));
}

#[test]
fn run_reports_usage_and_spawn_failures_without_contacting_a_daemon() {
    let root = tempfile::tempdir().unwrap();
    let rpc_socket = root.path().join("daemon.sock");

    let missing_args = isolated()
        .arg("run")
        .env("AGENT_TALK_RPC_SOCKET", &rpc_socket)
        .output()
        .unwrap();
    assert_eq!(missing_args.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(missing_args.stderr).unwrap(),
        "usage: agent-talk run <name> <executable> [args...]\n"
    );

    let missing_executable = isolated()
        .args(["run", "demo", "agent-talk-command-that-does-not-exist"])
        .env("AGENT_TALK_RPC_SOCKET", &rpc_socket)
        .output()
        .unwrap();
    assert_eq!(missing_executable.status.code(), Some(127));
    assert!(
        String::from_utf8(missing_executable.stderr)
            .unwrap()
            .contains("agent-talk-command-that-does-not-exist")
    );

    let not_executable = root.path().join("not-executable");
    fs::write(&not_executable, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&not_executable, fs::Permissions::from_mode(0o644)).unwrap();
    let permission_denied = isolated()
        .args(["run", "demo"])
        .arg(&not_executable)
        .output()
        .unwrap();
    assert_eq!(permission_denied.status.code(), Some(126));
    assert!(!rpc_socket.exists());
}
