use std::{
    env,
    ffi::OsStr,
    fmt::Write as _,
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
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
    net::{TcpListener, UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};
use tracing::{error, info, warn};

use crate::{
    backend::{Multiplexer, PaneInfo},
    config::{Config, is_safe_token},
    help,
    herdr::Herdr,
    journal::{Journal, Record},
    protocol::{Request, Response, SendOptions},
    state::{
        AgentState, BrokerState, Dispatch, ExternalMailboxEvent, MailboxDirection, Message, Origin,
        StoredMessage,
    },
    tmux::Tmux,
};

const MAX_BODY_BYTES: usize = 1024 * 1024;

/// MCP adapter 経由の操作は、呼び出し元 pane が登録済み agent であることを要求する。
/// `TMUX_PANE` は routing metadata でしかないが、**未登録 pane を拒否する既存境界は
/// 変更しない** (docs/decisions/0001-conversation-broker-scope.md 起動時 contract 5)。
const UNREGISTERED_CALLER: &str = "この操作は登録済みのagent paneからのみ実行できます (agent-talk register を先に実行してください)";

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

/// 送信が受理された経路。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendPath {
    Sent,
    Queued,
}

impl SendPath {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Queued => "queued",
        }
    }
}

/// `send` の成功応答の形。CLI は従来どおり人間向けテキスト、MCP adapter は
/// versioned JSON を受け取る。**暗黙に劣化させないため、MCP 経路では必ず構造化する。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendReport {
    Text,
    Json,
}

impl SendReport {
    fn usage_command(self) -> &'static str {
        match self {
            Self::Text => "send",
            Self::Json => "send-message-v1",
        }
    }

    fn accepted(self, path: SendPath, id: u64, pane: &str, addr: &str, name: &str) -> Response {
        match self {
            Self::Text => match path {
                SendPath::Sent => Response::ok(format!("sent -> {pane} ({addr}): #{id}\n")),
                SendPath::Queued => {
                    Response::ok(format!("queued (busy) -> {pane} ({addr}): #{id}\n"))
                }
            },
            Self::Json => Response::ok(format!(
                "{}\n",
                serde_json::json!({
                    "version": 1,
                    "id": id,
                    "path": path.as_str(),
                    "to": pane,
                    "name": name,
                })
            )),
        }
    }
}

/// `read` / `ack` から見たメッセージの状態。
#[derive(Debug)]
enum MessageAccess<'a> {
    /// 呼び出し元宛・配達完了済み・未受領。
    Pending(&'a StoredMessage),
    /// 存在しないか、既に受領報告済み。
    NotFound,
    NotMine,
    /// queue 中または配達未完了。呼び鈴の前に読ませない。
    Undelivered,
}

impl MessageAccess<'_> {
    fn reject_reason(&self, id: u64) -> String {
        match self {
            // 呼び出し側が Pending を分岐で処理済みのため到達しない。daemon を落とさない。
            Self::Pending(_) | Self::NotFound => {
                format!("message #{id} は見つかりません (受領報告済みの可能性があります)")
            }
            Self::NotMine => format!("message #{id} はこのpane宛ではありません"),
            Self::Undelivered => {
                format!("message #{id} はまだ配達されていません (呼び鈴を受けてから読んでください)")
            }
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
    if let Some(parent) = config.rpc_socket().parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let Some(listeners) = bind_rpc_sockets(&config.rpc_sockets).await? else {
        // 既に生きた daemon が居る。二重起動しない。
        return Ok(());
    };
    remove_stale_socket(&config.http_socket)?;
    let http_listener = UnixListener::bind(&config.http_socket)
        .with_context(|| format!("cannot bind {}", config.http_socket.display()))?;

    let tmux = config.tmux_socket.clone().map(Tmux::new);
    let herdr = config.herdr_socket.clone().map(Herdr::new);
    let mut mux = Multiplexer::new(tmux, herdr);
    let baseline = mux.probe().await?;
    let executable = env::current_exe()?;
    if let Some(tmux) = mux.tmux() {
        tmux.install_pane_exit_hook(&executable, config.rpc_socket())
            .await?;
        ensure!(
            Some(tmux.server_pid().await?) == baseline.tmux_pid,
            "tmux server changed during daemon startup"
        );
    }
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    for listener in listeners {
        spawn_accept_loop(listener, tx.clone());
    }
    spawn_http_accept_loop(http_listener, tx.clone());
    spawn_http_tcp(config.http_tcp.as_deref(), &tx).await;
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
        mux: mux.clone(),
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
            // 設定された backend が **すべて** 落ちたときだけ停止する。
            // 片方だけ落ちても、もう片方に居る agent の会話は続く。
            Event::ServerCheck => match mux.still_serving(&baseline).await {
                Ok(true) => failed_health_checks = 0,
                Ok(false) => break,
                Err(error) => {
                    failed_health_checks += 1;
                    if failed_health_checks >= 2 {
                        break;
                    }
                    warn!(%error, source = "health", "health check failed; retrying");
                }
            },
        }
    }

    if shutdown_requested {
        info!(source = "lifecycle", "stopping after shutdown request");
    } else {
        info!(
            source = "health",
            "stopping after all multiplexers went away"
        );
    }
    if let Some(tmux) = mux.tmux() {
        tmux.remove_pane_exit_hook().await;
    }
    for path in &broker.config.rpc_sockets {
        let _ = fs::remove_file(path);
    }
    let _ = fs::remove_file(&broker.config.http_socket);
    Ok(())
}

