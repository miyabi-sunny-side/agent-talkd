use std::{
    env,
    ffi::OsStr,
    fmt::Write as _,
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    Method, Request as HttpRequest, Response as HttpResponse, StatusCode,
    body::Incoming,
    header::{ALLOW, CACHE_CONTROL, CONTENT_TYPE},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};
use tracing::{error, info, warn};

use crate::{
    config::{Config, is_safe_token},
    help,
    journal::{Journal, Record},
    protocol::{Request, Response, SendOptions},
    state::{AgentState, BrokerState, Dispatch, ExternalMailboxEvent, MailboxDirection, Message},
    tmux::{PaneInfo, Tmux},
};

const MAX_BODY_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct ConversationSend {
    no_reply: bool,
}

#[derive(Debug)]
struct AuthoritySend {
    source: Option<String>,
    skill: Option<String>,
    no_reply: bool,
}

#[derive(Debug)]
enum SendIntent {
    Conversation(ConversationSend),
    Authority(AuthoritySend),
}

impl SendIntent {
    fn classify(options: SendOptions, registered: bool) -> std::result::Result<Self, &'static str> {
        if registered && options.skill.is_some() {
            return Err("登録agent paneから --skill は指定できません");
        }
        if registered && options.from.is_some() {
            return Err("登録agent paneから --from を上書きできません");
        }
        if options.from.is_some() || options.skill.is_some() {
            if options.from.is_some() && options.no_reply {
                return Err("--no-reply は外部mailbox送信 (--from) には使えません");
            }
            return Ok(Self::Authority(AuthoritySend {
                source: options.from,
                skill: options.skill,
                no_reply: options.no_reply,
            }));
        }
        Ok(Self::Conversation(ConversationSend {
            no_reply: options.no_reply,
        }))
    }

    fn no_reply(&self) -> bool {
        match self {
            Self::Conversation(send) => send.no_reply,
            Self::Authority(send) => send.no_reply,
        }
    }

    fn source(&self) -> Option<&str> {
        match self {
            Self::Conversation(_) => None,
            Self::Authority(send) => send.source.as_deref(),
        }
    }

    fn skill(&self) -> Option<&str> {
        match self {
            Self::Conversation(_) => None,
            Self::Authority(send) => send.skill.as_deref(),
        }
    }
}

enum Event {
    Request {
        request: Request,
        reply: oneshot::Sender<Response>,
        flushed: oneshot::Receiver<()>,
    },
    Http(HttpEvent),
    ServerCheck,
}

enum HttpEvent {
    Who {
        reply: oneshot::Sender<std::result::Result<Vec<WebAgent>, String>>,
    },
    Screen {
        pane: String,
        reply: oneshot::Sender<std::result::Result<WebScreen, WebError>>,
    },
    Mailboxes {
        reply: oneshot::Sender<Vec<String>>,
    },
    Mailbox {
        mailbox: String,
        after: Option<u64>,
        limit: usize,
        reply: oneshot::Sender<std::result::Result<Vec<ExternalMailboxView>, WebError>>,
    },
}

#[derive(Debug, Serialize)]
struct WebAgent {
    name: String,
    state: AgentState,
    pane_id: String,
    session: String,
    location: String,
    cwd: String,
}

#[derive(Debug, Serialize)]
struct WebScreen {
    pane_id: String,
    screen: String,
}

#[derive(Debug)]
struct WebError {
    status: StatusCode,
    code: &'static str,
}

impl WebError {
    fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }
}

