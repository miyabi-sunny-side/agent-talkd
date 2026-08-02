use std::{
    env,
    fs::{self, OpenOptions},
    io,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::sleep,
};

use crate::{
    config::Config,
    protocol::{Request, Response},
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const STALE_LOCK_AGE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: u32,
    pub ready: bool,
    /// この daemon が面倒を見ている multiplexer。
    ///
    /// 旧 daemon は返さないので `default` で空になる。空は「不明」であって
    /// 「無い」ではないため、置換判定では **狭いとは見なさない**
    /// (version 不一致の既存経路が拾う)。
    #[serde(default)]
    pub backends: Vec<String>,
}

struct Peer {
    pid: i32,
    uid: u32,
}

enum Probe {
    Current(DaemonStatus),
    Legacy(Peer),
    Missing,
}

struct LockGuard {
    path: PathBuf,
    _file: fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub async fn run_ensure_command() -> Result<i32> {
    let Some(config) = Config::discover_optional()? else {
        println!("agent-talk: tmux server not available; daemon not applicable");
        return Ok(0);
    };
    let status = ensure_daemon(&config).await?;
    println!(
        "agent-talk: daemon {} ready (pid {})",
        status.version, status.pid
    );
    Ok(0)
}

pub async fn run_status_command() -> Result<i32> {
    let Some(config) = Config::discover_optional()? else {
        println!("agent-talk: tmux server not available; daemon not applicable");
        return Ok(0);
    };
    let status = daemon_status(&config).await?;
    println!("{}", serde_json::to_string(&status)?);
    Ok(0)
}

pub async fn request(config: &Config, request: &Request) -> Result<Response> {
    if let Ok(response) = request_once(config, request).await {
        Ok(response)
    } else {
        ensure_daemon(config).await.map_err(|error| {
            anyhow::anyhow!("デーモンに接続できません (sandbox 内なら承認付きで再実行): {error}")
        })?;
        request_once(config, request).await.map_err(|error| {
            anyhow::anyhow!("デーモンに接続できません (sandbox 内なら承認付きで再実行): {error}")
        })
    }
}

pub async fn daemon_status(config: &Config) -> Result<DaemonStatus> {
    match probe(config).await? {
        Probe::Current(status) if status.ready => Ok(status),
        Probe::Current(_) => bail!("daemon is not ready"),
        Probe::Legacy(_) => bail!("daemon does not expose a version"),
        Probe::Missing => bail!("daemon is not running"),
    }
}

pub async fn ensure_daemon(config: &Config) -> Result<DaemonStatus> {
    let _lock = acquire_lock(config).await?;
    stop_daemons_with_narrower_backends(config).await?;
    match probe(config).await? {
        Probe::Current(status) if status.ready && status.version == env!("CARGO_PKG_VERSION") => {
            return Ok(status);
        }
        Probe::Current(_) => graceful_stop(config).await?,
        Probe::Legacy(peer) => stop_legacy(config, peer).await?,
        Probe::Missing => {}
    }

    if config.rpc_socket().exists() {
        fs::remove_file(config.rpc_socket()).with_context(|| {
            format!(
                "cannot remove stale socket {}",
                config.rpc_socket().display()
            )
        })?;
    }
    spawn_daemon(config)?;
    wait_for_expected_status(config, Instant::now() + STARTUP_TIMEOUT).await
}

/// 自分が担当する backend を **すべて** は見ていない daemon を止める。
///
/// 移行期には「herdr だけを知る daemon」が先に立っていることがある。そのまま
/// 起動すると、こちらは tmux socket を bind した直後に herdr socket 側の生存を
/// 見つけて中断し、死んだ socket file だけが残る。tmux 側のクライアントは
/// 永久に daemon へ届かなくなるので、狭い daemon は明示的に置換する。
async fn stop_daemons_with_narrower_backends(config: &Config) -> Result<()> {
    let required = config.backend_names();
    for path in &config.rpc_sockets {
        let Some(status) = status_at(path).await else {
            continue;
        };
        let covers = required
            .iter()
            .all(|backend| status.backends.iter().any(|known| known == backend));
        // backends が空 = 旧 daemon。version 不一致の既存経路に任せる。
        if status.backends.is_empty() || covers {
            continue;
        }
        shutdown_at(path).await?;
    }
    Ok(())
}

/// 指定 path に居る daemon の status を取る。居なければ `None`。
async fn status_at(path: &Path) -> Option<DaemonStatus> {
    let mut stream = UnixStream::connect(path).await.ok()?;
    if stream.peer_cred().ok()?.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    write_request(&mut stream, &internal_request("internal-daemon-status"))
        .await
        .ok()?;
    let response = read_response(&mut stream).await.ok()?;
    (response.code == 0)
        .then(|| serde_json::from_str(response.stdout.trim()).ok())
        .flatten()
}

/// 指定 path の daemon を止め、socket が消えるまで待つ。
async fn shutdown_at(path: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(path).await?;
    write_request(&mut stream, &internal_request("internal-daemon-shutdown")).await?;
    let response = read_response(&mut stream).await?;
    if response.code != 0 {
        bail!("daemon shutdown failed: {}", response.stderr.trim());
    }
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if UnixStream::connect(path).await.is_err() {
            let _ = fs::remove_file(path);
            return Ok(());
        }
        sleep(Duration::from_millis(25)).await;
    }
    bail!("daemon shutdown timed out at {}", path.display())
}

