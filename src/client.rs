use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::sleep,
};

use crate::{
    config::Config,
    protocol::{Request, Response},
};

pub async fn run(config: Config, command: String, args: Vec<String>) -> Result<i32> {
    let mut stdin = String::new();
    if command == "send" && args.len() <= 1 {
        io::stdin().read_to_string(&mut stdin)?;
        while stdin.ends_with('\n') {
            stdin.pop();
        }
    }
    let request = Request {
        command,
        args,
        stdin,
        pane: env::var("TMUX_PANE").ok(),
    };
    let response = request_daemon(&config, &request).await?;
    print_response(response)
}

async fn request_daemon(config: &Config, request: &Request) -> Result<Response> {
    let stream = match UnixStream::connect(&config.rpc_socket).await {
        Ok(stream) => stream,
        Err(_) => {
            ensure_daemon(config).await.map_err(|error| {
                anyhow::anyhow!(
                    "デーモンに接続できません (sandbox 内なら承認付きで再実行): {error}"
                )
            })?;
            UnixStream::connect(&config.rpc_socket)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "デーモンに接続できません (sandbox 内なら承認付きで再実行): {error}"
                    )
                })?
        }
    };
    let (reader, mut writer) = stream.into_split();
    writer.write_all(&serde_json::to_vec(request)?).await?;
    writer.write_all(b"\n").await?;
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).await?;
    if line.is_empty() {
        bail!("daemon closed the connection");
    }
    Ok(serde_json::from_str(&line)?)
}

async fn ensure_daemon(config: &Config) -> Result<()> {
    let parent = config
        .rpc_socket
        .parent()
        .context("runtime directory missing")?;
    fs::create_dir_all(parent)?;
    let lock = config.rpc_socket.with_extension("spawn");
    let mut deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(guard) => {
                if config.rpc_socket.exists() {
                    let _ = fs::remove_file(&config.rpc_socket);
                }
                let executable = env::current_exe()?;
                let socket = config.tmux_socket.clone();
                let mut command = Command::new(executable);
                command
                    .arg("daemon")
                    .env("AGENT_TALK_TMUX_SOCKET", socket)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                command.spawn().context("cannot spawn agent-talk daemon")?;
                let startup_deadline = Instant::now() + Duration::from_secs(5);
                let result = wait_for_socket(config, startup_deadline).await;
                drop(guard);
                let _ = fs::remove_file(&lock);
                return result;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if UnixStream::connect(&config.rpc_socket).await.is_ok() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    let stale = fs::metadata(&lock)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|time| time.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(10));
                    if stale {
                        let _ = fs::remove_file(&lock);
                        deadline = Instant::now() + Duration::from_secs(5);
                        continue;
                    }
                    bail!("daemon startup timed out");
                }
                sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn wait_for_socket(config: &Config, deadline: Instant) -> Result<()> {
    while Instant::now() < deadline {
        if UnixStream::connect(&config.rpc_socket).await.is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    bail!("daemon startup timed out")
}

fn print_response(response: Response) -> Result<i32> {
    if !response.stdout.is_empty() {
        print!("{}", response.stdout);
    }
    if !response.stderr.is_empty() {
        eprint!("{}", response.stderr);
    }
    Ok(response.code)
}