fn capture_failure(pane_still_exists: Option<bool>) -> WebError {
    if pane_still_exists == Some(false) {
        WebError::new(StatusCode::GONE, "pane_unavailable")
    } else {
        WebError::new(StatusCode::SERVICE_UNAVAILABLE, "capture_unavailable")
    }
}

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
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
    remove_stale_socket(&config.http_socket)?;
    let http_listener = UnixListener::bind(&config.http_socket)
        .with_context(|| format!("cannot bind {}", config.http_socket.display()))?;

    let tmux = Tmux::new(config.tmux_socket.clone());
    let server_pid = tmux.server_pid().await?;
    let executable = env::current_exe()?;
    tmux.install_pane_exit_hook(&executable, &config.rpc_socket)
        .await?;
    ensure!(
        tmux.server_pid().await? == server_pid,
        "tmux server changed during daemon startup"
    );
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    spawn_accept_loop(listener, tx.clone());
    spawn_http_accept_loop(http_listener, tx.clone());
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

    let mut shutdown_requested = false;
    let mut failed_health_checks = 0_u8;
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
            Event::Http(event) => broker.handle_http_event(event).await,
            Event::ServerCheck => match tmux.server_pid().await {
                Ok(pid) if pid == server_pid => failed_health_checks = 0,
                Ok(_) => break,
                Err(error) => {
                    failed_health_checks += 1;
                    if failed_health_checks >= 2 {
                        break;
                    }
                    warn!(
                        %error,
                        source = "tmux-health",
                        "tmux health check failed; retrying"
                    );
                }
            },
        }
    }

    if shutdown_requested {
        info!(source = "lifecycle", "stopping after shutdown request");
    } else {
        info!(source = "tmux-health", "stopping after tmux server exit");
    }
    tmux.remove_pane_exit_hook().await;
    let _ = fs::remove_file(&broker.config.rpc_socket);
    let _ = fs::remove_file(&broker.config.http_socket);
    Ok(())
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("cannot remove {}", path.display()))?;
    }
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

fn spawn_http_accept_loop(listener: UnixListener, tx: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let peer_uid = stream.peer_cred().ok().map(|credentials| credentials.uid());
            let effective_uid = unsafe { libc::geteuid() };
            if !peer_uid_allowed(peer_uid, effective_uid) {
                warn!(?peer_uid, effective_uid, "rejected HTTP socket peer uid");
                continue;
            }
            let tx = tx.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| route_http(request, tx.clone()));
                if let Err(error) = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    warn!(%error, "HTTP connection failed");
                }
            });
        }
    });
}

fn peer_uid_allowed(peer_uid: Option<u32>, effective_uid: u32) -> bool {
    peer_uid == Some(effective_uid)
}

async fn route_http(
    request: HttpRequest<Incoming>,
    tx: mpsc::Sender<Event>,
) -> std::result::Result<HttpResponse<Full<Bytes>>, std::convert::Infallible> {
    let response = match classify_http(request.method(), request.uri().path()) {
        HttpRoute::MethodNotAllowed => response(
            StatusCode::METHOD_NOT_ALLOWED,
            "application/json",
            br#"{"error":"method_not_allowed"}"#,
        )
        .with_header(ALLOW, "GET"),
        HttpRoute::Hello => json_response(
            StatusCode::OK,
            &serde_json::json!({
                "name": "agent-talk",
                "version": env!("CARGO_PKG_VERSION"),
            }),
        ),
        HttpRoute::Who => request_web_agents(&tx).await,
        HttpRoute::Screen(pane) => {
            if request.uri().query().is_some() {
                json_error(StatusCode::BAD_REQUEST, "invalid_query")
            } else {
                request_web_screen(&tx, pane).await
            }
        }
        HttpRoute::Mailboxes => {
            if request.uri().query().is_some() {
                json_error(StatusCode::BAD_REQUEST, "invalid_query")
            } else {
                request_web_mailboxes(&tx).await
            }
        }
        HttpRoute::Mailbox(mailbox) => {
            request_web_mailbox(&tx, mailbox, request.uri().query()).await
        }
        HttpRoute::BadRequest => json_error(StatusCode::BAD_REQUEST, "invalid_path_parameter"),
        HttpRoute::NotFound | HttpRoute::ApiNotFound => {
            json_error(StatusCode::NOT_FOUND, "not_found")
        }
        HttpRoute::Static => static_response(request.uri().path()),
    };
    Ok(response)
}

async fn request_web_agents(tx: &mpsc::Sender<Event>) -> HttpResponse<Full<Bytes>> {
    let (reply, receive) = oneshot::channel();
    if tx
        .send(Event::Http(HttpEvent::Who { reply }))
        .await
        .is_err()
    {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "broker_unavailable");
    }
    match receive.await {
        Ok(Ok(agents)) => json_response(StatusCode::OK, &serde_json::json!({ "agents": agents })),
        _ => json_error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable"),
    }
}

