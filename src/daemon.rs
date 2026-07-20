use std::{
    env,
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};
use tracing::{error, info, warn};

use crate::{
    config::Config,
    journal::{Journal, Record},
    protocol::{Request, Response},
    state::{AgentState, BrokerState, Dispatch, Message},
    tmux::{ControlEvent, PaneInfo, Tmux},
};

enum Event {
    Request(Request, oneshot::Sender<Response>),
    PaneExited(String),
    ControlDisconnected,
}

pub async fn run(config: Config) -> Result<()> {
    init_logging(&config.log, &config.log_level)?;
    if let Some(parent) = config.rpc_socket.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if config.rpc_socket.exists() {
        match UnixStream::connect(&config.rpc_socket).await {
            Ok(_) => return Ok(()),
            Err(_) => fs::remove_file(&config.rpc_socket)
                .with_context(|| format!("cannot remove {}", config.rpc_socket.display()))?,
        }
    }
    let listener = UnixListener::bind(&config.rpc_socket)
        .with_context(|| format!("cannot bind {}", config.rpc_socket.display()))?;

    let tmux = Tmux::new(config.tmux_socket.clone());
    let executable = env::current_exe()?;
    tmux.install_pane_exit_hook(&executable).await?;
    let (control_tx, mut control_rx) = mpsc::channel(32);
    let mut control = tmux.start_control(control_tx).await?;
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    spawn_accept_loop(listener, tx.clone());
    let event_tx = tx.clone();
    tokio::spawn(async move {
        while let Some(event) = control_rx.recv().await {
            let mapped = match event {
                ControlEvent::PaneExited(pane) => Event::PaneExited(pane),
                ControlEvent::Disconnected => Event::ControlDisconnected,
            };
            if event_tx.send(mapped).await.is_err() {
                break;
            }
        }
    });

    let (journal, state) = Journal::open(config.journal.clone())?;
    let mut broker = Broker {
        state,
        journal,
        tmux: tmux.clone(),
        config,
    };
    broker.reconcile(true).await;
    info!(source = "daemon", "started");

    while let Some(event) = rx.recv().await {
        match event {
            Event::Request(request, reply) => {
                let response = broker.handle(request).await;
                let _ = reply.send(response);
            }
            Event::PaneExited(pane) => {
                broker.remove_agent(&pane, "宛先が退出した").await;
            }
            Event::ControlDisconnected => break,
        }
    }

    info!(
        source = "tmux-control",
        "stopping after control-mode disconnect"
    );
    tmux.remove_pane_exit_hook().await;
    let _ = control.kill().await;
    let _ = fs::remove_file(&broker.config.rpc_socket);
    Ok(())
}

fn spawn_accept_loop(listener: UnixListener, tx: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_client(stream, tx).await {
                    warn!(%error, "client request failed");
                }
            });
        }
    });
}

async fn serve_client(stream: UnixStream, tx: mpsc::Sender<Event>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).await?;
    let request: Request = serde_json::from_str(&line)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(Event::Request(request, reply_tx)).await?;
    let response = reply_rx.await?;
    let encoded = serde_json::to_vec(&response)?;
    writer.write_all(&encoded).await?;
    writer.write_all(b"\n").await?;
    Ok(())
}

struct Broker {
    state: BrokerState,
    journal: Journal,
    tmux: Tmux,
    config: Config,
}

impl Broker {
    async fn handle(&mut self, request: Request) -> Response {
        let result = match request.command.as_str() {
            "register" => self.register(request).await,
            "unregister" => self.unregister(request).await,
            "busy" => {
                self.change_state(request.pane, AgentState::Busy, "hook")
                    .await
            }
            "idle" => {
                self.change_state(request.pane, AgentState::Idle, "hook")
                    .await
            }
            "turn-end" => self.turn_end(request.pane).await,
            "who" => self.who().await,
            "resolve" => self.resolve_command(request).await,
            "send" => self.send(request).await,
            "gc" | "watch" => Ok(Response::ok("")),
            "internal-pane-exited" => {
                if let Some(pane) = request.args.first() {
                    self.remove_agent(pane, "宛先が退出した").await;
                }
                Ok(Response::ok(""))
            }
            "internal-reconcile" => {
                self.reconcile(false).await;
                Ok(Response::ok(""))
            }
            _ => Ok(Response::error("unknown command")),
        };
        if let Err(error) = self.journal.checkpoint_if_needed(&self.state) {
            warn!(%error, "journal checkpoint failed");
        }
        match result {
            Ok(response) => response,
            Err(error) => {
                error!(%error, "request failed");
                Response::error(error.to_string())
            }
        }
    }

