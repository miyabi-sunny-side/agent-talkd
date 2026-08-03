use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::{
    process::Command,
    time::{Duration, sleep},
};

use crate::backend::{BackendKind, PaneInfo};

#[derive(Clone)]
pub struct Tmux {
    socket: String,
    /// test 専用の代役。`Some` のとき tmux subprocess を一切起動せず、
    /// scripted な pane 一覧を返し、その他の操作は成功したものとして扱う。
    /// production build にはこのフィールド自体が存在しない。
    #[cfg(test)]
    scripted: Option<Vec<PaneInfo>>,
    /// scripted モードで `deliver` された (pane, bell) の記録 (test 専用)。
    /// clone 間で共有されるので、broker へ渡した後からでも観測できる。
    #[cfg(test)]
    pub(crate) delivered: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

impl Tmux {
    pub fn new(socket: String) -> Self {
        Self {
            socket,
            #[cfg(test)]
            scripted: None,
            #[cfg(test)]
            delivered: std::sync::Arc::default(),
        }
    }

    /// tmux を起動せずに固定の pane 一覧を返す代役 (test 専用)。
    #[cfg(test)]
    pub fn scripted(panes: Vec<PaneInfo>) -> Self {
        Self {
            socket: String::new(),
            scripted: Some(panes),
            delivered: std::sync::Arc::default(),
        }
    }

    pub async fn panes(&self) -> Result<Vec<PaneInfo>> {
        #[cfg(test)]
        if let Some(panes) = &self.scripted {
            return Ok(panes.clone());
        }
        let format = "#{session_name}\t#{window_id}\t#{pane_id}\t#{pane_current_path}\t#{window_index}\t#{pane_index}\t#{@agent}";
        let output = self.run(["list-panes", "-a", "-F", format]).await?;
        Ok(output
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split('\t').collect();
                (fields.len() == 7).then(|| PaneInfo {
                    session: fields[0].into(),
                    window_id: fields[1].into(),
                    pane_id: fields[2].into(),
                    cwd: fields[3].into(),
                    window_index: fields[4].into(),
                    pane_index: fields[5].into(),
                    agent: (!fields[6].is_empty()).then(|| fields[6].into()),
                    backend: BackendKind::Tmux,
                    status: None,
                })
            })
            .collect())
    }

    pub async fn set_option(&self, pane: &str, key: &str, value: Option<&str>) -> Result<()> {
        let mut args = vec!["set-option", "-p", "-t", pane];
        if value.is_none() {
            args.push("-u");
        }
        args.push(key);
        if let Some(value) = value {
            args.push(value);
        }
        self.run(args).await.map(|_| ())
    }

    pub async fn deliver(&self, pane: &str, bell: &str) -> Result<()> {
        #[cfg(test)]
        if self.scripted.is_some() {
            self.delivered
                .lock()
                .unwrap()
                .push((pane.to_owned(), bell.to_owned()));
            return Ok(());
        }
        self.set_option(pane, "@agent_state", Some("busy")).await?;
        if let Err(error) = self.run(["send-keys", "-t", pane, "-l", bell]).await {
            let _ = self.set_option(pane, "@agent_state", Some("idle")).await;
            return Err(error);
        }
        sleep(Duration::from_millis(300)).await;
        if let Err(error) = self.run(["send-keys", "-t", pane, "Enter"]).await {
            let _ = self.set_option(pane, "@agent_state", Some("idle")).await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn capture_pane(&self, pane: &str) -> Result<String> {
        let output = self.run(["capture-pane", "-p", "-t", pane]).await?;
        if output.len() > MAX_CAPTURE_BYTES {
            bail!("captured pane exceeds {MAX_CAPTURE_BYTES} bytes");
        }
        Ok(output)
    }

    pub async fn mark_talk_sent(&self, pane: &str) {
        let _ = self.set_option(pane, "@agent_talk_sent", Some("1")).await;
    }

    pub async fn install_pane_exit_hook(&self, executable: &Path, rpc_socket: &Path) -> Result<()> {
        self.run([
            "set-environment",
            "-g",
            "AGENT_TALK_TMUX_SOCKET",
            &self.socket,
        ])
        .await?;
        self.run([
            "set-environment",
            "-g",
            "AGENT_TALK_RPC_SOCKET",
            &rpc_socket.to_string_lossy(),
        ])
        .await?;
        let command = format!(
            "run-shell -b -d 0.5 '{} internal-reconcile'",
            shell_quote(&executable.to_string_lossy())
        );
        for hook in [
            "pane-exited[987]",
            "after-kill-pane[987]",
            "window-unlinked[987]",
            "session-closed[987]",
        ] {
            self.run(["set-hook", "-g", hook, &command]).await?;
        }
        Ok(())
    }

    pub async fn remove_pane_exit_hook(&self) {
        for hook in [
            "pane-exited[987]",
            "after-kill-pane[987]",
            "window-unlinked[987]",
            "session-closed[987]",
        ] {
            let _ = self.run(["set-hook", "-gu", hook]).await;
        }
    }

    pub async fn server_pid(&self) -> Result<u32> {
        self.run(["display-message", "-p", "#{pid}"])
            .await?
            .trim()
            .parse()
            .context("invalid tmux server pid")
    }

    async fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        #[cfg(test)]
        if self.scripted.is_some() {
            return Ok(String::new());
        }
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .await
            .context("cannot execute tmux")?;
        if !output.status.success() {
            bail!(
                "{}",
                String::from_utf8_lossy(&output.stderr).trim().to_owned()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn shell_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}