async fn acquire_lock(config: &Config) -> Result<LockGuard> {
    let parent = config
        .rpc_socket()
        .parent()
        .context("runtime directory missing")?;
    fs::create_dir_all(parent)?;
    let path = config.rpc_socket().with_extension("spawn");
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                return Ok(LockGuard { path, _file: file });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|time| time.elapsed().ok())
                    .is_some_and(|age| age > STALE_LOCK_AGE);
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if Instant::now() >= deadline {
                    bail!("daemon lifecycle lock timed out");
                }
                sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn probe(config: &Config) -> Result<Probe> {
    let mut stream = match UnixStream::connect(&config.rpc_socket()).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(Probe::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    let credentials = stream.peer_cred()?;
    let uid = credentials.uid();
    if uid != unsafe { libc::geteuid() } {
        bail!("daemon socket peer uid mismatch");
    }
    let pid = credentials
        .pid()
        .context("daemon socket does not expose a peer pid")?;
    let request = internal_request("internal-daemon-status");
    write_request(&mut stream, &request).await?;
    let response = read_response(&mut stream).await?;
    if response.code == 0 {
        let status: DaemonStatus = serde_json::from_str(response.stdout.trim())
            .context("daemon returned an invalid status response")?;
        if status.pid != pid.cast_unsigned() {
            bail!("daemon status pid does not match socket peer");
        }
        return Ok(Probe::Current(status));
    }
    if response.stderr.contains("unknown command") {
        return Ok(Probe::Legacy(Peer { pid, uid }));
    }
    bail!("daemon status failed: {}", response.stderr.trim())
}

async fn graceful_stop(config: &Config) -> Result<()> {
    let response = request_once(config, &internal_request("internal-daemon-shutdown")).await?;
    if response.code != 0 {
        bail!("daemon shutdown failed: {}", response.stderr.trim());
    }
    wait_for_exit(config, None, Instant::now() + STARTUP_TIMEOUT).await
}

async fn stop_legacy(config: &Config, peer: Peer) -> Result<()> {
    if peer.uid != unsafe { libc::geteuid() } {
        bail!("refusing to stop a daemon owned by another uid");
    }
    verify_legacy_executable(peer.pid)?;
    let refreshed = match probe(config).await? {
        Probe::Legacy(refreshed) if refreshed.pid == peer.pid && refreshed.uid == peer.uid => {
            refreshed
        }
        Probe::Missing => return Ok(()),
        _ => bail!("daemon changed while preparing legacy replacement"),
    };
    if unsafe { libc::kill(refreshed.pid, libc::SIGTERM) } != 0 {
        return Err(io::Error::last_os_error()).context("cannot stop legacy daemon");
    }
    wait_for_exit(
        config,
        Some(refreshed.pid),
        Instant::now() + STARTUP_TIMEOUT,
    )
    .await
}

#[cfg(target_os = "linux")]
fn verify_legacy_executable(pid: i32) -> Result<()> {
    let path = fs::read_link(format!("/proc/{pid}/exe"))?;
    if path.file_name().and_then(|name| name.to_str()) != Some("agent-talk") {
        bail!(
            "refusing to stop unexpected legacy process: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_legacy_executable(_pid: i32) -> Result<()> {
    Ok(())
}

async fn wait_for_exit(config: &Config, pid: Option<i32>, deadline: Instant) -> Result<()> {
    while Instant::now() < deadline {
        let connection = UnixStream::connect(&config.rpc_socket()).await;
        let socket_gone = !config.rpc_socket().exists() && connection.is_err();
        let process_gone = pid.is_none_or(|pid| {
            let result = unsafe { libc::kill(pid, 0) };
            result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        });
        if socket_gone && process_gone {
            return Ok(());
        }
        if pid.is_some() && process_gone && connection.is_err() {
            let _ = fs::remove_file(config.rpc_socket());
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    bail!("daemon shutdown timed out")
}

fn spawn_daemon(config: &Config) -> Result<()> {
    let executable = env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(tmux_socket) = &config.tmux_socket {
        command.env("AGENT_TALK_TMUX_SOCKET", tmux_socket);
    }
    // 解決済みの backend 構成をそのまま daemon へ渡す。渡さないと daemon 側で
    // 再探索して、起動元とは別の backend 構成になり得る。
    command.env(
        "AGENT_TALK_HERDR_SOCKET",
        config.herdr_socket.as_deref().unwrap_or(Path::new("")),
    );
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().context("cannot spawn agent-talk daemon")?;
    Ok(())
}

async fn wait_for_expected_status(config: &Config, deadline: Instant) -> Result<DaemonStatus> {
    while Instant::now() < deadline {
        if let Ok(Probe::Current(status)) = probe(config).await
            && status.ready
            && status.version == env!("CARGO_PKG_VERSION")
        {
            return Ok(status);
        }
        sleep(Duration::from_millis(50)).await;
    }
    bail!("daemon startup timed out")
}

async fn request_once(config: &Config, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(&config.rpc_socket()).await?;
    let credentials = stream.peer_cred()?;
    if credentials.uid() != unsafe { libc::geteuid() } {
        bail!("daemon socket peer uid mismatch");
    }
    write_request(&mut stream, request).await?;
    read_response(&mut stream).await
}

async fn write_request(stream: &mut UnixStream, request: &Request) -> Result<()> {
    stream.write_all(&serde_json::to_vec(request)?).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

async fn read_response(stream: &mut UnixStream) -> Result<Response> {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    if line.is_empty() {
        bail!("daemon closed the connection");
    }
    Ok(serde_json::from_str(&line)?)
}

fn internal_request(command: &str) -> Request {
    Request {
        command: command.into(),
        args: Vec::new(),
        stdin: String::new(),
        pane: crate::backend::self_pane(),
        send_options: None,
    }
}

pub fn executable_matches_version(path: &Path, expected: &str) -> Result<()> {
    let output = Command::new(path).arg("--version").output()?;
    if !output.status.success() {
        bail!("updated binary --version failed");
    }
    let actual = String::from_utf8(output.stdout)?;
    if actual.trim() != format!("agent-talk {expected}") {
        bail!(
            "updated binary version mismatch: expected {expected}, got {}",
            actual.trim()
        );
    }
    Ok(())
}
