use std::{
    env,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::tmux::socket_name;

#[derive(Debug, Clone)]
pub struct Config {
    pub tmux_socket: String,
    pub rpc_socket: PathBuf,
    pub journal: PathBuf,
    pub log: PathBuf,
    pub queue_limit: usize,
    pub log_level: String,
}

impl Config {
    pub fn discover() -> Result<Self> {
        let tmux_socket = discover_tmux_socket()?;
        let name = socket_name(&tmux_socket);
        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".cache/agent-talkd/run"));
        let state = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/state"));
        let queue_limit = tmux_option(&tmux_socket, "@agent_talkd_queue_limit")
            .and_then(|value| value.parse().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(1000);
        let log_level = tmux_option(&tmux_socket, "@agent_talkd_log_level")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "info".into());
        Ok(Self {
            tmux_socket,
            rpc_socket: env::var_os("AGENT_TALK_RPC_SOCKET")
                .map(PathBuf::from)
                .unwrap_or_else(|| runtime.join("agent-talkd").join(format!("{name}.sock"))),
            journal: state.join("agent-talkd").join(format!("{name}.journal")),
            log: state.join("agent-talkd").join("agent-talkd.log"),
            queue_limit,
            log_level,
        })
    }
}

fn tmux_option(socket: &str, name: &str) -> Option<String> {
    let output = std::process::Command::new("tmux")
        .args(["-S", socket, "show-option", "-gqv", name])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn discover_tmux_socket() -> Result<String> {
    if let Some(value) = env::var_os("AGENT_TALK_TMUX_SOCKET") {
        return Ok(value.to_string_lossy().into_owned());
    }
    if let Ok(tmux) = env::var("TMUX")
        && let Some(socket) = tmux.split(',').next()
        && !socket.is_empty()
    {
        return Ok(socket.to_owned());
    }
    let output = std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{socket_path}"])
        .output()
        .context("cannot locate tmux server")?;
    if !output.status.success() {
        bail!("tmux サーバーに接続できません (sandbox 内なら承認付きで再実行)");
    }
    let socket = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if socket.is_empty() || Path::new(&socket).file_name().is_none() {
        bail!("tmux socket path is empty");
    }
    Ok(socket)
}