    async fn register(&mut self, request: Request) -> Result<Response> {
        let Some(pane) = request.pane else {
            return Ok(Response::ok(""));
        };
        let Some(name) = request.args.first() else {
            return Ok(Response::error("usage: agent-talk register <name>"));
        };
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            || name.is_empty()
        {
            return Ok(Response::error(format!(
                "name は英数字と - _ のみ: '{name}'"
            )));
        }
        let displaced = self
            .state
            .agents
            .get(&pane)
            .map(|agent| agent.queue.values().cloned().collect())
            .unwrap_or_default();
        if !self
            .notify_failures(displaced, "宛先エージェントが入れ替わった", Some(&pane))
            .await
        {
            return Ok(Response::error(
                "前の宛先への配達失敗通知を永続化できないため登録を中止しました",
            ));
        }
        self.tmux.set_option(&pane, "@agent", Some(name)).await?;
        self.tmux
            .set_option(&pane, "@agent_state", Some("idle"))
            .await?;
        self.journal.append(&Record::Register {
            pane: pane.clone(),
            name: name.clone(),
            state: AgentState::Idle,
        })?;
        self.state.register(pane.clone(), name.clone());
        info!(%pane, %name, source = "register", "registered");
        Ok(Response::ok(""))
    }

    async fn unregister(&mut self, request: Request) -> Result<Response> {
        let Some(pane) = request.pane else {
            return Ok(Response::ok(""));
        };
        if self.remove_agent(&pane, "宛先エージェントが退出した").await {
            if let Err(error) = self.tmux.set_option(&pane, "@agent", None).await {
                warn!(%pane, %error, source = "unregister", "agent mirror removal failed");
            }
            if let Err(error) = self.tmux.set_option(&pane, "@agent_state", None).await {
                warn!(%pane, %error, source = "unregister", "state mirror removal failed");
            }
        } else {
            return Ok(Response::error(
                "配達失敗通知を永続化できないため登録解除を中止しました",
            ));
        }
        Ok(Response::ok(""))
    }

    async fn change_state(
        &mut self,
        pane: Option<String>,
        state: AgentState,
        source: &'static str,
    ) -> Result<Response> {
        let Some(pane) = pane else {
            return Ok(Response::ok(""));
        };
        if let Err(error) = self
            .tmux
            .set_option(&pane, "@agent_state", Some(state.as_str()))
            .await
        {
            warn!(%pane, %error, source, "pane state mirror failed");
        }
        self.journal.append(&Record::State {
            pane: pane.clone(),
            state,
        })?;
        self.state.set_state(&pane, state);
        info!(%pane, ?state, source, "state changed");
        Ok(Response::ok(""))
    }

    async fn turn_end(&mut self, pane: Option<String>) -> Result<Response> {
        let Some(pane) = pane else {
            return Ok(Response::ok(""));
        };
        if let Err(error) = self
            .tmux
            .set_option(&pane, "@agent_state", Some("idle"))
            .await
        {
            warn!(%pane, %error, source = "turn-end", "pane state mirror failed");
        }
        self.journal.append(&Record::State {
            pane: pane.clone(),
            state: AgentState::Idle,
        })?;
        let message = self.state.turn_end(&pane);
        info!(%pane, source = "turn-end", queued = message.is_some(), "turn ended");
        if let Some(message) = message {
            if let Err(error) = self.journal.append(&Record::State {
                pane: pane.clone(),
                state: AgentState::Busy,
            }) {
                self.state.requeue_after_delivery_failure(&pane, message);
                return Ok(Response::error(format!(
                    "配達状態を journal に書き込めません: {error}"
                )));
            }
            if self.tmux.deliver(&pane, &message.bell).await.is_ok() {
                self.journal.append(&Record::Complete {
                    pane: pane.clone(),
                    id: message.id,
                })?;
                info!(%pane, id = message.id, source = "turn-end", "delivered");
            } else {
                self.state.requeue_after_delivery_failure(&pane, message);
                self.journal.append(&Record::State {
                    pane,
                    state: AgentState::Idle,
                })?;
            }
        } else {
            self.state.set_state(&pane, AgentState::Idle);
        }
        Ok(Response::ok(""))
    }

