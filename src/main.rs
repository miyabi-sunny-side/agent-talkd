mod backend;
mod client;
mod config;
mod daemon;
mod help;
mod herdr;
mod journal;
mod lifecycle;
mod paths;
mod procid;
mod protocol;
mod run;
mod state;
mod update;

use std::{env, process::ExitCode};

use anyhow::Result;
use config::Config;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(
            u8::try_from(code.clamp(0, 255)).expect("clamped exit code fits into u8"),
        ),
        Err(error) => {
            eprintln!("agent-talk: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<i32> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        eprint!("{}", help::GLOBAL);
        return Ok(1);
    };
    let args: Vec<_> = args.collect();
    if command == "--help" && args.is_empty() {
        print!("{}", help::GLOBAL);
        return Ok(0);
    }
    if args.first().is_some_and(|arg| arg == "--help")
        && let Some(text) = help::command(&command)
    {
        println!("{text}");
        return Ok(0);
    }
    if command == "--version" {
        println!("agent-talk {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }

    if command == "update" {
        if !args.is_empty() {
            eprint!("{}", help::GLOBAL);
            return Ok(1);
        }
        return update::run().await;
    }
    if command == "ensure-daemon" {
        if !args.is_empty() {
            eprint!("{}", help::GLOBAL);
            return Ok(1);
        }
        return lifecycle::run_ensure_command().await;
    }
    if command == "daemon-status" {
        if !args.is_empty() {
            eprint!("{}", help::GLOBAL);
            return Ok(1);
        }
        return lifecycle::run_status_command().await;
    }
    if command == "run" {
        return run::run(args).await;
    }

    if matches!(
        command.as_str(),
        "register" | "unregister" | "busy" | "idle" | "turn-end"
    ) && backend::self_pane().is_none()
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
    if !help::is_known(&command) {
        eprint!("{}", help::GLOBAL);
        return Ok(1);
    }
    client::run(config, command, args).await
}