/// 両 backend 分の RPC socket をすべて開く。
///
/// tmux の pane に居るクライアントと herdr の pane に居るクライアントは
/// 別の path を導出するが、どちらもこの 1 プロセスへ届く。
/// これが backend をまたいだ会話の土台になる。
///
/// 既に生きた daemon が居るときは `None` を返す (二重起動しない)。
async fn bind_rpc_sockets(paths: &[PathBuf]) -> Result<Option<Vec<UnixListener>>> {
    // **先に全 path を調べる。** 1 つでも生きた daemon が居るなら 1 つも bind
    // しない。途中まで bind してから中断すると、drop された listener の
    // pathname だけが残り、その socket へ繋ぐクライアントが永久に待たされる。
    for path in paths {
        if path.exists() && UnixStream::connect(path).await.is_ok() {
            return Ok(None);
        }
    }
    let mut listeners = Vec::with_capacity(paths.len());
    for path in paths {
        remove_stale_socket(path)?;
        listeners.push(
            UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?,
        );
    }
    Ok(Some(listeners))
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

/// agent-terrace 相当の TCP 面を必要なら開く。
///
/// bind に失敗しても daemon は止めない。TCP はスマホ向けの追加面であって、
/// agent 同士の会話 (UDS 面) はそれと独立に成立していなければならない。
async fn spawn_http_tcp(addr: Option<&str>, tx: &mpsc::Sender<Event>) {
    let Some(addr) = addr else {
        return;
    };
    match TcpListener::bind(addr).await {
        Ok(listener) => {
            info!(source = "http-tcp", %addr, "listening");
            spawn_http_tcp_accept_loop(listener, tx.clone());
        }
        Err(error) => {
            error!(%error, source = "http-tcp", %addr, "cannot bind; continuing without TCP");
        }
    }
}

/// agent-terrace 相当の TCP 面。
///
/// UDS 面と違い peer uid で絞れない。ネットワーク境界は Tailscale が担う、
/// というのが user の明示的な判断であり、ここでは独自の認証層を持たない
/// (2026-08-03 の依頼文: 「現在存在しているagent達が、TCP越しに悪さをするのは
/// 目を瞑る」)。既定では `AGENT_TALK_HTTP_ADDR` 未設定＝TCP 面なし。
fn spawn_http_tcp_accept_loop(listener: TcpListener, tx: mpsc::Sender<Event>) {
    tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| route_http(request, tx.clone()));
                if let Err(error) = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    warn!(%error, %peer, "HTTP over TCP connection failed");
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
    mux: Multiplexer,
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
        let panes = self.mux.panes().await?;
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
            .mux
            .panes()
            .await
            .map_err(|_| WebError::new(StatusCode::SERVICE_UNAVAILABLE, "tmux_unavailable"))?;
        if !panes.iter().any(|candidate| candidate.pane_id == pane) {
            return Err(WebError::new(StatusCode::GONE, "pane_unavailable"));
        }
        if let Ok(screen) = self.mux.capture_pane(pane).await {
            Ok(WebScreen {
                pane_id: pane.to_owned(),
                screen,
            })
        } else {
            let pane_still_exists = self
                .mux
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
            "who" => self.who(&request).await,
            "resolve" => self.resolve_command(request).await,
            "send" if request.send_options.is_none() => self.send(request, SendReport::Text).await,
            "send" => Ok(Response::error(
                "send optionsにはsend-v2 protocolが必要です",
            )),
            "send-v2" if request.send_options.is_some() => {
                self.send(request, SendReport::Text).await
            }
            "send-v2" => Ok(Response::error("send-v2 optionsがありません")),
            "read" => Ok(self.read(&request)),
            // MCP adapter 専用の versioned RPC。ADR 0001 の起動時 contract 5 により
            // 未登録 pane からは何も出来ない。この gate は message 状態の分岐より前に置く。
            "send-message-v1" | "read-v1" | "ack-v1" | "peers-v1"
                if !self.caller_is_registered(request.pane.as_deref()) =>
            {
                Ok(Response::error(UNREGISTERED_CALLER))
            }
            "send-message-v1" if request.send_options.is_some() => {
                self.send(request, SendReport::Json).await
            }
            "send-message-v1" => Ok(Response::error("send-message-v1 optionsがありません")),
            "read-v1" => Ok(self.read_json(&request)),
            "ack-v1" => self.ack(&request),
            "peers-v1" => self.peers_json(&request).await,
            "reply" => self.reply(request),
            "mailbox-list-v1" => self.mailbox_list(&request),
            "internal-daemon-status" => Ok(Response::ok(format!(
                "{}\n",
                serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "pid": std::process::id(),
                    "ready": true,
                    "backends": self.config.backend_names(),
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
        self.mux.set_option(&pane, "@agent", Some(name)).await?;
        self.mux
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
            if let Err(error) = self.mux.set_option(&pane, "@agent", None).await {
                warn!(%pane, %error, source = "unregister", "agent mirror removal failed");
            }
            if let Err(error) = self.mux.set_option(&pane, "@agent_state", None).await {
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
            .mux
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
            .mux
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
            if self.mux.deliver(&pane, &bell).await.is_ok() {
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

    async fn who(&self, request: &Request) -> Result<Response> {
        let panes = self.mux.panes().await?;
        let mut output = String::new();
        // backend 列は移行期にどちらの multiplexer に居る相手なのかを示す。
        // herdr backend では multiplexer 自身が持つ状態も併記する
        // (agent-talkd の hook 由来の状態より信頼できる場面がある)。
        for pane in &panes {
            if let Some(agent) = self.state.agents.get(&pane.pane_id) {
                let observed = pane
                    .status
                    .map_or_else(String::new, |status| format!("/{}", status.as_str()));
                let _ = writeln!(
                    output,
                    "{:<10} {:<11} {:<5} {}:{}.{} ({})  {}",
                    agent.name,
                    format!("{}{observed}", agent.state.as_str()),
                    pane.backend.as_str(),
                    pane.session,
                    pane.window_index,
                    pane.pane_index,
                    pane.pane_id,
                    pane.cwd
                );
            }
        }
        // 両方向の未受領 ID (docs/decisions/0002-message-retention-ack.md)。本文は含めない。
        if let Some(caller) = request.pane.as_deref() {
            let to_me = self.state.pending_to_me(caller);
            if !to_me.is_empty() {
                let _ = writeln!(output, "pending-to-me: {}", format_ids(&to_me));
            }
            for (target, ids) in self.state.pending_from_me(caller) {
                let _ = writeln!(output, "pending-from-me {target}: {}", format_ids(&ids));
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
    async fn send(&mut self, request: Request, report: SendReport) -> Result<Response> {
        let Some(addr) = request.args.first() else {
            return Ok(Response::error(help::usage(report.usage_command())));
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

        let panes = self.mux.panes().await?;
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
            self.mux.mark_talk_sent(from_pane).await;
        }
        // 送信時点の identity を捕捉する。後からレジストリを引き直さない。
        let origin = registered_sender.map_or_else(
            || Origin::new("human", from_agent.clone()),
            |(pane, name)| Origin::new(pane, name),
        );
        let dispatch = self.state.dispatch(
            &pane,
            origin,
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
                    retires: None,
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
                    return Ok(report.accepted(SendPath::Queued, id, &pane, addr, &expected));
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
                if self.mux.deliver(&pane, &bell).await.is_ok() {
                    self.journal.append(&Record::Complete {
                        pane: pane.clone(),
                        id,
                    })?;
                    self.state.complete_delivery(&pane, id);
                    info!(%pane, id, source = "send", "delivered");
                    Ok(report.accepted(SendPath::Sent, id, &pane, addr, &expected))
                } else {
                    let target_is_live = self
                        .mux
                        .panes()
                        .await
                        .is_ok_and(|panes| panes.iter().any(|item| item.pane_id == pane));
                    if !target_is_live {
                        self.state.set_state(&pane, AgentState::Idle);
                        // 配達不能な terminal tombstone。受領報告待ちにはしない。
                        self.journal.append(&Record::Consumed { id })?;
                        self.state.ack(id);
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
                    Ok(report.accepted(SendPath::Queued, id, &pane, addr, &expected))
                }
            }
            Err(_) => Ok(Response::error(format!(
                "宛先 {pane} ({addr}) は退出済みです"
            ))),
        }
    }

    /// 呼び出し元 pane が現在登録済みの agent か。
    ///
    /// daemon は単一の event loop で1リクエストを最後まで処理するため、この判定と
    /// 後続の操作の間に他のリクエストが割り込むことはない。
    fn caller_is_registered(&self, pane: Option<&str>) -> bool {
        pane.is_some_and(|pane| self.state.agents.contains_key(pane))
    }

    /// `read` / `ack` が共有する宛先・配達状態の検査
    /// (docs/decisions/0002-message-retention-ack.md「`ack_message` の契約」)。
    fn access(&self, id: u64, pane: &str) -> MessageAccess<'_> {
        // 受領報告済み (`Acked`) は存在しないものとして扱う。以後 read は not-found、
        // ack は mutation なしで冪等成功になる。
        let Some(stored) = self.state.message(id).filter(|stored| !stored.acked) else {
            return MessageAccess::NotFound;
        };
        let current_name = self.state.agents.get(pane).map(|agent| agent.name.as_str());
        if stored.target_pane != pane || current_name != Some(stored.message.target_name.as_str()) {
            return MessageAccess::NotMine;
        }
        if !stored.delivered {
            return MessageAccess::Undelivered;
        }
        MessageAccess::Pending(stored)
    }

    /// 本文を返すだけで状態を変えない。受領報告が来るまで何度でも読める。
    fn read(&self, request: &Request) -> Response {
        let (id, pane) = match request_target(request, "read") {
            Ok(target) => target,
            Err(response) => return response,
        };
        match self.access(id, &pane) {
            MessageAccess::Pending(stored) => Response::ok(stored.message.brief.clone()),
            other => Response::error(other.reject_reason(id)),
        }
    }

    /// 構造化 read。MCP adapter の `read_message` が使う。
    fn read_json(&self, request: &Request) -> Response {
        let (id, pane) = match request_target(request, "read-v1") {
            Ok(target) => target,
            Err(response) => return response,
        };
        match self.access(id, &pane) {
            MessageAccess::Pending(stored) => {
                // 送信時点で捕捉した名前を返す。現在のレジストリを引き直さない。
                let from = stored.message.sender_label().to_owned();
                // 返信先は、捕捉時と同じ identity で今も登録中の pane のときだけ。
                let reply_to = self.state.reply_target(&stored.message);
                Response::ok(format!(
                    "{}\n",
                    serde_json::json!({
                        "version": 1,
                        "id": id,
                        "from": from,
                        "reply_to": reply_to,
                        "body": stored.message.brief,
                    })
                ))
            }
            other => Response::error(other.reject_reason(id)),
        }
    }

    /// 受領報告。journal の append + fsync が成功する前に可視性を `Acked` へ進めない。
    fn ack(&mut self, request: &Request) -> Result<Response> {
        let (id, pane) = match request_target(request, "ack-v1") {
            Ok(target) => target,
            Err(response) => return Ok(response),
        };
        let outcome = match self.access(id, &pane) {
            // 存在しない ID は mutation なしで冪等成功にする。checkpoint / prune 後の
            // 再送を安全にするため (0002「なぜ「存在しない ID」を成功にするか」)。
            MessageAccess::NotFound => "no_pending_message",
            MessageAccess::Pending(_) => {
                self.journal.append(&Record::Consumed { id })?;
                self.state.ack(id);
                info!(%pane, id, source = "ack", "acked");
                "acked"
            }
            other => return Ok(Response::error(other.reject_reason(id))),
        };
        Ok(Response::ok(format!(
            "{}\n",
            serde_json::json!({ "version": 1, "id": id, "outcome": outcome })
        )))
    }

    /// 登録 agent 一覧と両方向の未受領 ID。MCP adapter の `list_peers` が使う。
    async fn peers_json(&self, request: &Request) -> Result<Response> {
        let panes = self.mux.panes().await?;
        let caller = request.pane.as_deref();
        let pending_to_me = caller.map(|pane| self.state.pending_to_me(pane));
        let pending_from_me = caller.map(|pane| self.state.pending_from_me(pane));
        let peers: Vec<_> = panes
            .iter()
            .filter_map(|pane| {
                let agent = self.state.agents.get(&pane.pane_id)?;
                Some(serde_json::json!({
                    "name": agent.name,
                    "state": agent.state,
                    "location": format!("{}:{}.{}", pane.session, pane.window_index, pane.pane_index),
                    "pane": pane.pane_id,
                    "cwd": pane.cwd,
                    "queued": agent.queue.len(),
                    "pending_from_me": pending_from_me
                        .as_ref()
                        .and_then(|pending| pending.get(&pane.pane_id))
                        .cloned()
                        .unwrap_or_default(),
                }))
            })
            .collect();
        Ok(Response::ok(format!(
            "{}\n",
            serde_json::json!({
                "version": 1,
                "self": caller,
                "pending_to_me": pending_to_me.unwrap_or_default(),
                "peers": peers,
            })
        )))
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
        let panes = self.mux.panes().await.map_err(|error| {
            Response::error(format!(
                "tmux サーバーに接続できません (sandbox 内なら承認付きで再実行): {error}"
            ))
        })?;
        // pane id の直接指定。tmux (`%5`) と herdr (`w1:p2`) の両方を受ける。
        if crate::backend::BackendKind::of(addr).is_some() {
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
        let panes = self.mux.panes().await.unwrap_or_default();
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
                // 「配達されなかった」ではなく受領報告の欠如を表す文言にする
                // (docs/decisions/0002-message-retention-ack.md「pane 消滅時の掃除」)。
                let failure_brief = format!(
                    "# agent-talk 未受領通知\n- from: system\n- to: {expected}\n- reply: 不要\n- original: #{}\n- reason: 受領報告されないまま{reason}\n\n## 元の依頼\n{}",
                    original.id, original.brief
                );
                let dispatch = self.state.dispatch(
                    &original.sender,
                    Origin::new("system", "system"),
                    failure_brief,
                    &expected,
                    |id| {
                        format!(
                            "[agent-talk] 未受領のまま終了: message #{} は受領報告されないまま{reason}ため回収されました。agent-talk read {id} で元の依頼内容を確認してください。",
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
                    self.rollback_notice(&original.sender, id, dispatch);
                    return false;
                };
                let message = stored.message.clone();
                // **通知の永続化と original の退役を1回の append にまとめる。**
                // 2回に分けると、その間のクラッシュ / append 失敗で original が
                // `Pending` のまま通知だけ残り、次の reconcile がもう1通作ってしまう。
                if let Err(error) = self.journal.append(&Record::Enqueue {
                    pane: original.sender.clone(),
                    message,
                    retires: Some(original.id),
                }) {
                    // 永続化前なので何も起きていない。再試行が唯一の通知を作る。
                    self.rollback_notice(&original.sender, id, dispatch);
                    error!(%error, "cannot persist failure notification");
                    return false;
                }
                self.state.ack(original.id);
                // ここから先は通知が durable。以後の失敗は queue 済み通知の
                // **配達の再試行**であって、新しい通知の生成ではない。
                self.deliver_notice(&original.sender, id, dispatch).await;
                continue;
            }
            // 通知先が居ない場合も、残った `Pending` を terminal `Acked` にする。
            if let Err(error) = self.journal.append(&Record::Consumed { id: original.id }) {
                error!(%error, id = original.id, "cannot retire unacked message");
                return false;
            }
            self.state.ack(original.id);
        }
        true
    }

    /// 永続化前に作りかけた通知を in-memory から取り消す。
    fn rollback_notice(&mut self, sender: &str, id: u64, dispatch: Dispatch) {
        if matches!(dispatch, Dispatch::Deliver(_)) {
            self.state.set_state(sender, AgentState::Idle);
        }
        self.state.discard_message(id);
    }

    /// durable な通知を配達する。失敗しても通知は queue に残り、次の turn-end で鳴る。
    /// **新しい通知は作らない。**
    async fn deliver_notice(&mut self, sender: &str, id: u64, dispatch: Dispatch) {
        if !matches!(dispatch, Dispatch::Deliver(_)) {
            return;
        }
        if let Err(error) = self.journal.append(&Record::State {
            pane: sender.to_owned(),
            state: AgentState::Busy,
        }) {
            warn!(%error, id, "cannot persist notice delivery state; leaving it queued");
            self.state.requeue_after_delivery_failure(sender, id);
            return;
        }
        let Some(stored) = self.state.message(id) else {
            error!(id, "failure notification body missing before delivery");
            self.state.set_state(sender, AgentState::Idle);
            return;
        };
        let bell = stored.message.bell.clone();
        if self.mux.deliver(sender, &bell).await.is_ok() {
            if let Err(error) = self.journal.append(&Record::Complete {
                pane: sender.to_owned(),
                id,
            }) {
                // journal 上は未配達のまま。再起動後に同じ通知が再配達される。
                warn!(%error, id, "cannot complete notice delivery; it will be retried");
                return;
            }
            self.state.complete_delivery(sender, id);
        } else {
            self.state.requeue_after_delivery_failure(sender, id);
            if let Err(error) = self.journal.append(&Record::State {
                pane: sender.to_owned(),
                state: AgentState::Idle,
            }) {
                warn!(%error, id, "cannot requeue notice; it stays queued in the journal");
            }
        }
    }

    async fn reconcile(&mut self, startup: bool) {
        let Ok(panes) = self.mux.panes().await else {
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

/// `read` / `ack` 系が共有する引数検証。
fn request_target(
    request: &Request,
    command: &str,
) -> std::result::Result<(u64, String), Response> {
    let Some(raw_id) = request.args.first() else {
        return Err(Response::error(help::usage(command)));
    };
    let Ok(id) = raw_id.trim_start_matches('#').parse::<u64>() else {
        return Err(Response::error(format!("message id が不正です: {raw_id}")));
    };
    let Some(pane) = request.pane.clone() else {
        return Err(Response::error(format!(
            "{command} は登録済みのtmux pane内で実行してください"
        )));
    };
    Ok((id, pane))
}

fn format_ids(ids: &[u64]) -> String {
    ids.iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(" ")
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
    use std::{collections::BTreeMap, path::PathBuf};

    use hyper::{Method, StatusCode};
    use tempfile::TempDir;

    use super::{
        Broker, HttpRoute, Journal, MailboxPageError, Request, SendIntent, SendOptions, WebAgent,
        capture_failure, classify_http, decode_path_segment, parse_mailbox_page,
        parse_mailbox_query, peer_uid_allowed, rfc3339, static_response,
    };
    use crate::{
        backend::{Multiplexer, PaneInfo},
        config::Config,
        state::AgentState,
        tmux::Tmux,
    };

    fn pane_info(pane_id: &str, agent: Option<&str>) -> PaneInfo {
        PaneInfo {
            session: "test".into(),
            window_id: "@0".into(),
            pane_id: pane_id.into(),
            cwd: "/tmp".into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            agent: agent.map(str::to_owned),
            backend: crate::backend::BackendKind::Tmux,
            status: None,
        }
    }

    /// tmux subprocess を起動しない in-process broker。
    /// journal の durability 契約を failpoint で検証するために必要。
    fn broker(dir: &TempDir, panes: Vec<PaneInfo>) -> Broker {
        let journal_path = dir.path().join("queue.journal");
        let (journal, state) = Journal::open(journal_path.clone()).unwrap();
        Broker {
            state,
            journal,
            mux: Multiplexer::new(Some(Tmux::scripted(panes)), None),
            config: Config {
                tmux_socket: Some(String::new()),
                herdr_socket: None,
                rpc_sockets: vec![PathBuf::new()],
                http_socket: PathBuf::new(),
                http_tcp: None,
                journal: journal_path,
                log: PathBuf::new(),
                queue_limit: 1000,
                log_level: "info".into(),
                skill_syntax: BTreeMap::new(),
                allowed_skills: None,
                allowed_sources: std::collections::BTreeSet::new(),
            },
        }
    }

    fn request(command: &str, pane: Option<&str>, args: &[&str]) -> Request {
        Request {
            command: command.into(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            stdin: String::new(),
            pane: pane.map(str::to_owned),
            send_options: None,
        }
    }

    fn send_request(pane: &str, to: &str, body: &str) -> Request {
        Request {
            command: "send-message-v1".into(),
            args: vec![to.to_owned()],
            stdin: body.to_owned(),
            pane: Some(pane.to_owned()),
            send_options: Some(SendOptions::default()),
        }
    }

    fn json(response: &super::Response) -> serde_json::Value {
        serde_json::from_str(response.stdout.trim()).unwrap_or_else(|error| {
            panic!(
                "not JSON ({error}): {:?} / {:?}",
                response.stdout, response.stderr
            )
        })
    }

    async fn registered_pair(dir: &TempDir) -> Broker {
        let mut broker = broker(
            dir,
            vec![
                pane_info("%1", Some("codex")),
                pane_info("%2", Some("claude")),
            ],
        );
        for (pane, name) in [("%1", "codex"), ("%2", "claude")] {
            let response = broker
                .handle(request("register", Some(pane), &[name]))
                .await;
            assert_eq!(response.code, 0, "{}", response.stderr);
        }
        broker
    }

    #[tokio::test]
    async fn mcp_rpcs_reject_every_unregistered_caller_before_touching_message_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        // %1 -> %2 に1通送り、未受領のまま残しておく。
        let sent = broker.handle(send_request("%1", "claude", "body")).await;
        assert_eq!(sent.code, 0, "{}", sent.stderr);
        let id = json(&sent)["id"].as_u64().unwrap();

        // %9 は登録されていない。4つの RPC すべてが拒否される。
        for (command, args) in [
            ("send-message-v1", vec!["claude"]),
            ("read-v1", vec![id.to_string().as_str()]),
            ("ack-v1", vec![id.to_string().as_str()]),
            ("peers-v1", vec![]),
        ] {
            let mut denied = request(command, Some("%9"), &args);
            denied.stdin = "body".into();
            denied.send_options = Some(SendOptions::default());
            let response = broker.handle(denied).await;
            assert_eq!(response.code, 1, "{command} must be rejected");
            assert!(
                response.stderr.contains("登録済みのagent pane"),
                "{command}: {}",
                response.stderr
            );
        }
        // 拒否は mutation を伴わない。message は Pending のまま。
        assert!(!broker.state.message(id).unwrap().acked);

        // 不在 ID への ack も、登録確認の前に成功を返さない。
        let unknown = broker
            .handle(request("ack-v1", Some("%9"), &["4242"]))
            .await;
        assert_eq!(unknown.code, 1);
        assert!(unknown.stderr.contains("登録済みのagent pane"));

        // 未登録 pane が human として送信できてしまわないこと。
        assert_eq!(broker.state.messages.len(), 1);
    }

    #[tokio::test]
    async fn the_legacy_cli_send_still_allows_human_callers() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let mut human = request("send", None, &["claude", "human", "body"]);
        human.pane = None;
        let response = broker.handle(human).await;
        assert_eq!(response.code, 0, "{}", response.stderr);
        assert!(response.stdout.starts_with("sent -> "), "{response:?}");
        let stored = broker.state.messages.values().next().unwrap();
        assert_eq!(stored.message.sender, "human");
        assert_eq!(stored.message.sender_label(), "human");
    }

    #[tokio::test]
    async fn send_message_v1_returns_versioned_json_for_both_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let sent = json(&broker.handle(send_request("%1", "claude", "body")).await);
        assert_eq!(sent["version"], 1);
        assert_eq!(sent["path"], "sent");
        assert_eq!(sent["to"], "%2");
        assert_eq!(sent["name"], "claude");

        let queued = json(&broker.handle(send_request("%1", "claude", "second")).await);
        assert_eq!(queued["version"], 1);
        assert_eq!(queued["path"], "queued");
        assert!(queued["id"].as_u64().unwrap() > sent["id"].as_u64().unwrap());
    }

    #[tokio::test]
    async fn read_v1_reports_the_identity_captured_at_send_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let id = json(&broker.handle(send_request("%1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();

        let read = json(
            &broker
                .handle(request("read-v1", Some("%2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(read["from"], "codex");
        assert_eq!(read["reply_to"], "%1");

        // %1 が別 agent に置き換わっても from は変わらず、reply_to は消える。
        broker
            .handle(request("register", Some("%1"), &["gemini"]))
            .await;
        let after = json(
            &broker
                .handle(request("read-v1", Some("%2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(after["from"], "codex", "現在のレジストリを引き直さない");
        assert!(after["reply_to"].is_null(), "新しい住人へ返信させない");
    }

    #[tokio::test]
    async fn captured_identity_survives_a_journal_restart() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let mut broker = registered_pair(&dir).await;
            json(&broker.handle(send_request("%1", "claude", "body")).await)["id"]
                .as_u64()
                .unwrap()
        };
        let mut restarted = broker(
            &dir,
            vec![
                pane_info("%1", Some("codex")),
                pane_info("%2", Some("claude")),
            ],
        );
        let read = json(
            &restarted
                .handle(request("read-v1", Some("%2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(read["from"], "codex");
        assert_eq!(read["reply_to"], "%1");
    }

    #[tokio::test]
    async fn an_ack_whose_journal_append_fails_stays_pending_and_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let id = json(&broker.handle(send_request("%1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();
        let journal_len = std::fs::metadata(&broker.config.journal).unwrap().len();

        // (a) append 失敗は error 応答になる。
        broker.journal.fail_next_appends(1);
        let failed = broker
            .handle(request("ack-v1", Some("%2"), &[&id.to_string()]))
            .await;
        assert_eq!(failed.code, 1, "{failed:?}");
        assert!(failed.stderr.contains("injected journal append failure"));

        // (b) message は Pending のまま可視。journal も1バイトも伸びていない。
        assert!(!broker.state.message(id).unwrap().acked);
        assert_eq!(broker.state.pending_to_me("%2"), vec![id]);
        assert_eq!(
            std::fs::metadata(&broker.config.journal).unwrap().len(),
            journal_len
        );

        // (c) 再 read が成功する。
        let reread = broker
            .handle(request("read-v1", Some("%2"), &[&id.to_string()]))
            .await;
        assert_eq!(reread.code, 0, "{}", reread.stderr);
        assert!(json(&reread)["body"].as_str().unwrap().contains("body"));

        // (d) 後から ack すると成功する。
        let acked = json(
            &broker
                .handle(request("ack-v1", Some("%2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(acked["outcome"], "acked");
        assert!(broker.state.message(id).unwrap().acked);
        assert!(broker.state.pending_to_me("%2").is_empty());
    }

    /// `%1` に届いた、`original` に対する未受領通知の数。
    fn notices_for(broker: &Broker, original: u64) -> usize {
        let marker = format!("- original: #{original}");
        broker
            .state
            .messages
            .values()
            .filter(|stored| stored.target_pane == "%1" && stored.message.brief.contains(&marker))
            .count()
    }

    /// `%2` が未受領メッセージを1通抱えたまま消える状況を作る。
    async fn pending_then_target_vanishes(dir: &TempDir) -> (Broker, u64) {
        let mut broker = registered_pair(dir).await;
        let original = json(&broker.handle(send_request("%1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            None,
        );
        (broker, original)
    }

    #[tokio::test]
    async fn a_failed_notice_append_creates_no_notice_and_blocks_the_removal() {
        let dir = tempfile::tempdir().unwrap();
        let (mut broker, original) = pending_then_target_vanishes(&dir).await;

        broker.journal.fail_next_appends(1);
        assert!(
            !broker.remove_agent("%2", "宛先が退出した").await,
            "通知を永続化できない間は remove しない"
        );
        assert!(
            broker.state.agents.contains_key("%2"),
            "早すぎる Remove が起きてはならない"
        );
        assert!(
            !broker.state.message(original).unwrap().acked,
            "original は Pending のまま"
        );
        assert_eq!(notices_for(&broker, original), 0, "通知はまだ作られない");

        // 通常の retry 経路 (reconcile) がちょうど1通だけ作る。
        broker.reconcile(false).await;
        assert_eq!(notices_for(&broker, original), 1);
        assert!(broker.state.message(original).unwrap().acked);
        assert!(!broker.state.agents.contains_key("%2"));

        // 何度 reconcile しても増えない。
        for _ in 0..3 {
            broker.reconcile(false).await;
        }
        assert_eq!(notices_for(&broker, original), 1);
    }

    #[tokio::test]
    async fn notice_delivery_failures_never_produce_a_second_notice() {
        let dir = tempfile::tempdir().unwrap();
        let (mut broker, original) = pending_then_target_vanishes(&dir).await;

        // Enqueue{retires} だけ通し、配達系の append をすべて失敗させる。
        broker.journal.fail_appends_after(1);
        broker.remove_agent("%2", "宛先が退出した").await;
        broker.journal.clear_failpoints();

        assert_eq!(notices_for(&broker, original), 1);
        assert!(
            broker.state.message(original).unwrap().acked,
            "通知が durable になった時点で original は退役している"
        );
        // 配達に失敗した通知は queue に残り、送信者の次の turn-end で鳴る。
        assert!(broker.state.agents["%1"].queue.len() == 1);

        for _ in 0..3 {
            broker.reconcile(false).await;
        }
        assert_eq!(
            notices_for(&broker, original),
            1,
            "reconcile が2通目を作ってはならない"
        );
    }

    #[tokio::test]
    async fn the_notice_and_the_retirement_survive_a_restart_as_one_unit() {
        let dir = tempfile::tempdir().unwrap();
        let original = {
            let (mut broker, original) = pending_then_target_vanishes(&dir).await;
            // 通知が durable になった直後にプロセスが落ちる状況を模す。
            broker.journal.fail_appends_after(1);
            broker.remove_agent("%2", "宛先が退出した").await;
            original
        };
        // 再起動: replay 後も original は退役済み、通知はちょうど1通。
        let mut restarted = broker(&dir, vec![pane_info("%1", Some("codex"))]);
        assert!(
            restarted
                .state
                .message(original)
                .is_none_or(|stored| stored.acked),
            "replay 後も original は Pending に戻らない"
        );
        assert_eq!(notices_for(&restarted, original), 1);
        restarted.reconcile(false).await;
        assert_eq!(
            notices_for(&restarted, original),
            1,
            "再起動後の reconcile も2通目を作らない"
        );
    }

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
