use std::{env, io, os::unix::process::ExitStatusExt};

use anyhow::Result;
use tokio::{process::Command, signal::unix};

use crate::{
    config::Config,
    help, lifecycle,
    protocol::{Request, Response},
};

pub async fn run(args: Vec<String>) -> Result<i32> {
    let [name, executable, child_args @ ..] = args.as_slice() else {
        eprintln!("{}", help::usage("run"));
        return Ok(1);
    };

    let config = discover_tmux_config();
    let registered = if let Some(config) = config.as_ref() {
        request(config, "register", vec![name.clone()])
            .await
            .is_some_and(|response| response.code == 0)
    } else {
        false
    };

    let result = execute(executable, child_args).await;

    if registered && let Some(config) = config.as_ref() {
        let _ = request(config, "unregister", Vec::new()).await;
    }

    match result {
        Ok(code) => Ok(code),
        Err(error) => {
            eprintln!("agent-talk: '{executable}' を実行できません: {error}");
            Ok(if error.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            })
        }
    }
}

fn discover_tmux_config() -> Option<Config> {
    if env::var_os("TMUX").is_none() || env::var_os("TMUX_PANE").is_none() {
        return None;
    }
    Config::discover().ok()
}

async fn request(config: &Config, command: &str, args: Vec<String>) -> Option<Response> {
    lifecycle::request(
        config,
        &Request {
            command: command.into(),
            args,
            stdin: String::new(),
            pane: env::var("TMUX_PANE").ok(),
            send_options: None,
        },
    )
    .await
    .ok()
}

async fn execute(executable: &str, args: &[String]) -> io::Result<i32> {
    let mut interrupt = unix::signal(unix::SignalKind::interrupt())?;
    let mut quit = unix::signal(unix::SignalKind::quit())?;
    let mut terminate = unix::signal(unix::SignalKind::terminate())?;
    let mut child = Command::new(executable).args(args).spawn()?;

    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = interrupt.recv() => {}
            _ = quit.recv() => {}
            _ = terminate.recv() => {}
        }
    };

    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
}