    async fn who(&self) -> Result<Response> {
        let panes = self.tmux.panes().await?;
        let mut output = String::new();
        for pane in panes {
            if let Some(agent) = self.state.agents.get(&pane.pane_id) {
                output.push_str(&format!(
                    "{:<10} {:<5} {}:{}.{} ({})  {}\n",
                    agent.name,
                    agent.state.as_str(),
                    pane.session,
                    pane.window_index,
                    pane.pane_index,
                    pane.pane_id,
                    pane.cwd
                ));
            }
        }
        Ok(Response::ok(output))
    }

    async fn resolve_command(&self, request: Request) -> Result<Response> {
        let Some(addr) = request.args.first() else {
            return Ok(Response::error(
                "usage: agent-talk resolve [scope/]<name> | %pane",
            ));
        };
        match self.resolve(addr, request.pane.as_deref()).await {
            Ok((pane, _)) => Ok(Response::ok(format!("{pane}\n"))),
            Err(response) => Ok(response),
        }
    }

    async fn send(&mut self, request: Request) -> Result<Response> {
        let Some(addr) = request.args.first() else {
            return Ok(Response::error(
                "usage: agent-talk send [scope/]<name> [message]",
            ));
        };
        let (pane, expected) = match self.resolve(addr, request.pane.as_deref()).await {
            Ok(hit) => hit,
            Err(response) => return Ok(response),
        };
        let body = if request.args.len() > 1 {
            request.args[1..].join(" ")
        } else {
            request.stdin
        };
        if body.is_empty() {
            return Ok(Response::error("本文が空です"));
        }

        let panes = self.tmux.panes().await?;
        let from_pane = request.pane.as_deref();
        let from_agent = from_pane
            .and_then(|pane| self.state.agents.get(pane))
            .map(|agent| agent.name.clone())
            .unwrap_or_else(|| "human".into());
        let from_info = from_pane.and_then(|id| panes.iter().find(|p| p.pane_id == id));
        let brief = self.write_brief(addr, &expected, &from_agent, from_info, &body)?;
        let bell = format!(
            "[agent-talk] {from_agent} から依頼が届きました。{} を読んで対応してください。",
            brief.display()
        );
        if from_agent != "human"
            && let Some(from_pane) = from_pane
        {
            self.tmux.mark_talk_sent(from_pane).await;
        }
        let sender = from_pane.unwrap_or("human").to_owned();
        let dispatch =
            self.state
                .dispatch(&pane, sender, brief.display().to_string(), bell, &expected);
        match dispatch {
            Ok(Dispatch::Deliver(message)) => {
                if let Err(error) = self.journal.append(&Record::State {
                    pane: pane.clone(),
                    state: AgentState::Busy,
                }) {
                    self.state.set_state(&pane, AgentState::Idle);
                    return Ok(Response::error(format!(
                        "配達状態を書き込めず配達できません (依頼書は {} に残っています): {error}",
                        brief.display()
                    )));
                }
                if self.tmux.deliver(&pane, &message.bell).await.is_ok() {
                    info!(%pane, id = message.id, source = "send", "delivered");
                    Ok(Response::ok(format!(
                        "sent -> {pane} ({addr}): {}\n",
                        brief.display()
                    )))
                } else {
                    let target_is_live = self
                        .tmux
                        .panes()
                        .await
                        .is_ok_and(|panes| panes.iter().any(|item| item.pane_id == pane));
                    if !target_is_live {
                        self.state.set_state(&pane, AgentState::Idle);
                        self.remove_agent(&pane, "宛先が退出した").await;
                        return Ok(Response::error(format!(
                            "宛先 {pane} ({addr}) は退出済みです。依頼書は {} に残っています",
                            brief.display()
                        )));
                    }
                    let id = message.id;
                    self.state.requeue_after_delivery_failure(&pane, message);
                    if let Err(error) = self.journal.append(&Record::State {
                        pane: pane.clone(),
                        state: AgentState::Idle,
                    }) {
                        self.state.remove_message(&pane, id);
                        return Ok(Response::error(format!(
                            "配達状態を書き込めず配達できません (依頼書は {} に残っています): {error}",
                            brief.display()
                        )));
                    }
                    let message = self.state.agents[&pane].queue[&id].clone();
                    if let Err(error) = self.journal.append(&Record::Enqueue {
                        pane: pane.clone(),
                        message,
                    }) {
                        self.state.remove_message(&pane, id);
                        return Ok(Response::error(format!(
                            "キューへ書き込めず配達できません (依頼書は {} に残っています): {error}",
                            brief.display()
                        )));
                    }
                    Ok(Response::ok(format!(
                        "queued (busy) -> {pane} ({addr}): {}\n",
                        brief.display()
                    )))
                }
            }
            Ok(Dispatch::Queued(id)) => {
                if self.state.agents[&pane].queue.len() > self.config.queue_limit {
                    self.state.remove_message(&pane, id);
                    return Ok(Response::error(format!(
                        "宛先 {pane} のキュー保持上限 ({}) を超えました (依頼書は {} に残っています)",
                        self.config.queue_limit,
                        brief.display()
                    )));
                }
                let message = self.state.agents[&pane].queue[&id].clone();
                if let Err(error) = self.journal.append(&Record::Enqueue {
                    pane: pane.clone(),
                    message,
                }) {
                    self.state.remove_message(&pane, id);
                    return Ok(Response::error(format!(
                        "キューへ書き込めず配達できません (依頼書は {} に残っています): {error}",
                        brief.display()
                    )));
                }
                info!(%pane, id, source = "send", "queued");
                Ok(Response::ok(format!(
                    "queued (busy) -> {pane} ({addr}): {}\n",
                    brief.display()
                )))
            }
            Err(_) => Ok(Response::error(format!(
                "宛先 {pane} ({addr}) は退出済みです。依頼書は {} に残っています",
                brief.display()
            ))),
        }
    }