async fn request_web_screen(tx: &mpsc::Sender<Event>, pane: String) -> HttpResponse<Full<Bytes>> {
    let (reply, receive) = oneshot::channel();
    if tx
        .send(Event::Http(HttpEvent::Screen { pane, reply }))
        .await
        .is_err()
    {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "broker_unavailable");
    }
    match receive.await {
        Ok(Ok(screen)) => json_response(StatusCode::OK, &screen),
        Ok(Err(error)) => json_error(error.status, error.code),
        Err(_) => json_error(StatusCode::SERVICE_UNAVAILABLE, "broker_unavailable"),
    }
}

async fn request_web_mailboxes(tx: &mpsc::Sender<Event>) -> HttpResponse<Full<Bytes>> {
    let (reply, receive) = oneshot::channel();
    if tx
        .send(Event::Http(HttpEvent::Mailboxes { reply }))
        .await
        .is_err()
    {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "broker_unavailable");
    }
    match receive.await {
        Ok(mailboxes) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "mailboxes": mailboxes }),
        ),
        Err(_) => json_error(StatusCode::SERVICE_UNAVAILABLE, "broker_unavailable"),
    }
}

async fn request_web_mailbox(
    tx: &mpsc::Sender<Event>,
    mailbox: String,
    query: Option<&str>,
) -> HttpResponse<Full<Bytes>> {
    let (after, limit) = match parse_mailbox_query(query) {
        Ok(page) => page,
        Err(code) => return json_error(StatusCode::BAD_REQUEST, code),
    };
    let (reply, receive) = oneshot::channel();
    if tx
        .send(Event::Http(HttpEvent::Mailbox {
            mailbox: mailbox.clone(),
            after,
            limit,
            reply,
        }))
        .await
        .is_err()
    {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "broker_unavailable");
    }
    match receive.await {
        Ok(Ok(events)) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "version": 1, "mailbox": mailbox, "events": events }),
        ),
        Ok(Err(error)) => json_error(error.status, error.code),
        Err(_) => json_error(StatusCode::SERVICE_UNAVAILABLE, "broker_unavailable"),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HttpRoute {
    MethodNotAllowed,
    Hello,
    Who,
    Screen(String),
    Mailboxes,
    Mailbox(String),
    BadRequest,
    NotFound,
    ApiNotFound,
    Static,
}

fn classify_http(method: &Method, path: &str) -> HttpRoute {
    if method != Method::GET {
        return HttpRoute::MethodNotAllowed;
    }
    match path {
        "/v1/hello" => HttpRoute::Hello,
        "/v1/who" => HttpRoute::Who,
        "/v1/mailboxes" => HttpRoute::Mailboxes,
        path if path.starts_with("/v1/agents/") && path.ends_with("/screen") => {
            let encoded = path
                .strip_prefix("/v1/agents/")
                .and_then(|rest| rest.strip_suffix("/screen"))
                .unwrap_or_default();
            match decode_path_segment(encoded) {
                Some(pane) if valid_pane_id(&pane) => HttpRoute::Screen(pane),
                Some(_) => HttpRoute::NotFound,
                None => HttpRoute::BadRequest,
            }
        }
        path if let Some(encoded) = path.strip_prefix("/v1/mailbox/") => {
            match decode_path_segment(encoded) {
                Some(mailbox) if is_safe_token(&mailbox) => HttpRoute::Mailbox(mailbox),
                Some(_) => HttpRoute::NotFound,
                None => HttpRoute::BadRequest,
            }
        }
        path if path.starts_with("/v1/") => HttpRoute::ApiNotFound,
        _ => HttpRoute::Static,
    }
}

