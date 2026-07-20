use std::{path::Path, process::Stdio};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
    time::{Duration, sleep},
};

#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub session: String,
    pub window_id: String,
    pub pane_id: String,
    pub cwd: String,
    pub window_index: String,
    pub pane_index: String,
    pub agent: Option<String>,
    pub agent_state: Option<String>,
}

#[derive(Debug)]
pub enum ControlEvent {
    PaneExited(String),
    Disconnected,
}

#[derive(Clone)]
pub struct Tmux {
    socket: String,
}

impl Tmux {
    pub fn new(socket: String) -> Self {
        Self { socket }
    }

    pub async fn panes(&self) -> Result<Vec<PaneInfo>> {
        let format = "#{session_name}\t#{window_id}\t#{pane_id}\t#{pane_current_path}\t#{window_index}\t#{pane_index}\t#{@agent}\t#{@agent_state}";
        let output = self.run(["list-panes", "-a", "-F", format]).await?;
        Ok(output
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split('\t').collect();
                (fields.len() == 8).then(|| PaneInfo {
                    session: fields[0].into(),
                    window_id: fields[1].into(),
                    pane_id: fields[2].into(),
                    cwd: fields[3].into(),
                    window_index: fields[4].into(),
                    pane_index: fields[5].into(),
                    agent: (!fields[6].is_empty()).then(|| fields[6].into()),
                    agent_state: (!fields[7].is_empty()).then(|| fields[7].into()),
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

    pub async fn mark_talk_sent(&self, pane: &str) {
        let _ = self.set_option(pane, "@agent_talk_sent", Some("1")).await;
    }

    pub async fn install_pane_exit_hook(&self, executable: &Path) -> Result<()> {
        self.run([
            "set-environment",
            "-g",
            "AGENT_TALK_TMUX_SOCKET",
            &self.socket,
        ])
        .await?;
        let command = format!(
            "run-shell -b '{} internal-reconcile'",
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

    pub async fn start_control(&self, tx: mpsc::Sender<ControlEvent>) -> Result<Child> {
        self.run(["list-sessions"]).await?;
        let mut child = Command::new("tmux")
            .args([
                "-C",
                "-S",
                &self.socket,
                "new-session",
                "-A",
                "-s",
                "_agent_talkd",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("cannot start tmux control mode")?;
        let stdout = child.stdout.take().context("tmux control stdout missing")?;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // tmux 3.6b does not emit this for panes outside the control
                // session. Keep it as a forward-compatible fast path; global
                // hooks trigger a full reconciliation on current releases.
                if let Some(rest) = line.strip_prefix("%pane-exited ") {
                    let pane = rest.split_whitespace().next().unwrap_or(rest);
                    let _ = tx.send(ControlEvent::PaneExited(pane.to_owned())).await;
                }
            }
            let _ = tx.send(ControlEvent::Disconnected).await;
        });
        Ok(child)
    }

    async fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
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

pub fn socket_name(socket: &str) -> String {
    Path::new(socket)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("default")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn shell_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}