    async fn resolve(
        &self,
        addr: &str,
        self_pane: Option<&str>,
    ) -> std::result::Result<(String, String), Response> {
        let panes = self.tmux.panes().await.map_err(|error| {
            Response::error(format!(
                "tmux サーバーに接続できません (sandbox 内なら承認付きで再実行): {error}"
            ))
        })?;
        if addr.starts_with('%') && addr[1..].chars().all(|c| c.is_ascii_digit()) {
            return self
                .state
                .agents
                .get(addr)
                .map(|agent| (addr.to_owned(), agent.name.clone()))
                .ok_or_else(|| {
                    Response::error(format!("pane {addr} は @agent 登録されていません"))
                });
        }
        let (scope, name) = addr
            .split_once('/')
            .map_or((None, addr), |(scope, name)| (Some(scope), name));
        let mut candidates: Vec<_> = panes
            .iter()
            .filter(|pane| Some(pane.pane_id.as_str()) != self_pane)
            .filter_map(|pane| {
                self.state
                    .agents
                    .get(&pane.pane_id)
                    .filter(|agent| agent.name == name)
                    .map(|agent| (pane, agent))
            })
            .collect();
        if let Some(scope) = scope {
            candidates.retain(|(pane, _)| {
                pane.session == scope || Path::new(&pane.cwd).file_name() == Some(OsStr::new(scope))
            });
        } else if let Some(self_pane) = self_pane
            && let Some(origin) = panes.iter().find(|pane| pane.pane_id == self_pane)
        {
            let same_window: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|(pane, _)| pane.window_id == origin.window_id)
                .collect();
            if !same_window.is_empty() {
                candidates = same_window;
            } else {
                candidates.retain(|(pane, _)| pane.session == origin.session);
            }
        }
        match candidates.as_slice() {
            [(pane, agent)] => Ok((pane.pane_id.clone(), agent.name.clone())),
            [] => {
                let mut stderr =
                    format!("agent-talk: 宛先 '{addr}' がスコープ内に見つかりません。待受中:\n");
                stderr.push_str(&self.pretty_agents(&panes));
                Err(Response {
                    code: 1,
                    stdout: String::new(),
                    stderr,
                })
            }
            _ => {
                let mut stderr = format!(
                    "agent-talk: 宛先 '{addr}' の候補が複数あります。<scope>/<name> で指定してください:\n"
                );
                for (pane, agent) in candidates {
                    stderr.push_str(&pretty(agent.name.as_str(), agent.state, pane));
                }
                Err(Response {
                    code: 1,
                    stdout: String::new(),
                    stderr,
                })
            }
        }
    }

    fn pretty_agents(&self, panes: &[PaneInfo]) -> String {
        let mut output = String::new();
        for pane in panes {
            if let Some(agent) = self.state.agents.get(&pane.pane_id) {
                output.push_str(&pretty(&agent.name, agent.state, pane));
            }
        }
        output
    }

    fn write_brief(
        &self,
        addr: &str,
        expected: &str,
        from: &str,
        info: Option<&PaneInfo>,
        body: &str,
    ) -> Result<PathBuf> {
        let maildir_exists = self.config.maildir.exists();
        fs::create_dir_all(&self.config.maildir)?;
        if !maildir_exists {
            fs::set_permissions(&self.config.maildir, fs::Permissions::from_mode(0o700))?;
        }
        let from_file = safe_name(from, "human");
        let to_file = safe_name(expected, "agent");
        let now = local_timestamp()?;
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.subsec_nanos();
        for suffix in 0..1000_u16 {
            let path = self.config.maildir.join(format!(
                "{now}-{from_file}-to-{to_file}-{:06x}.md",
                nonce.wrapping_add(u32::from(suffix)) & 0x00ff_ffff
            ));
            let mut file = match OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            let origin = info
                .map(|pane| format!(" (session: {}, pane: {})", pane.session, pane.pane_id))
                .unwrap_or_default();
            let reply = if from != "human" {
                info.map_or(
                    "不要 (人間からの依頼。結果は自分の画面に表示すれば読まれる)"
                        .to_owned(),
                    |pane| {
                        format!(
                            "agent-talk send '{}' に返信本文を stdin で渡す (pane ID 指定は曖昧にならない)",
                            pane.pane_id
                        )
                    },
                )
            } else {
                "不要 (人間からの依頼。結果は自分の画面に表示すれば読まれる)".to_owned()
            };
            write!(
                file,
                "# agent-talk 依頼書\n- from: {from}{origin}\n- to: {addr}\n- reply: {reply}\n\n{body}\n"
            )?;
            file.sync_all()?;
            return Ok(path);
        }
        bail!("依頼書ファイルを作成できません")
    }

    async fn remove_agent(&mut self, pane: &str, reason: &str) -> bool {
        let Some(agent) = self.state.agents.get(pane) else {
            return true;
        };
        let messages: Vec<_> = agent.queue.values().cloned().collect();
        if !self.notify_failures(messages, reason, Some(pane)).await {
            error!(%pane, "failure notifications remain pending in journal");
            return false;
        }
        if let Err(error) = self.journal.append(&Record::Remove {
            pane: pane.to_owned(),
        }) {
            error!(%pane, %error, "cannot journal agent removal");
            return false;
        }
        let removed = self.state.remove(pane);
        info!(%pane, source = "pane-exited", queued = removed.len(), "removed");
        true
    }

    async fn notify_failures(
        &mut self,
        messages: Vec<Message>,
        reason: &str,
        excluded_pane: Option<&str>,
    ) -> bool {
        let panes = self.tmux.panes().await.unwrap_or_default();
        for original in messages {
            if !original.sender.starts_with('%') {
                continue;
            }
            if Some(original.sender.as_str()) == excluded_pane {
                continue;
            }
            let Some(sender_agent) = self.state.agents.get(&original.sender) else {
                continue;
            };
            let expected = sender_agent.name.clone();
            let sender_is_current = panes.iter().any(|pane| {
                pane.pane_id == original.sender && pane.agent.as_deref() == Some(expected.as_str())
            });
            if !sender_is_current {
                continue;
            }
            let bell = format!(
                "[agent-talk] 配達失敗: {} は {} ため配達されませんでした。必要なら宛先を確認して送り直してください。",
                original.brief, reason
            );
            let dispatch = self.state.dispatch(
                &original.sender,
                "system".into(),
                original.brief.clone(),
                bell,
                &expected,
            );
            match dispatch {
                Ok(Dispatch::Deliver(message)) => {
                    if let Err(error) = self.journal.append(&Record::State {
                        pane: original.sender.clone(),
                        state: AgentState::Busy,
                    }) {
                        self.state.set_state(&original.sender, AgentState::Idle);
                        error!(%error, "cannot persist failure notification state");
                        return false;
                    }
                    if self
                        .tmux
                        .deliver(&original.sender, &message.bell)
                        .await
                        .is_err()
                    {
                        self.state.set_state(&original.sender, AgentState::Busy);
                        let queued = self.state.dispatch(
                            &original.sender,
                            message.sender,
                            message.brief,
                            message.bell,
                            &expected,
                        );
                        let Ok(Dispatch::Queued(id)) = queued else {
                            return false;
                        };
                        let message = self.state.agents[&original.sender].queue[&id].clone();
                        if let Err(error) = self.journal.append(&Record::Enqueue {
                            pane: original.sender.clone(),
                            message,
                        }) {
                            self.state.remove_message(&original.sender, id);
                            error!(%error, "cannot persist failure notification");
                            return false;
                        }
                    }
                }
                Ok(Dispatch::Queued(id)) => {
                    let message = self.state.agents[&original.sender].queue[&id].clone();
                    if let Err(error) = self.journal.append(&Record::Enqueue {
                        pane: original.sender.clone(),
                        message,
                    }) {
                        self.state.remove_message(&original.sender, id);
                        error!(%error, "cannot persist failure notification");
                        return false;
                    }
                }
                Err(_) => {}
            }
        }
        true
    }

    async fn reconcile(&mut self, startup: bool) {
        let Ok(panes) = self.tmux.panes().await else {
            return;
        };
        let stale: Vec<_> = self
            .state
            .agents
            .iter()
            .filter(|(pane_id, agent)| {
                !panes.iter().any(|pane| {
                    pane.pane_id == **pane_id && pane.agent.as_deref() == Some(agent.name.as_str())
                })
            })
            .map(|(pane, _)| pane.clone())
            .collect();
        for pane in stale {
            self.remove_agent(&pane, "宛先が不在の").await;
        }
        if !startup {
            return;
        }
        for pane in &panes {
            if self.state.agents.contains_key(&pane.pane_id) {
                continue;
            }
            let Some(name) = pane.agent.as_ref() else {
                continue;
            };
            let state = match pane.agent_state.as_deref() {
                Some("busy") => AgentState::Busy,
                _ => AgentState::Idle,
            };
            if self
                .journal
                .append(&Record::Register {
                    pane: pane.pane_id.clone(),
                    name: name.clone(),
                    state,
                })
                .is_ok()
            {
                self.state
                    .restore_agent(pane.pane_id.clone(), name.clone(), state);
                info!(
                    pane = %pane.pane_id,
                    %name,
                    ?state,
                    source = "startup-mirror",
                    "registration recovered"
                );
            }
        }
        for pane in panes {
            let Some(agent) = self.state.agents.get(&pane.pane_id) else {
                continue;
            };
            let hinted_state = match pane.agent_state.as_deref() {
                Some("busy") => AgentState::Busy,
                Some("idle") => AgentState::Idle,
                _ => continue,
            };
            if agent.state != hinted_state
                && self
                    .journal
                    .append(&Record::State {
                        pane: pane.pane_id.clone(),
                        state: hinted_state,
                    })
                    .is_ok()
            {
                self.state.set_state(&pane.pane_id, hinted_state);
                info!(
                    pane = %pane.pane_id,
                    ?hinted_state,
                    source = "startup-mirror",
                    "state recovered"
                );
            }
        }
    }
}

fn pretty(name: &str, state: AgentState, pane: &PaneInfo) -> String {
    format!(
        "{name:<10} {:<5} {}:{}.{} ({})  {}\n",
        state.as_str(),
        pane.session,
        pane.window_index,
        pane.pane_index,
        pane.pane_id,
        pane.cwd
    )
}

fn safe_name(input: &str, fallback: &str) -> String {
    let name: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if name.is_empty() {
        fallback.into()
    } else {
        name
    }
}

fn local_timestamp() -> Result<String> {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&now, &mut local) }.is_null() {
        bail!("現在時刻を取得できません");
    }
    Ok(format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec
    ))
}

fn init_logging(path: &Path, configured_level: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        let parent_exists = parent.exists();
        fs::create_dir_all(parent)?;
        if !parent_exists {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let file = Arc::new(file);
    let writer = move || file.try_clone().expect("log file clone failed");
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(configured_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(writer)
        .try_init()
        .ok();
    Ok(())
}