fn decode_path_segment(encoded: &str) -> Option<String> {
    if encoded.is_empty() || encoded.contains('/') || encoded.contains('\0') {
        return None;
    }
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = *bytes.get(index + 1)?;
            let low = *bytes.get(index + 2)?;
            decoded.push(hex_value(high)? * 16 + hex_value(low)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    if decoded.contains(&b'/') || decoded.contains(&0) {
        return None;
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn valid_pane_id(pane: &str) -> bool {
    pane.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn parse_mailbox_query(
    query: Option<&str>,
) -> std::result::Result<(Option<u64>, usize), &'static str> {
    let mut after = None::<&str>;
    let mut limit = None::<&str>;
    let Some(query) = query else {
        return parse_mailbox_page(after, limit).map_err(|_| "invalid_query");
    };
    if query.is_empty() {
        return Err("invalid_query");
    }
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err("invalid_query");
        };
        match key {
            "after" if after.is_none() => after = Some(value),
            "limit" if limit.is_none() => limit = Some(value),
            "after" | "limit" => return Err("duplicate_query_parameter"),
            _ => return Err("invalid_query"),
        }
    }
    parse_mailbox_page(after, limit).map_err(|error| match error {
        MailboxPageError::InvalidAfter => "invalid_after",
        MailboxPageError::InvalidLimit | MailboxPageError::LimitOutOfRange => "invalid_limit",
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MailboxPageError {
    InvalidAfter,
    InvalidLimit,
    LimitOutOfRange,
}

fn parse_mailbox_page(
    after: Option<&str>,
    limit: Option<&str>,
) -> std::result::Result<(Option<u64>, usize), MailboxPageError> {
    let after = after
        .map(|value| value.parse().map_err(|_| MailboxPageError::InvalidAfter))
        .transpose()?;
    let limit = limit
        .map(|value| value.parse().map_err(|_| MailboxPageError::InvalidLimit))
        .transpose()?
        .unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return Err(MailboxPageError::LimitOutOfRange);
    }
    Ok((after, limit))
}

trait HeaderExt {
    fn with_header(self, name: hyper::header::HeaderName, value: &'static str) -> Self;
}

impl HeaderExt for HttpResponse<Full<Bytes>> {
    fn with_header(mut self, name: hyper::header::HeaderName, value: &'static str) -> Self {
        self.headers_mut()
            .insert(name, value.parse().expect("valid header"));
        self
    }
}

fn json_response(status: StatusCode, value: &impl Serialize) -> HttpResponse<Full<Bytes>> {
    let body =
        serde_json::to_vec(value).unwrap_or_else(|_| br#"{"error":"encoding_failed"}"#.to_vec());
    response(status, "application/json; charset=utf-8", &body)
}

fn json_error(status: StatusCode, code: &str) -> HttpResponse<Full<Bytes>> {
    json_response(status, &serde_json::json!({ "error": code }))
}

fn static_response(path: &str) -> HttpResponse<Full<Bytes>> {
    if embedded::ASSETS.is_empty() {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "static_assets_unavailable");
    }
    let normalized = if path == "/" { "/index.html" } else { path };
    let asset = embedded::ASSETS
        .iter()
        .find(|(route, _)| *route == normalized)
        .or_else(|| {
            embedded::ASSETS
                .iter()
                .find(|(route, _)| *route == "/index.html")
        });
    let Some((route, bytes)) = asset else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "static_assets_unavailable");
    };
    response(StatusCode::OK, content_type(route), bytes).with_header(CACHE_CONTROL, "no-cache")
}

fn content_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(OsStr::to_str) {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json; charset=utf-8",
        _ => "text/html; charset=utf-8",
    }
}

fn response(
    status: StatusCode,
    content_type: &'static str,
    body: &[u8],
) -> HttpResponse<Full<Bytes>> {
    HttpResponse::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .body(Full::new(Bytes::copy_from_slice(body)))
        .expect("valid HTTP response")
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
    async fn handle_http_event(&self, event: HttpEvent) {
        match event {
            HttpEvent::Who { reply } => {
                let result = self.web_agents().await.map_err(|error| error.to_string());
                let _ = reply.send(result);
            }
            HttpEvent::Screen { pane, reply } => {
                let _ = reply.send(self.web_screen(&pane).await);
            }
            HttpEvent::Mailboxes { reply } => {
                let _ = reply.send(self.config.allowed_sources.iter().cloned().collect());
            }
            HttpEvent::Mailbox {
                mailbox,
                after,
                limit,
                reply,
            } => {
                let _ = reply.send(self.web_mailbox(&mailbox, after, limit));
            }
        }
    }

    async fn web_agents(&self) -> Result<Vec<WebAgent>> {
        let panes = self.tmux.panes().await?;
        let mut agents = Vec::new();
        for pane in panes {
            if let Some(agent) = self.state.agents.get(&pane.pane_id) {
                agents.push(WebAgent {
                    name: agent.name.clone(),
                    state: agent.state,
                    pane_id: pane.pane_id,
                    session: pane.session.clone(),
                    location: format!("{}:{}.{}", pane.session, pane.window_index, pane.pane_index),
                    cwd: pane.cwd,
                });
            }
        }
        Ok(agents)
    }

    async fn web_screen(&self, pane: &str) -> std::result::Result<WebScreen, WebError> {
        if !self.state.agents.contains_key(pane) {
            return Err(WebError::new(StatusCode::NOT_FOUND, "agent_not_found"));
        }
        let panes = self
            .tmux
            .panes()
            .await
            .map_err(|_| WebError::new(StatusCode::SERVICE_UNAVAILABLE, "tmux_unavailable"))?;
        if !panes.iter().any(|candidate| candidate.pane_id == pane) {
            return Err(WebError::new(StatusCode::GONE, "pane_unavailable"));
        }
        if let Ok(screen) = self.tmux.capture_pane(pane).await {
            Ok(WebScreen {
                pane_id: pane.to_owned(),
                screen,
            })
        } else {
            let pane_still_exists = self
                .tmux
                .panes()
                .await
                .ok()
                .map(|panes| panes.iter().any(|candidate| candidate.pane_id == pane));
            Err(capture_failure(pane_still_exists))
        }
    }

    fn web_mailbox(
        &self,
        mailbox: &str,
        after: Option<u64>,
        limit: usize,
    ) -> std::result::Result<Vec<ExternalMailboxView>, WebError> {
        if !self.config.allowed_sources.contains(mailbox) {
            return Err(WebError::new(StatusCode::NOT_FOUND, "mailbox_not_found"));
        }
        Ok(self
            .state
            .mailbox_events(mailbox, after, limit)
            .into_iter()
            .map(ExternalMailboxView::from)
            .collect())
    }

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
            "read" => self.read(&request),
            "reply" => self.reply(request),
            "mailbox-list-v1" => self.mailbox_list(&request),
            "internal-daemon-status" => Ok(Response::ok(format!(
                "{}\n",
                serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "pid": std::process::id(),
                    "ready": true,
                })
            ))),
            "internal-daemon-shutdown" | "gc" | "watch" => Ok(Response::ok("")),
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
            return Ok(Response::error(help::usage("register")));
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
                let _ = writeln!(
                    output,
                    "{:<10} {:<5} {}:{}.{} ({})  {}",
                    agent.name,
                    agent.state.as_str(),
                    pane.session,
                    pane.window_index,
                    pane.pane_index,
                    pane.pane_id,
                    pane.cwd
                );
            }
        }
        Ok(Response::ok(output))
    }

    async fn resolve_command(&self, request: Request) -> Result<Response> {
        let Some(addr) = request.args.first() else {
            return Ok(Response::error(help::usage("resolve")));
        };
        match self.resolve(addr, request.pane.as_deref()).await {
            Ok((pane, _)) => Ok(Response::ok(format!("{pane}\n"))),
            Err(response) => Ok(response),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn send(&mut self, request: Request) -> Result<Response> {
        let Some(addr) = request.args.first() else {
            return Ok(Response::error(help::usage("send")));
        };
        let registered_sender = request.pane.as_deref().and_then(|pane| {
            self.state
                .agents
                .get(pane)
                .map(|agent| (pane.to_owned(), agent.name.clone()))
        });
        let intent = match SendIntent::classify(
            request.send_options.unwrap_or_default(),
            registered_sender.is_some(),
        ) {
            Ok(intent) => intent,
            Err(error) => return Ok(Response::error(error)),
        };
        let external_source = intent.source().map(str::to_owned);
        if let Some(skill) = intent.skill() {
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
        if let Some(source) = intent.source() {
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
        let skill_prefix = if let Some(skill) = intent.skill() {
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
                "本文がサイズ上限 ({MAX_BODY_BYTES} bytes) を超えています"
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
            .or_else(|| intent.source().map(str::to_owned))
            .unwrap_or_else(|| "human".into());
        let reply_info = registered_sender.as_ref().and(from_info);
        let brief_mode = if external_source.is_some() {
            BriefMode::External(0)
        } else if intent.no_reply() {
            BriefMode::NoReply
        } else {
            BriefMode::Normal
        };
        let brief = build_brief(addr, &from_agent, from_info, reply_info, &body, brief_mode);
        if let Some((from_pane, _)) = registered_sender.as_ref() {
            self.tmux.mark_talk_sent(from_pane).await;
        }
        let sender = registered_sender.map_or_else(|| "human".into(), |(pane, _)| pane);
        let dispatch = self.state.dispatch(
            &pane,
            sender,
            brief,
            &expected,
            |id| {
                if intent.no_reply() {
                    format!(
                        "{skill_prefix}[agent-talk] {from_agent} から連絡が届きました。agent-talk read {id} で本文を確認してください。返信は不要です。"
                    )
                } else {
                    format!(
                        "{skill_prefix}[agent-talk] {from_agent} から依頼が届きました。agent-talk read {id} で本文を確認して対応してください。"
                    )
                }
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
                        build_brief(
                            addr,
                            &from_agent,
                            from_info,
                            reply_info,
                            &body,
                            BriefMode::External(id),
                        ),
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
                            skill: intent.skill().map(str::to_owned),
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
                        "配達状態を書き込めず配達できません (#{id}): {error}"
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

    fn read(&mut self, request: &Request) -> Result<Response> {
        let Some(raw_id) = request.args.first() else {
            return Ok(Response::error(help::usage("read")));
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
            return Ok(Response::error(help::usage("reply")));
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
                "本文がサイズ上限 ({MAX_BODY_BYTES} bytes) を超えています"
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

    fn mailbox_list(&self, request: &Request) -> Result<Response> {
        if request.pane.is_some() {
            return Ok(Response::error(
                "mailbox-list-v1 は外部caller (TMUX_PANEなし) 専用です",
            ));
        }
        let Some(mailbox) = request.args.first() else {
            return Ok(Response::error(help::usage("mailbox-list-v1")));
        };
        if !is_safe_token(mailbox) || !self.config.allowed_sources.contains(mailbox) {
            return Ok(Response::error("mailbox が許可されていません"));
        }
        let mut after = None::<&str>;
        let mut limit = None::<&str>;
        let mut index = 1;
        while index < request.args.len() {
            let option = request.args[index].as_str();
            let Some(value) = request.args.get(index + 1) else {
                return Ok(Response::error(format!("{option} には値が必要です")));
            };
            match option {
                "--after" => {
                    if after.is_some() {
                        return Ok(Response::error("--after は複数指定できません"));
                    }
                    after = Some(value);
                }
                "--limit" => {
                    if limit.is_some() {
                        return Ok(Response::error("--limit は複数指定できません"));
                    }
                    limit = Some(value);
                }
                _ => {
                    return Ok(Response::error(format!(
                        "不明なmailbox-listオプションです: {option}"
                    )));
                }
            }
            index += 2;
        }
        let (after, limit) = match parse_mailbox_page(after, limit) {
            Ok(page) => page,
            Err(MailboxPageError::InvalidAfter) => {
                return Err(anyhow::anyhow!(
                    "after id が不正です: {}",
                    after.expect("invalid after has a value")
                ));
            }
            Err(MailboxPageError::InvalidLimit) => {
                return Err(anyhow::anyhow!(
                    "limit が不正です: {}",
                    limit.expect("invalid limit has a value")
                ));
            }
            Err(MailboxPageError::LimitOutOfRange) => {
                return Ok(Response::error("limit は1から500の範囲です"));
            }
        };
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
            if same_window.is_empty() {
                candidates.retain(|(pane, _)| pane.session == origin.session);
            } else {
                candidates = same_window;
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

    #[allow(clippy::too_many_lines)]
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
            let initial_state = AgentState::Idle;
            if self
                .journal
                .append(&Record::Register {
                    pane: pane.pane_id.clone(),
                    name: name.clone(),
                    state: initial_state,
                })
                .is_ok()
            {
                self.state
                    .restore_agent(pane.pane_id.clone(), name.clone(), initial_state);
                info!(
                    pane = %pane.pane_id,
                    %name,
                    state = ?initial_state,
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
        .map_or(0, |duration| duration.as_secs().cast_signed())
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

#[derive(Clone, Copy)]
enum BriefMode {
    Normal,
    NoReply,
    External(u64),
}

fn build_brief(
    addr: &str,
    from: &str,
    origin_info: Option<&PaneInfo>,
    reply_info: Option<&PaneInfo>,
    body: &str,
    mode: BriefMode,
) -> String {
    let origin = origin_info
        .map(|pane| format!(" (session: {}, pane: {})", pane.session, pane.pane_id))
        .unwrap_or_default();
    let reply = match mode {
        BriefMode::External(id) => {
            format!("agent-talk reply {id} に本文を渡す (このmessageへの返信)")
        }
        BriefMode::NoReply => {
            "原則不要 (一方向の連絡。重大な実害を防ぐ異議がある場合のみ1通だけ返信可)".to_owned()
        }
        BriefMode::Normal => reply_info.map_or_else(
            || "不要 (人間からの依頼。結果は自分の画面に表示すれば読まれる)".to_owned(),
            |pane| {
                format!(
                    "agent-talk send '{}' に返信本文を stdin で渡す (pane ID 指定は曖昧にならない)",
                    pane.pane_id
                )
            },
        ),
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
mod tests {
    use hyper::{Method, StatusCode};

    use super::{
        HttpRoute, MailboxPageError, SendIntent, SendOptions, WebAgent, capture_failure,
        classify_http, decode_path_segment, parse_mailbox_page, parse_mailbox_query,
        peer_uid_allowed, rfc3339, static_response,
    };
    use crate::state::AgentState;

    #[test]
    fn http_routes_are_read_only_and_api_misses_do_not_fall_back() {
        assert_eq!(classify_http(&Method::GET, "/v1/hello"), HttpRoute::Hello);
        assert_eq!(classify_http(&Method::GET, "/v1/who"), HttpRoute::Who);
        assert_eq!(
            classify_http(&Method::GET, "/v1/agents/%251/screen"),
            HttpRoute::Screen("%1".into())
        );
        assert_eq!(
            classify_http(&Method::GET, "/v1/mailboxes"),
            HttpRoute::Mailboxes
        );
        assert_eq!(
            classify_http(&Method::GET, "/v1/mailbox/review%3Asecurity"),
            HttpRoute::Mailbox("review:security".into())
        );
        assert_eq!(
            classify_http(&Method::GET, "/v1/missing"),
            HttpRoute::ApiNotFound
        );
        assert_eq!(
            classify_http(&Method::GET, "/agents/one"),
            HttpRoute::Static
        );
        assert_eq!(
            classify_http(&Method::POST, "/v1/who"),
            HttpRoute::MethodNotAllowed
        );
        for path in [
            "/v1/agents/screen",
            "/v1/agents/%1/screen",
            "/v1/agents//screen",
            "/v1/mailbox/bad%2Fname",
        ] {
            assert_eq!(classify_http(&Method::GET, path), HttpRoute::BadRequest);
        }
        for path in [
            "/v1/agents/%252F/screen",
            "/v1/agents/%250x/screen",
            "/v1/mailbox/Bad",
        ] {
            assert_eq!(classify_http(&Method::GET, path), HttpRoute::NotFound);
        }
    }

    #[test]
    fn strict_percent_decoder_rejects_malformed_and_unsafe_segments() {
        assert_eq!(decode_path_segment("%251"), Some("%1".into()));
        assert_eq!(decode_path_segment("mobile"), Some("mobile".into()));
        assert_eq!(decode_path_segment("%"), None);
        assert_eq!(decode_path_segment("%GG"), None);
        assert_eq!(decode_path_segment("%2F"), None);
        assert_eq!(decode_path_segment("%00"), None);
        assert_eq!(decode_path_segment("a/b"), None);
    }

    #[test]
    fn mailbox_query_is_bounded_and_rejects_duplicates() {
        assert_eq!(parse_mailbox_query(None), Ok((None, 100)));
        assert_eq!(
            parse_mailbox_query(Some("after=41&limit=2")),
            Ok((Some(41), 2))
        );
        assert_eq!(parse_mailbox_query(Some("limit=0")), Err("invalid_limit"));
        assert_eq!(parse_mailbox_query(Some("limit=501")), Err("invalid_limit"));
        assert_eq!(
            parse_mailbox_query(Some("after=1&after=2")),
            Err("duplicate_query_parameter")
        );
        assert_eq!(parse_mailbox_query(Some("unknown=1")), Err("invalid_query"));
        assert_eq!(parse_mailbox_page(None, None), Ok((None, 100)));
        assert_eq!(
            parse_mailbox_page(Some("9"), Some("500")),
            Ok((Some(9), 500))
        );
        assert_eq!(
            parse_mailbox_page(Some("bad"), None),
            Err(MailboxPageError::InvalidAfter)
        );
        assert_eq!(
            parse_mailbox_page(None, Some("bad")),
            Err(MailboxPageError::InvalidLimit)
        );
        assert_eq!(
            parse_mailbox_page(None, Some("501")),
            Err(MailboxPageError::LimitOutOfRange)
        );
    }

    #[test]
    fn uid_gate_requires_a_known_matching_peer() {
        assert!(peer_uid_allowed(Some(1000), 1000));
        assert!(!peer_uid_allowed(Some(1001), 1000));
        assert!(!peer_uid_allowed(None, 1000));
    }

    #[test]
    fn capture_failure_only_reports_gone_after_confirmed_disappearance() {
        let gone = capture_failure(Some(false));
        assert_eq!(gone.status, StatusCode::GONE);
        assert_eq!(gone.code, "pane_unavailable");
        for still_exists in [Some(true), None] {
            let transient = capture_failure(still_exists);
            assert_eq!(transient.status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(transient.code, "capture_unavailable");
        }
    }

    #[test]
    fn web_agent_json_escapes_paths_with_spaces_and_quotes() {
        let encoded = serde_json::to_string(&WebAgent {
            name: "co\"dex".into(),
            state: AgentState::Idle,
            pane_id: "%1".into(),
            session: "work".into(),
            location: "work:0.1".into(),
            cwd: "/tmp/project with \"quotes\"".into(),
        })
        .unwrap();
        assert!(encoded.contains(r#""cwd":"/tmp/project with \"quotes\"""#));
    }

    #[test]
    fn cargo_build_without_frontend_reports_static_assets_unavailable() {
        if super::embedded::ASSETS.is_empty() {
            assert_eq!(
                static_response("/").status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
    }

    #[test]
    fn epoch_is_rendered_as_stable_utc_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(86_401), "1970-01-02T00:00:01Z");
    }

    #[test]
    fn send_intent_separates_conversation_from_authority() {
        let conversation = SendIntent::classify(
            SendOptions {
                no_reply: true,
                ..SendOptions::default()
            },
            true,
        )
        .unwrap();
        assert!(matches!(conversation, SendIntent::Conversation(_)));
        assert!(conversation.no_reply());
        assert_eq!(conversation.source(), None);
        assert_eq!(conversation.skill(), None);

        let authority = SendIntent::classify(
            SendOptions {
                from: Some("mobile".into()),
                skill: Some("deliver".into()),
                no_reply: false,
            },
            false,
        )
        .unwrap();
        assert!(matches!(authority, SendIntent::Authority(_)));
        assert_eq!(authority.source(), Some("mobile"));
        assert_eq!(authority.skill(), Some("deliver"));
        assert!(!authority.no_reply());

        let skill_no_reply = SendIntent::classify(
            SendOptions {
                skill: Some("deliver".into()),
                no_reply: true,
                ..SendOptions::default()
            },
            false,
        )
        .unwrap();
        assert!(matches!(skill_no_reply, SendIntent::Authority(_)));
        assert_eq!(skill_no_reply.source(), None);
        assert_eq!(skill_no_reply.skill(), Some("deliver"));
        assert!(skill_no_reply.no_reply());
    }

    #[test]
    fn registered_sender_cannot_construct_authority_intent() {
        let skill = SendIntent::classify(
            SendOptions {
                skill: Some("deliver".into()),
                ..SendOptions::default()
            },
            true,
        );
        assert_eq!(
            skill.unwrap_err(),
            "登録agent paneから --skill は指定できません"
        );

        let source = SendIntent::classify(
            SendOptions {
                from: Some("mobile".into()),
                ..SendOptions::default()
            },
            true,
        );
        assert_eq!(
            source.unwrap_err(),
            "登録agent paneから --from を上書きできません"
        );

        let external_no_reply = SendIntent::classify(
            SendOptions {
                from: Some("mobile".into()),
                no_reply: true,
                ..SendOptions::default()
            },
            false,
        );
        assert_eq!(
            external_no_reply.unwrap_err(),
            "--no-reply は外部mailbox送信 (--from) には使えません"
        );
    }
}
