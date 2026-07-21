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
    protocol::{Request, Response, SendOptions},
};

pub async fn run(config: Config, mut command: String, args: Vec<String>) -> Result<i32> {
    let (args, send_options) = if command == "send" {
        parse_send_args(args)?
    } else {
        (args, None)
    };
    if send_options.is_some() {
        command = "send-v2".into();
    }
    let mut stdin = String::new();
    if matches!(command.as_str(), "send" | "send-v2") && args.len() <= 1 {
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
        send_options,
    };
    let response = request_daemon(&config, &request).await?;
    print_response(response)
}

fn parse_send_args(args: Vec<String>) -> Result<(Vec<String>, Option<SendOptions>)> {
    let Some(addr) = args.first().cloned() else {
        return Ok((args, None));
    };
    let mut parsed = vec![addr];
    let mut options = SendOptions::default();
    let mut has_options = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--" => {
                parsed.extend(args[index + 1..].iter().cloned());
                return Ok((parsed, has_options.then_some(options)));
            }
            "--from" | "--skill" => {
                let option = args[index].as_str();
                let Some(value) = args.get(index + 1) else {
                    bail!("{option} には値が必要です");
                };
                let slot = if option == "--from" {
                    &mut options.from
                } else {
                    &mut options.skill
                };
                if slot.replace(value.clone()).is_some() {
                    bail!("{option} は複数指定できません");
                }
                has_options = true;
                index += 2;
            }
            value if value.starts_with("--") => bail!("不明なsendオプションです: {value}"),
            _ => {
                parsed.extend(args[index..].iter().cloned());
                return Ok((parsed, has_options.then_some(options)));
            }
        }
    }
    Ok((parsed, has_options.then_some(options)))
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

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
    };

    use tempfile::tempdir;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    use super::*;

    #[test]
    fn parses_send_options_before_inline_body() {
        let (args, options) = parse_send_args(vec![
            "claude".into(),
            "--from".into(),
            "mobile".into(),
            "--skill".into(),
            "deliver".into(),
            "hello".into(),
            "world".into(),
        ])
        .unwrap();
        assert_eq!(args, ["claude", "hello", "world"]);
        let options = options.unwrap();
        assert_eq!(options.from.as_deref(), Some("mobile"));
        assert_eq!(options.skill.as_deref(), Some("deliver"));
    }

    #[test]
    fn double_dash_preserves_option_like_body() {
        let (args, options) = parse_send_args(vec![
            "claude".into(),
            "--".into(),
            "--skill".into(),
            "literal".into(),
        ])
        .unwrap();
        assert_eq!(args, ["claude", "--skill", "literal"]);
        assert!(options.is_none());
    }

    #[tokio::test]
    async fn send_v2_error_does_not_fall_back_to_legacy_send() {
        let dir = tempdir().unwrap();
        let rpc_socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&rpc_socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut line = String::new();
            BufReader::new(reader).read_line(&mut line).await.unwrap();
            let request: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(request.command, "send-v2");
            assert_eq!(
                request.send_options.unwrap().skill.as_deref(),
                Some("deliver")
            );
            let response = Response::error("unknown command");
            writer
                .write_all(&serde_json::to_vec(&response).unwrap())
                .await
                .unwrap();
            writer.write_all(b"\n").await.unwrap();
        });
        let config = Config {
            tmux_socket: String::new(),
            rpc_socket,
            journal: PathBuf::new(),
            log: PathBuf::new(),
            queue_limit: 1,
            log_level: "info".into(),
            skill_syntax: BTreeMap::new(),
            allowed_skills: None,
            allowed_sources: BTreeSet::new(),
        };
        let code = run(
            config,
            "send".into(),
            vec![
                "claude".into(),
                "--skill".into(),
                "deliver".into(),
                "body".into(),
            ],
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(code, 1);
    }
}
