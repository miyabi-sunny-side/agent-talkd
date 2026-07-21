use std::{
    env,
    ffi::OsStr,
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};
use tracing::{error, info, warn};

use crate::{
    config::{Config, is_safe_token},
    journal::{Journal, Record},
    protocol::{Request, Response},
    state::{AgentState, BrokerState, Dispatch, ExternalMailboxEvent, MailboxDirection, Message},
    tmux::{ControlEvent, PaneInfo, Tmux},
};

const MAX_BODY_BYTES: usize = 1024 * 1024;

enum Event {
    Request {
        request: Request,
        reply: oneshot::Sender<Response>,
        flushed: oneshot::Receiver<()>,
    },
    PaneExited(String),
    ControlDisconnected,
    ServerCheck,
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
    tmux.install_pane_exit_hook(&executable, &config.rpc_socket)
        .await?;
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
    let health_tx = tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            if health_tx.send(Event::ServerCheck).await.is_err() {
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

    let mut control_lost = false;
    let mut shutdown_requested = false;
    while let Some(event) = rx.recv().await {
        match event {
            Event::Request {
                request,
                reply,
                flushed,
            } => {
                let shutdown = request.command == "internal-daemon-shutdown";
                let response = broker.handle(request).await;
                let _ = reply.send(response);
                if shutdown {
                    shutdown_requested = true;
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), flushed).await;
                    break;
                }
            }
            Event::PaneExited(pane) => {
                broker.remove_agent(&pane, "宛先が退出した").await;
            }
            Event::ControlDisconnected => {
                if tmux.server_is_alive().await {
                    control_lost = true;
                    warn!(
                        source = "tmux-control",
                        "control mode disconnected while server is alive; using health checks"
                    );
                } else {
                    break;
                }
            }
            Event::ServerCheck => {
                if control_lost && !tmux.server_is_alive().await {
                    break;
                }
            }
        }
    }

    if shutdown_requested {
        info!(source = "lifecycle", "stopping after shutdown request");
    } else {
        info!(
            source = "tmux-control",
            "stopping after control-mode disconnect"
        );
    }
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
    let (flushed_tx, flushed_rx) = oneshot::channel();
    tx.send(Event::Request {
        request,
        reply: reply_tx,
        flushed: flushed_rx,
    })
    .await?;
    let response = reply_rx.await?;
    let encoded = serde_json::to_vec(&response)?;
    writer.write_all(&encoded).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    writer.shutdown().await?;
    let _ = flushed_tx.send(());
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
            "send" if request.send_options.is_none() => self.send(request).await,
            "send" => Ok(Response::error(
                "send optionsにはsend-v2 protocolが必要です",
            )),
            "send-v2" if request.send_options.is_some() => self.send(request).await,
            "send-v2" => Ok(Response::error("send-v2 optionsがありません")),
            "read" => self.read(request),
            "reply" => self.reply(request),
            "mailbox-list-v1" => self.mailbox_list(request),
            "internal-daemon-status" => Ok(Response::ok(format!(
                "{}\n",
                serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "pid": std::process::id(),
                    "ready": true,
                })
            ))),
            "internal-daemon-shutdown" => Ok(Response::ok("")),
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
        if let Err(error) = self.journal.checkpoint_if_needed(&mut self.state) {
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
        let displaced = self.state.messages_for_target(&pane);
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
        let message_id = loop {
            let Some(id) = self.state.turn_end(&pane) else {
                break None;
            };
            if self.state.message(id).is_some() {
                break Some(id);
            }
            error!(%pane, id, "queued message body missing; skipping");
            if let Err(error) = self.journal.append(&Record::Complete {
                pane: pane.clone(),
                id,
            }) {
                return Ok(Response::error(format!(
                    "欠損メッセージのskipをjournalへ書き込めません: {error}"
                )));
            }
        };
        info!(%pane, source = "turn-end", queued = message_id.is_some(), "turn ended");
        if let Some(id) = message_id {
            if let Err(error) = self.journal.append(&Record::State {
                pane: pane.clone(),
                state: AgentState::Busy,
            }) {
                self.state.requeue_after_delivery_failure(&pane, id);
                return Ok(Response::error(format!(
                    "配達状態を journal に書き込めません: {error}"
                )));
            }
            let Some(stored) = self.state.message(id) else {
                error!(%pane, id, "message body disappeared before delivery");
                self.state.set_state(&pane, AgentState::Idle);
                return Ok(Response::error(format!(
                    "message #{id} の本文が見つからないため配達を中止しました"
                )));
            };
            let bell = stored.message.bell.clone();
            if self.tmux.deliver(&pane, &bell).await.is_ok() {
                self.journal.append(&Record::Complete {
                    pane: pane.clone(),
                    id,
                })?;
                self.state.complete_delivery(&pane, id);
                info!(%pane, id, source = "turn-end", "delivered");
            } else {
                self.state.requeue_after_delivery_failure(&pane, id);
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
                "usage: agent-talk send [scope/]<name> [--from <source>] [--skill <name>] [--] [message]",
            ));
        };
        let options = request.send_options.clone().unwrap_or_default();
        let external_source = options.from.clone();
        if let Some(skill) = options.skill.as_deref() {
            if !is_safe_token(skill) {
                return Ok(Response::error(format!(
                    "skill名は64文字以内の小文字英数字と : _ - のみです: '{skill}'"
                )));
            }
            if self
                .config
                .allowed_skills
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(skill))
            {
                return Ok(Response::error(format!(
                    "skill '{skill}' は @agent_talkd_allowed_skills で許可されていません"
                )));
            }
        }
        let registered_sender = request.pane.as_deref().and_then(|pane| {
            self.state
                .agents
                .get(pane)
                .map(|agent| (pane.to_owned(), agent.name.clone()))
        });
        if let Some(source) = options.from.as_deref() {
            if registered_sender.is_some() {
                return Ok(Response::error(
                    "登録agent paneから --from を上書きできません",
                ));
            }
            if !is_safe_token(source) {
                return Ok(Response::error(format!(
                    "送信元ラベルは64文字以内の小文字英数字と : _ - のみです: '{source}'"
                )));
            }
            if !self.config.allowed_sources.contains(source) {
                return Ok(Response::error(format!(
                    "送信元 '{source}' は @agent_talkd_allowed_sources で許可されていません"
                )));
            }
            if matches!(source, "human" | "system")
                || self.state.agents.values().any(|agent| agent.name == source)
            {
                return Ok(Response::error(format!(
                    "送信元 '{source}' は予約済みのため指定できません"
                )));
            }
        }
        let (pane, expected) = match self.resolve(addr, request.pane.as_deref()).await {
            Ok(hit) => hit,
            Err(response) => return Ok(response),
        };
        let skill_prefix = if let Some(skill) = options.skill.as_deref() {
            let Some(syntax) = self.config.skill_syntax.get(&expected) else {
                return Ok(Response::error(format!(
                    "agent '{expected}' のskill記法が @agent_talkd_skill_syntax にありません"
                )));
            };
            format!("{}{skill} ", syntax.prefix())
        } else {
            String::new()
        };
        let body = if request.args.len() > 1 {
            request.args[1..].join(" ")
        } else {
            request.stdin
        };
        if body.is_empty() {
            return Ok(Response::error("本文が空です"));
        }
        if body.len() > MAX_BODY_BYTES {
            return Ok(Response::error(format!(
                "本文がサイズ上限 ({} bytes) を超えています",
                MAX_BODY_BYTES
            )));
        }
        if self.state.is_busy(&pane) && self.state.queue_len(&pane) >= self.config.queue_limit {
            return Ok(Response::error(format!(
                "宛先 {pane} のキュー保持上限 ({}) を超えました",
                self.config.queue_limit
            )));
        }

        let panes = self.tmux.panes().await?;
        let from_info = request
            .pane
            .as_deref()
            .and_then(|id| panes.iter().find(|p| p.pane_id == id));
        let from_agent = registered_sender
            .as_ref()
            .map(|(_, name)| name.clone())
            .or(options.from.clone())
            .unwrap_or_else(|| "human".into());
        let reply_info = registered_sender.as_ref().and(from_info);
        let brief = build_brief(
            addr,
            &from_agent,
            from_info,
            reply_info,
            &body,
            external_source.is_some(),
            0,
        );
        if let Some((from_pane, _)) = registered_sender.as_ref() {
            self.tmux.mark_talk_sent(from_pane).await;
        }
        let sender = registered_sender
            .map(|(pane, _)| pane)
            .unwrap_or_else(|| "human".into());
        let dispatch = self.state.dispatch(
            &pane,
            sender,
            brief,
            &expected,
            |id| {
                format!(
                    "{skill_prefix}[agent-talk] {from_agent} から依頼が届きました。agent-talk read {id} で本文を確認して対応してください。"
                )
            },
        );
        match dispatch {
            Ok(dispatch) => {
                let id = match dispatch {
                    Dispatch::Deliver(id) | Dispatch::Queued(id) => id,
                };
                if external_source.is_some() {
                    self.state.set_brief(
                        id,
                        build_brief(addr, &from_agent, from_info, reply_info, &body, true, id),
                    );
                }
                let Some(stored) = self.state.message(id) else {
                    error!(%pane, id, "new message body missing before persistence");
                    if matches!(dispatch, Dispatch::Deliver(_)) {
                        self.state.set_state(&pane, AgentState::Idle);
                    }
                    self.state.discard_message(id);
                    return Ok(Response::error(format!(
                        "message #{id} の本文が見つからないため配達を中止しました"
                    )));
                };
                let message = stored.message.clone();
                let external_created_at = now_epoch();
                let external_event =
                    external_source
                        .as_deref()
                        .map(|source_label| ExternalMailboxEvent {
                            id,
                            created_at: external_created_at,
                            mailbox: source_label.to_owned(),
                            source_label: source_label.to_owned(),
                            direction: MailboxDirection::Out,
                            body: body.clone(),
                            skill: options.skill.clone(),
                            target_name: expected.clone(),
                            target_pane: pane.clone(),
                            reply_to: None,
                        });
                if let Some(event) = external_event.as_ref()
                    && let Err(error) = self.journal.append(&Record::ExternalMailbox {
                        event: event.clone(),
                    })
                {
                    if matches!(dispatch, Dispatch::Deliver(_)) {
                        self.state.set_state(&pane, AgentState::Idle);
                    }
                    self.state.discard_message(id);
                    return Ok(Response::error(format!(
                        "mailbox journal に書き込めず配達を続行できません: {error}"
                    )));
                }
                if let Some(event) = external_event {
                    self.state.add_mailbox_event(event);
                }
                if let Err(error) = self.journal.append(&Record::Enqueue {
                    pane: pane.clone(),
                    message,
                }) {
                    if matches!(dispatch, Dispatch::Deliver(_)) {
                        self.state.set_state(&pane, AgentState::Idle);
                    }
                    self.state.discard_message(id);
                    return Ok(Response::error(format!(
                        "本文を journal に書き込めず配達できません: {error}"
                    )));
                }
                if matches!(dispatch, Dispatch::Queued(_)) {
                    info!(%pane, id, source = "send", "queued");
                    return Ok(Response::ok(format!(
                        "queued (busy) -> {pane} ({addr}): #{id}\n"
                    )));
                }
                if let Err(error) = self.journal.append(&Record::State {
                    pane: pane.clone(),
                    state: AgentState::Busy,
                }) {
                    self.state.set_state(&pane, AgentState::Idle);
                    return Ok(Response::error(format!(
                        "配達状態を書き込めず配達できません (#{}): {error}",
                        id
                    )));
                }
                let Some(stored) = self.state.message(id) else {
                    error!(%pane, id, "persisted message body missing before delivery");
                    self.state.set_state(&pane, AgentState::Idle);
                    return Ok(Response::error(format!(
                        "message #{id} の本文が見つからないため配達を中止しました"
                    )));
                };
                let bell = stored.message.bell.clone();
                if self.tmux.deliver(&pane, &bell).await.is_ok() {
                    self.journal.append(&Record::Complete {
                        pane: pane.clone(),
                        id,
                    })?;
                    self.state.complete_delivery(&pane, id);
                    info!(%pane, id, source = "send", "delivered");
                    Ok(Response::ok(format!("sent -> {pane} ({addr}): #{id}\n")))
                } else {
                    let target_is_live = self
                        .tmux
                        .panes()
                        .await
                        .is_ok_and(|panes| panes.iter().any(|item| item.pane_id == pane));
                    if !target_is_live {
                        self.state.set_state(&pane, AgentState::Idle);
                        self.journal.append(&Record::Consumed { id })?;
                        self.state.consume(id);
                        self.remove_agent(&pane, "宛先が退出した").await;
                        return Ok(Response::error(format!(
                            "宛先 {pane} ({addr}) は退出済みです (message #{id})"
                        )));
                    }
                    self.state.requeue_after_delivery_failure(&pane, id);
                    if let Err(error) = self.journal.append(&Record::State {
                        pane: pane.clone(),
                        state: AgentState::Idle,
                    }) {
                        return Ok(Response::error(format!(
                            "配達状態を書き込めず配達できません (message #{id}): {error}"
                        )));
                    }
                    Ok(Response::ok(format!(
                        "queued (busy) -> {pane} ({addr}): #{id}\n"
                    )))
                }
            }
            Err(_) => Ok(Response::error(format!(
                "宛先 {pane} ({addr}) は退出済みです"
            ))),
        }
    }

    fn read(&mut self, request: Request) -> Result<Response> {
        let Some(raw_id) = request.args.first() else {
            return Ok(Response::error("usage: agent-talk read <id>"));
        };
        let Ok(id) = raw_id.trim_start_matches('#').parse::<u64>() else {
            return Ok(Response::error(format!("message id が不正です: {raw_id}")));
        };
        let Some(pane) = request.pane.as_deref() else {
            return Ok(Response::error(
                "read は登録済みのtmux pane内で実行してください",
            ));
        };
        let Some(stored) = self.state.message(id) else {
            return Ok(Response::error(format!(
                "message #{id} は見つかりません (checkpoint 済みの可能性があります)"
            )));
        };
        let current_name = self.state.agents.get(pane).map(|agent| agent.name.as_str());
        if stored.target_pane != pane || current_name != Some(stored.message.target_name.as_str()) {
            return Ok(Response::error(format!(
                "message #{id} はこのpane宛ではありません"
            )));
        }
        let brief = stored.message.brief.clone();
        if !stored.consumed {
            self.journal.append(&Record::Consumed { id })?;
            self.state.consume(id);
        }
        Ok(Response::ok(brief))
    }

    fn reply(&mut self, request: Request) -> Result<Response> {
        let Some(raw_id) = request.args.first() else {
            return Ok(Response::error(
                "usage: agent-talk reply <original-id> [body]",
            ));
        };
        let Ok(original_id) = raw_id.trim_start_matches('#').parse::<u64>() else {
            return Ok(Response::error(format!("message id が不正です: {raw_id}")));
        };
        let Some(pane) = request.pane.as_deref() else {
            return Ok(Response::error(
                "reply は登録済みのtmux pane内で実行してください",
            ));
        };
        let Some(original) = self.state.external_event(original_id).cloned() else {
            return Ok(Response::error(format!(
                "external message #{original_id} が見つかりません"
            )));
        };
        if original.direction != MailboxDirection::Out {
            return Ok(Response::error("reply 対象は外部発のmessageのみです"));
        }
        let Some(agent_name) = self.state.agents.get(pane).map(|agent| agent.name.clone()) else {
            return Ok(Response::error("reply 元paneが登録されていません"));
        };
        if original.target_pane != pane || original.target_name != agent_name {
            return Ok(Response::error("このpaneはreply対象のagentではありません"));
        }
        let body = if request.args.len() > 1 {
            request.args[1..].join(" ")
        } else {
            request.stdin
        };
        if body.is_empty() {
            return Ok(Response::error("本文が空です"));
        }
        if body.len() > MAX_BODY_BYTES {
            return Ok(Response::error(format!(
                "本文がサイズ上限 ({} bytes) を超えています",
                MAX_BODY_BYTES
            )));
        }
        let id = self.state.allocate_id();
        let event = ExternalMailboxEvent {
            id,
            created_at: now_epoch(),
            mailbox: original.mailbox.clone(),
            source_label: agent_name,
            direction: MailboxDirection::In,
            body,
            skill: original.skill.clone(),
            target_name: original.target_name,
            target_pane: original.target_pane,
            reply_to: Some(original_id),
        };
        self.journal.append(&Record::ExternalMailbox {
            event: event.clone(),
        })?;
        self.state.add_mailbox_event(event);
        Ok(Response::ok(format!("replied: #{id}\n")))
    }

    fn mailbox_list(&self, request: Request) -> Result<Response> {
        if request.pane.is_some() {
            return Ok(Response::error(
                "mailbox-list-v1 は外部caller (TMUX_PANEなし) 専用です",
            ));
        }
        let Some(mailbox) = request.args.first() else {
            return Ok(Response::error(
                "usage: agent-talk mailbox-list-v1 <mailbox> [--after <id>] [--limit <n>]",
            ));
        };
        if !is_safe_token(mailbox) || !self.config.allowed_sources.contains(mailbox) {
            return Ok(Response::error("mailbox が許可されていません"));
        }
        let mut after = None;
        let mut limit = 100usize;
        let mut seen_after = false;
        let mut seen_limit = false;
        let mut index = 1;
        while index < request.args.len() {
            let option = request.args[index].as_str();
            let Some(value) = request.args.get(index + 1) else {
                return Ok(Response::error(format!("{option} には値が必要です")));
            };
            match option {
                "--after" => {
                    if seen_after {
                        return Ok(Response::error("--after は複数指定できません"));
                    }
                    seen_after = true;
                    after = Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| anyhow::anyhow!("after id が不正です: {value}"))?,
                    );
                }
                "--limit" => {
                    if seen_limit {
                        return Ok(Response::error("--limit は複数指定できません"));
                    }
                    seen_limit = true;
                    limit = value
                        .parse::<usize>()
                        .map_err(|_| anyhow::anyhow!("limit が不正です: {value}"))?;
                    if limit == 0 || limit > 500 {
                        return Ok(Response::error("limit は1から500の範囲です"));
                    }
                }
                _ => {
                    return Ok(Response::error(format!(
                        "不明なmailbox-listオプションです: {option}"
                    )));
                }
            }
            index += 2;
        }
        let events = self
            .state
            .mailbox_events(mailbox, after, limit)
            .into_iter()
            .map(ExternalMailboxView::from)
            .collect::<Vec<_>>();
        Ok(Response::ok(format!(
            "{}\n",
            serde_json::json!({"version": 1, "mailbox": mailbox, "events": events})
        )))
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

    async fn remove_agent(&mut self, pane: &str, reason: &str) -> bool {
        if !self.state.agents.contains_key(pane) {
            return true;
        }
        let messages = self.state.messages_for_target(pane);
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
        self.state.remove(pane);
        info!(%pane, source = "pane-exited", "removed");
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
            let sender_target = original
                .sender
                .starts_with('%')
                .then(|| {
                    self.state
                        .agents
                        .get(&original.sender)
                        .map(|agent| agent.name.clone())
                })
                .flatten()
                .filter(|expected| {
                    Some(original.sender.as_str()) != excluded_pane
                        && panes.iter().any(|pane| {
                            pane.pane_id == original.sender
                                && pane.agent.as_deref() == Some(expected.as_str())
                        })
                });
            if let Some(expected) = sender_target {
                let failure_brief = format!(
                    "# agent-talk 配達失敗通知\n- from: system\n- to: {expected}\n- reply: 不要\n- original: #{}\n- reason: {reason}\n\n## 元の依頼\n{}",
                    original.id, original.brief
                );
                let dispatch = self.state.dispatch(
                    &original.sender,
                    "system".into(),
                    failure_brief,
                    &expected,
                    |id| {
                        format!(
                            "[agent-talk] 配達失敗: message #{} は {reason}ため配達されませんでした。agent-talk read {id} で元の依頼内容を確認してください。",
                            original.id
                        )
                    },
                );
                let Ok(dispatch) = dispatch else {
                    return false;
                };
                let id = match dispatch {
                    Dispatch::Deliver(id) | Dispatch::Queued(id) => id,
                };
                let Some(stored) = self.state.message(id) else {
                    error!(id, "failure notification body missing before persistence");
                    if matches!(dispatch, Dispatch::Deliver(_)) {
                        self.state.set_state(&original.sender, AgentState::Idle);
                    }
                    self.state.discard_message(id);
                    return false;
                };
                let message = stored.message.clone();
                if let Err(error) = self.journal.append(&Record::Enqueue {
                    pane: original.sender.clone(),
                    message,
                }) {
                    if matches!(dispatch, Dispatch::Deliver(_)) {
                        self.state.set_state(&original.sender, AgentState::Idle);
                    }
                    self.state.discard_message(id);
                    error!(%error, "cannot persist failure notification");
                    return false;
                }
                if matches!(dispatch, Dispatch::Deliver(_)) {
                    if let Err(error) = self.journal.append(&Record::State {
                        pane: original.sender.clone(),
                        state: AgentState::Busy,
                    }) {
                        self.state.set_state(&original.sender, AgentState::Idle);
                        error!(%error, "cannot persist failure notification state");
                        return false;
                    }
                    let Some(stored) = self.state.message(id) else {
                        error!(id, "failure notification body missing before delivery");
                        self.state.set_state(&original.sender, AgentState::Idle);
                        return false;
                    };
                    let bell = stored.message.bell.clone();
                    if self.tmux.deliver(&original.sender, &bell).await.is_ok() {
                        if let Err(error) = self.journal.append(&Record::Complete {
                            pane: original.sender.clone(),
                            id,
                        }) {
                            error!(%error, "cannot complete failure notification");
                            return false;
                        }
                        self.state.complete_delivery(&original.sender, id);
                    } else {
                        self.state
                            .requeue_after_delivery_failure(&original.sender, id);
                        if let Err(error) = self.journal.append(&Record::State {
                            pane: original.sender.clone(),
                            state: AgentState::Idle,
                        }) {
                            error!(%error, "cannot requeue failure notification");
                            return false;
                        }
                    }
                }
            }
            if let Err(error) = self.journal.append(&Record::Consumed { id: original.id }) {
                error!(%error, id = original.id, "cannot consume failed message");
                return false;
            }
            self.state.consume(original.id);
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
            let state = AgentState::Idle;
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

#[derive(Debug, serde::Serialize)]
struct ExternalMailboxView {
    id: u64,
    created_at: String,
    mailbox: String,
    source_label: String,
    direction: MailboxDirection,
    body: String,
    skill: Option<String>,
    target_name: String,
    target_pane: String,
    reply_to: Option<u64>,
}

impl From<ExternalMailboxEvent> for ExternalMailboxView {
    fn from(event: ExternalMailboxEvent) -> Self {
        Self {
            id: event.id,
            created_at: rfc3339(event.created_at),
            mailbox: event.mailbox,
            source_label: event.source_label,
            direction: event.direction,
            body: event.body,
            skill: event.skill,
            target_name: event.target_name,
            target_pane: event.target_pane,
            reply_to: event.reply_to,
        }
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

fn rfc3339(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month, day)
}

fn build_brief(
    addr: &str,
    from: &str,
    origin_info: Option<&PaneInfo>,
    reply_info: Option<&PaneInfo>,
    body: &str,
    external: bool,
    id: u64,
) -> String {
    let origin = origin_info
        .map(|pane| format!(" (session: {}, pane: {})", pane.session, pane.pane_id))
        .unwrap_or_default();
    let reply = if external {
        format!("agent-talk reply {id} に本文を渡す (このmessageへの返信)")
    } else {
        reply_info.map_or_else(
            || "不要 (人間からの依頼。結果は自分の画面に表示すれば読まれる)".to_owned(),
            |pane| {
                format!(
                    "agent-talk send '{}' に返信本文を stdin で渡す (pane ID 指定は曖昧にならない)",
                    pane.pane_id
                )
            },
        )
    };
    format!(
        "# agent-talk 依頼書\n- from: {from}{origin}\n- to: {addr}\n- reply: {reply}\n\n{body}\n"
    )
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

#[cfg(test)]
mod mailbox_tests {
    use super::rfc3339;

    #[test]
    fn epoch_is_rendered_as_stable_utc_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(86_401), "1970-01-02T00:00:01Z");
    }
}
