mod config;
mod daemon;
mod herdr;
mod history;
mod update;

use anyhow::{Result, bail};

const HELP: &str = "agent-talk — remote messages for Herdr sessions\n\nUsage: agent-talk daemon | update | --version | --help\n\nThe daemon serves the embedded browser app and HTTP API.\nSet AGENT_TALK_HTTP_ADDR and AGENT_TALK_HERDR_SOCKET.\nPeer communication and broker commands have been removed.\n";

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agent-talk: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [arg] if arg == "--help" => print!("{HELP}"),
        [arg] if arg == "--version" => println!("agent-talk {}", env!("CARGO_PKG_VERSION")),
        [arg] if arg == "daemon" => daemon::run(config::Config::discover()?).await?,
        [arg] if arg == "update" => {
            update::run()?;
        }
        _ => bail!("{HELP}"),
    }
    Ok(())
}
