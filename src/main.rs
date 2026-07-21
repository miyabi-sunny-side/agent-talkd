mod client;
mod config;
mod daemon;
mod journal;
mod lifecycle;
mod protocol;
mod state;
mod tmux;
mod update;

use std::{env, process::ExitCode};

use anyhow::Result;
use config::Config;

const HELP: &str = r#"agent-talk: tmux 上の対話エージェント同士の連絡係。

  agent-talk --version
  agent-talk update
  agent-talk ensure-daemon
  agent-talk daemon-status
  agent-talk register <name>
  agent-talk unregister
  agent-talk busy | idle
  agent-talk turn-end
  agent-talk who
  agent-talk gc
  agent-talk resolve <addr>
  agent-talk send <addr> [--from <source>] [--skill <name>] [--] [message]
  agent-talk read <id>
"#;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("agent-talk: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<i32> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        eprint!("{HELP}");
        return Ok(1);
    };
    if command == "--version" {
        println!("agent-talk {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let args: Vec<_> = args.collect();

    if command == "update" {
        if !args.is_empty() {
            eprint!("{HELP}");
            return Ok(1);
        }
        return update::run().await;
    }
    if command == "ensure-daemon" {
        if !args.is_empty() {
            eprint!("{HELP}");
            return Ok(1);
        }
        return lifecycle::run_ensure_command().await;
    }
    if command == "daemon-status" {
        if !args.is_empty() {
            eprint!("{HELP}");
            return Ok(1);
        }
        return lifecycle::run_status_command().await;
    }

    if matches!(
        command.as_str(),
        "register" | "unregister" | "busy" | "idle" | "turn-end"
    ) && (env::var_os("TMUX").is_none() || env::var_os("TMUX_PANE").is_none())
    {
        return Ok(0);
    }
    if matches!(command.as_str(), "gc" | "watch") {
        return Ok(0);
    }

    let config = Config::discover()?;
    if command == "daemon" {
        daemon::run(config).await?;
        return Ok(0);
    }
    let known = matches!(
        command.as_str(),
        "register"
            | "unregister"
            | "busy"
            | "idle"
            | "turn-end"
            | "who"
            | "gc"
            | "watch"
            | "resolve"
            | "send"
            | "read"
            | "internal-daemon-status"
            | "internal-daemon-shutdown"
            | "internal-pane-exited"
            | "internal-reconcile"
    );
    if !known {
        eprint!("{HELP}");
        return Ok(1);
    }
    client::run(config, command, args).await
}
