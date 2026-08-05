use std::{
    collections::BTreeMap,
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
use tracing::{debug, error, info, warn};

use crate::{
    backend::{BackendKind, Multiplexer, PaneInfo},
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

/// 配達完了からこの時間 ack が無ければ受領催促を送る
/// (docs/design.md「受領報告と保持」の催促契約)。
const NAG_AFTER: std::time::Duration = std::time::Duration::from_mins(1);

/// 受領催促の再送間隔。連打で pane を荒らさないための下限。
const NAG_COOLDOWN: std::time::Duration = std::time::Duration::from_mins(5);

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
            Self::Json => "send-message",
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
    Letter {
        source: String,
        target: String,
        body: String,
        reply: oneshot::Sender<std::result::Result<serde_json::Value, WebError>>,
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
    /// 兄弟 agent の判定 (session 同名の backend 跨ぎを混ぜない) に使う。
    backend: &'static str,
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

#[allow(clippy::too_many_lines)]
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
        herdr_misses: std::collections::HashMap::new(),
    };
    broker.startup().await;
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
                Ok(true) => {
                    failed_health_checks = 0;
                    broker.sync_herdr_registry().await;
                    broker.drain_queued().await;
                    broker.nag_unacked().await;
                }
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
        HttpRoute::MethodNotAllowed(allow) => response(
            StatusCode::METHOD_NOT_ALLOWED,
            "application/json",
            br#"{"error":"method_not_allowed"}"#,
        )
        .with_header(ALLOW, allow),
        HttpRoute::Letters => request_web_letter(request, &tx).await,
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

/// `POST /api/letters` — 唯一の書き込み route。
///
/// 無認証 TCP 面に出るため、browser からの cross-site simple request を弾く:
/// Content-Type は application/json のみ受理 (form/text-plain は 415)、CORS header は
/// 一切返さない (preflight が通らないので他 site の fetch は送れない)。
/// source の許可は daemon 側の allowlist (`@agent_talkd_allowed_sources`、既定 deny)
/// が最終判定する — UI の申告は信用しない。
async fn request_web_letter(
    request: HttpRequest<Incoming>,
    tx: &mpsc::Sender<Event>,
) -> HttpResponse<Full<Bytes>> {
    use http_body_util::BodyExt;

    #[derive(serde::Deserialize)]
    struct Letter {
        source: String,
        target: String,
        body: String,
    }

    let json_content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .map(str::trim)
                .is_some_and(|media| media.eq_ignore_ascii_case("application/json"))
        });
    if !json_content_type {
        return json_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "json_only");
    }
    let limited = http_body_util::Limited::new(request.into_body(), MAX_BODY_BYTES);
    let Ok(collected) = limited.collect().await else {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "letter_too_large");
    };
    let Ok(letter) = serde_json::from_slice::<Letter>(&collected.to_bytes()) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid_letter");
    };
    if letter.source.is_empty() || letter.target.is_empty() || letter.body.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "invalid_letter");
    }
    let (reply, receive) = oneshot::channel();
    if tx
        .send(Event::Http(HttpEvent::Letter {
            source: letter.source,
            target: letter.target,
            body: letter.body,
            reply,
        }))
        .await
        .is_err()
    {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "broker_unavailable");
    }
    match receive.await {
        Ok(Ok(accepted)) => json_response(StatusCode::OK, &accepted),
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
    MethodNotAllowed(&'static str),
    Letters,
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
    // 手紙の投函だけが唯一の書き込み route (ADR 0001 の GET 専用を user 指示で
    // 部分的に撤回)。それ以外の非 GET は従来どおり一切受けない。
    if path == "/api/letters" {
        return if method == Method::POST {
            HttpRoute::Letters
        } else {
            HttpRoute::MethodNotAllowed("POST")
        };
    }
    if method != Method::GET {
        return HttpRoute::MethodNotAllowed("GET");
    }
    match path {
        "/api/hello" => HttpRoute::Hello,
        "/api/who" => HttpRoute::Who,
        "/api/mailboxes" => HttpRoute::Mailboxes,
        path if path.starts_with("/api/agents/") && path.ends_with("/screen") => {
            let encoded = path
                .strip_prefix("/api/agents/")
                .and_then(|rest| rest.strip_suffix("/screen"))
                .unwrap_or_default();
            match decode_path_segment(encoded) {
                Some(pane) if valid_pane_id(&pane) => HttpRoute::Screen(pane),
                Some(_) => HttpRoute::NotFound,
                None => HttpRoute::BadRequest,
            }
        }
        path if let Some(encoded) = path.strip_prefix("/api/mailbox/") => {
            match decode_path_segment(encoded) {
                Some(mailbox) if is_safe_token(&mailbox) => HttpRoute::Mailbox(mailbox),
                Some(_) => HttpRoute::NotFound,
                None => HttpRoute::BadRequest,
            }
        }
        path if path.starts_with("/api/") => HttpRoute::ApiNotFound,
        // 撤去済みの旧 API prefix の墓標。SPA fallback (200) に落とすと、残存する旧
        // updater の health probe が「旧 API がまだ正常」と誤認して fail-open になる。
        // alias ではなく拒否 — 旧 path は API としてもう存在しない。
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

/// `register` command と同じ文字種で agent 名を検証する。herdr の native 名や
/// `@agent` mirror をそのまま登録すると、CLI では拒否される `bad/name` 等が
/// pull/startup 経由でだけ登録され、宛先文法 (`scope/name`) が壊れる。
fn usable_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn valid_pane_id(pane: &str) -> bool {
    // 両 backend の pane id 形式 (`%5` / `w1:p2`) を strict parser 1つで判定する。
    // tmux 形式だけを受けると herdr agent の Screen が実機で 404 になる。
    BackendKind::of(pane).is_some()
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
    /// herdr pull 同期の欠落 counter (pane → 連続欠落回数、memory のみ)。
    /// 成功 snapshot で識別が確認できるたびに 0 へ戻る。
    herdr_misses: std::collections::HashMap<String, u8>,
}

impl Broker {
    async fn handle_http_event(&mut self, event: HttpEvent) {
        match event {
            HttpEvent::Who { reply } => {
                let result = self.web_agents().await.map_err(|error| error.to_string());
                let _ = reply.send(result);
            }
            HttpEvent::Letter {
                source,
                target,
                body,
                reply,
            } => {
                let _ = reply.send(self.web_letter(source, target, body).await);
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
                    backend: pane.backend.as_str(),
                });
            }
        }
        Ok(agents)
    }

    /// HTTP からの手紙を既存の外部 mailbox 送信経路へ流す。
    ///
    /// 独自の送信実装を持たない — allowlist 判定・resolve・queue 上限・
    /// journal-first の永続化・配達/requeue は CLI の `send --from` と
    /// 完全に同一の経路である。拒否時は journal も state も変化しない。
    async fn web_letter(
        &mut self,
        source: String,
        target: String,
        body: String,
    ) -> std::result::Result<serde_json::Value, WebError> {
        let request = Request {
            command: "send-v2".into(),
            args: vec![target],
            stdin: body,
            pane: None,
            send_options: Some(SendOptions {
                from: Some(source),
                skill: None,
                no_reply: false,
            }),
        };
        let response = self
            .send(request, SendReport::Json)
            .await
            .map_err(|_| WebError::new(StatusCode::INTERNAL_SERVER_ERROR, "letter_failed"))?;
        if response.code == 0 {
            return serde_json::from_str(response.stdout.trim())
                .map_err(|_| WebError::new(StatusCode::INTERNAL_SERVER_ERROR, "letter_failed"));
        }
        // 表示は code に落とす (本文の日本語エラーは HTTP へ流さない)。
        if response.stderr.contains("許可されていません") {
            Err(WebError::new(StatusCode::FORBIDDEN, "source_not_allowed"))
        } else if response.stderr.contains("見つかりません") || response.stderr.contains("退出済み")
        {
            Err(WebError::new(StatusCode::NOT_FOUND, "target_not_found"))
        } else {
            Err(WebError::new(StatusCode::BAD_REQUEST, "letter_rejected"))
        }
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
        let result = match canonical_command(&request.command) {
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
            // MCP adapter 専用の RPC。ADR 0001 の起動時 contract 5 により
            // 未登録 pane からは何も出来ない。この gate は message 状態の分岐より前に置く。
            "send-message" | "read-message" | "ack-message" | "list-peers"
                if !self.caller_is_registered(request.pane.as_deref()) =>
            {
                Ok(Response::error(UNREGISTERED_CALLER))
            }
            "send-message" if request.send_options.is_some() => {
                self.send(request, SendReport::Json).await
            }
            "send-message" => Ok(Response::error("send-message optionsがありません")),
            "read-message" => Ok(self.read_json(&request)),
            "ack-message" => self.ack(&request),
            "list-peers" => self.peers_json(&request).await,
            "reply" => self.reply(request),
            "mailbox-list" => self.mailbox_list(&request),
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
        if !usable_agent_name(name) {
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
        if let Some(pane) = request.pane.as_deref()
            && matches!(BackendKind::of(pane), Some(BackendKind::Herdr))
        {
            // herdr の登録は native 検出の pull 同期が管理する。手動 unregister を
            // 受けても次の tick で再登録されて混乱するだけなので、明示的に断る。
            return Ok(Response::error(
                "herdr pane の登録は herdr の検出が管理します (外すには agent を停止してください)",
            ));
        }
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
        let delivered = match self.deliver_queued_head(&pane, "turn-end").await {
            Ok(delivered) => delivered,
            Err(reason) => return Ok(Response::error(reason)),
        };
        info!(%pane, source = "turn-end", queued = delivered, "turn ended");
        Ok(Response::ok(""))
    }

    /// queue 先頭を1件だけ配達する。turn-end と health tick の再配達が共有する
    /// 唯一の配送状態遷移 (成功: State Busy → Complete / 失敗: 同 ID requeue +
    /// State Idle)。呼び出し時点で pane は daemon memory 上 Idle であること。
    /// 返り値は「配達対象があったか」。journal へ書けない場合だけ Err。
    async fn deliver_queued_head(
        &mut self,
        pane: &str,
        source: &'static str,
    ) -> std::result::Result<bool, String> {
        // suspect 中は配達しない (占有者交代の疑いがある間、誤配の窓を開けない)。
        if self
            .state
            .agents
            .get(pane)
            .is_some_and(|agent| agent.suspect)
        {
            self.state.set_state(pane, AgentState::Idle);
            return Ok(false);
        }
        let message_id = loop {
            let Some(id) = self.state.turn_end(pane) else {
                break None;
            };
            if self.state.message(id).is_some() {
                break Some(id);
            }
            error!(%pane, id, "queued message body missing; skipping");
            if let Err(error) = self.journal.append(&Record::Complete {
                pane: pane.to_owned(),
                id,
            }) {
                return Err(format!(
                    "欠損メッセージのskipをjournalへ書き込めません: {error}"
                ));
            }
        };
        let Some(id) = message_id else {
            self.state.set_state(pane, AgentState::Idle);
            return Ok(false);
        };
        if let Err(error) = self.journal.append(&Record::State {
            pane: pane.to_owned(),
            state: AgentState::Busy,
        }) {
            self.state.requeue_after_delivery_failure(pane, id);
            return Err(format!("配達状態を journal に書き込めません: {error}"));
        }
        let Some(stored) = self.state.message(id) else {
            error!(%pane, id, "message body disappeared before delivery");
            self.state.set_state(pane, AgentState::Idle);
            return Err(format!(
                "message #{id} の本文が見つからないため配達を中止しました"
            ));
        };
        let bell = stored.message.bell.clone();
        if self.mux.deliver(pane, &bell).await.is_ok() {
            if let Err(error) = self.journal.append(&Record::Complete {
                pane: pane.to_owned(),
                id,
            }) {
                return Err(error.to_string());
            }
            self.state.complete_delivery(pane, id);
            info!(%pane, id, source, "delivered");
        } else {
            self.state.requeue_after_delivery_failure(pane, id);
            // 配達失敗は正常系 (相手が busy 判定など)。次の契機で再試行される。
            // silent にすると診断できないので debug には残す。
            debug!(%pane, id, source, "queued delivery failed; will retry");
            if let Err(error) = self.journal.append(&Record::State {
                pane: pane.to_owned(),
                state: AgentState::Idle,
            }) {
                return Err(error.to_string());
            }
        }
        Ok(true)
    }

    /// 観測に基づく登録の共通経路 (herdr snapshot / 起動時 mirror)。
    ///
    /// journal が durable になった時だけ memory を更新して true を返す。
    fn register_observed(&mut self, pane: &str, name: &str, source: &'static str) -> bool {
        if !usable_agent_name(name) {
            warn!(%pane, %name, source, "observed agent name is not addressable; skipped");
            return false;
        }
        if self
            .journal
            .append(&Record::Register {
                pane: pane.to_owned(),
                name: name.to_owned(),
                state: AgentState::Idle,
            })
            .is_err()
        {
            return false;
        }
        self.state
            .restore_agent(pane.to_owned(), name.to_owned(), AgentState::Idle);
        self.herdr_misses.remove(pane);
        info!(%pane, %name, source, "registered");
        true
    }

    /// herdr backend の登録を native identity から pull 同期する (2秒 tick)。
    ///
    /// tmux の登録は hook による opt-in + startup mirror 復旧のままだが、herdr は
    /// herdr 自身が agent を検出する側なので、hook を持たない agent (grok 等) も
    /// 現れた数秒後には peer になる。lifecycle は次の3規則:
    /// - **出現** (未登録 pane に identity): 即 pull 登録。idempotent。
    /// - **交代** (登録名と別の identity): 強い証拠なので debounce せず即 takeover —
    ///   旧登録を回収してから新 identity を登録する。回収の永続化に失敗したら
    ///   suspect のまま新登録もしない (旧宛の mail を新占有者へ流さない)。
    /// - **欠落** (pane 消滅 or identity 無し): 1回目で suspect (配達と message RPC
    ///   caller を遮断)、**成功 snapshot 2連続**で evict。同一 identity が戻れば解除。
    ///   snapshot の RPC 失敗は判定を一切進めない (不完全な証拠で消さない)。
    async fn sync_herdr_registry(&mut self) {
        let Some(snapshot) = self.mux.herdr_snapshot().await else {
            return;
        };
        let Ok(panes) = snapshot else {
            return;
        };
        // 宛先文法に載らない名前は identity 無しとして扱う (登録もしないし、
        // 既存登録の pane がその名前に変われば欠落と同じ suspect → evict を辿る)。
        let detected: std::collections::HashMap<String, Option<String>> = panes
            .into_iter()
            .map(|pane| {
                let agent = pane.agent.filter(|name| usable_agent_name(name));
                (pane.pane_id, agent)
            })
            .collect();
        // 出現・確認・交代。
        for (pane_id, identity) in &detected {
            let Some(name) = identity else { continue };
            match self.state.agents.get(pane_id) {
                None => {
                    self.register_observed(pane_id, name, "herdr-pull");
                }
                Some(agent) if agent.name == *name => {
                    self.herdr_misses.remove(pane_id);
                    if let Some(agent) = self.state.agents.get_mut(pane_id) {
                        agent.suspect = false;
                    }
                }
                Some(_) => {
                    // 交代: 旧登録の回収が durable になった後にだけ新 identity を登録。
                    // Register の append に失敗しても旧登録は既に消えており、
                    // 次 tick の None 分岐が同じ helper で durable 登録し直す。
                    if self
                        .remove_agent(pane_id, "宛先エージェントが入れ替わった")
                        .await
                    {
                        self.register_observed(pane_id, name, "herdr-pull");
                    } else if let Some(agent) = self.state.agents.get_mut(pane_id) {
                        // 回収を永続化できない間は疑い扱いのまま次 tick で再試行。
                        agent.suspect = true;
                    }
                }
            }
        }
        // 欠落 (登録済み herdr pane が identity 付きで見えない)。
        let missing: Vec<String> = self
            .state
            .agents
            .keys()
            .filter(|pane| {
                matches!(
                    crate::backend::BackendKind::of(pane),
                    Some(BackendKind::Herdr)
                ) && detected.get(*pane).is_none_or(Option::is_none)
            })
            .cloned()
            .collect();
        // まず欠落した全 pane を suspect にしてから evict する。同 tick で
        // 送信元と宛先が同時に欠落した場合、evict 側の未受領回収通知は
        // 送信元の suspect を見て queue に退避する — 処理順が map の順序に
        // 依存すると、先に evict された側の通知が silent に落ちる。
        let mut evictions = Vec::new();
        for pane in missing {
            let misses = self.herdr_misses.entry(pane.clone()).or_insert(0);
            *misses = misses.saturating_add(1);
            if *misses >= 2 {
                evictions.push(pane.clone());
            }
            if let Some(agent) = self.state.agents.get_mut(&pane) {
                agent.suspect = true;
            }
        }
        for pane in evictions {
            if self.remove_agent(&pane, "宛先エージェントが退出した").await {
                self.herdr_misses.remove(&pane);
            }
            // remove の永続化に失敗した場合は suspect のまま次 tick で再試行。
        }
    }

    /// Idle のまま queue が残っている pane の先頭を、health tick ごとに拾い直す。
    ///
    /// turn-end の一瞬は herdr の画面検出がまだ working を返すことがあり、
    /// その1回の失敗で再契機が無いと message が滞留する。idle の正の証拠が
    /// 次に得られた時点 (最大 tick 間隔 + API 時間) で同じ ID を再配達する。
    /// backend は限定しない — 新規 send が queue を追い越さない規則 (dispatch)
    /// と組むため、tmux 側の失敗残留もこの経路が掃く。
    async fn drain_queued(&mut self) {
        let candidates: Vec<String> = self
            .state
            .agents
            .iter()
            .filter(|(_, agent)| {
                agent.state == AgentState::Idle && !agent.queue.is_empty() && !agent.suspect
            })
            .map(|(pane, _)| pane.clone())
            .collect();
        for pane in candidates {
            if let Err(reason) = self.deliver_queued_head(&pane, "queued-retry").await {
                warn!(%pane, %reason, source = "queued-retry", "queued delivery attempt failed");
            }
        }
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
        // dispatch と同じ predicate (`defers_delivery`) で判定する。busy だけを
        // 見ると、suspect や queue 残留で queue 行きになる送信が上限を素通りする。
        if self
            .state
            .agents
            .get(&pane)
            .is_some_and(crate::state::Agent::defers_delivery)
            && self.state.queue_len(&pane) >= self.config.queue_limit
        {
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
                        "{skill_prefix}[agent-talk] {from_agent} から連絡が届きました。read_message {id} で本文を確認し、ack_message で受領報告してください。返信は不要です。"
                    )
                } else {
                    format!(
                        "{skill_prefix}[agent-talk] {from_agent} から依頼が届きました。read_message {id} で本文を確認し、作業前に ack_message で受領報告してから対応してください。"
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
                    also_retires: Vec::new(),
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
        // suspect (herdr の検出が途切れた疑い) の pane は、占有者が入れ替わって
        // いる可能性があるため message RPC を一時的に受けない (他人のメール読みの遮断)。
        pane.is_some_and(|pane| {
            self.state
                .agents
                .get(pane)
                .is_some_and(|agent| !agent.suspect)
        })
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

    /// 本文を返し、読了だけを記録する。受領報告が来るまで何度でも読める。
    fn read(&mut self, request: &Request) -> Response {
        let (id, pane) = match request_target(request, "read") {
            Ok(target) => target,
            Err(response) => return response,
        };
        let brief = match self.access(id, &pane) {
            MessageAccess::Pending(stored) => Ok(stored.message.brief.clone()),
            other => Err(other.reject_reason(id)),
        };
        match brief {
            Ok(brief) => {
                self.state.mark_read(id);
                Response::ok(brief)
            }
            Err(reason) => Response::error(reason),
        }
    }

    /// 構造化 read。MCP adapter の `read_message` が使う。
    fn read_json(&mut self, request: &Request) -> Response {
        let (id, pane) = match request_target(request, "read-message") {
            Ok(target) => target,
            Err(response) => return response,
        };
        let payload = match self.access(id, &pane) {
            MessageAccess::Pending(stored) => {
                // 送信時点で捕捉した名前を返す。現在のレジストリを引き直さない。
                let from = stored.message.sender_label().to_owned();
                // 返信先は、捕捉時と同じ identity で今も登録中の pane のときだけ。
                let reply_to = self.state.reply_target(&stored.message);
                Ok((from, reply_to, stored.message.brief.clone()))
            }
            other => Err(other.reject_reason(id)),
        };
        match payload {
            Ok((from, reply_to, body)) => {
                self.state.mark_read(id);
                Response::ok(format!(
                    "{}\n",
                    serde_json::json!({
                        "version": 1,
                        "id": id,
                        "from": from,
                        "reply_to": reply_to,
                        "body": body,
                    })
                ))
            }
            Err(reason) => Response::error(reason),
        }
    }

    /// 受領報告。journal の append + fsync が成功する前に可視性を `Acked` へ進めない。
    fn ack(&mut self, request: &Request) -> Result<Response> {
        let (id, pane) = match request_target(request, "ack-message") {
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
                "mailbox-list は外部caller (TMUX_PANEなし) 専用です",
            ));
        }
        let Some(mailbox) = request.args.first() else {
            return Ok(Response::error(help::usage("mailbox-list")));
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
        // 正式名称 `tmux/<scope>/<name>` / `herdr/<scope>/<name>`。tmux の session と
        // herdr の workspace label が同名でも一意に指せる (user 指定の正式形)。
        let (backend, rest) = match addr.split_once('/') {
            Some((prefix, rest)) if rest.contains('/') => match prefix {
                "tmux" => (Some(BackendKind::Tmux), rest),
                "herdr" => (Some(BackendKind::Herdr), rest),
                other => {
                    return Err(Response::error(format!(
                        "backend '{other}' は不明です (tmux/<scope>/<name> か herdr/<scope>/<name>)"
                    )));
                }
            },
            _ => (None, addr),
        };
        let (scope, name) = rest
            .split_once('/')
            .map_or((None, rest), |(scope, name)| (Some(scope), name));
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
        if let Some(backend) = backend {
            candidates.retain(|(pane, _)| pane.backend == backend);
        }
        if let Some(scope) = scope {
            // 主名 (tmux session / herdr label)、herdr の workspace_id alias
            // (`w2/codex` の後方互換)、cwd の basename のどれでも引ける。
            candidates.retain(|(pane, _)| {
                pane.session == scope
                    || pane.scope_alias.as_deref() == Some(scope)
                    || Path::new(&pane.cwd).file_name() == Some(OsStr::new(scope))
            });
        } else if let Some(self_pane) = self_pane
            && let Some(origin) = panes.iter().find(|pane| pane.pane_id == self_pane)
        {
            // bare 名は**自 backend 限定** — herdr label と tmux session の同名を
            // 暗黙に跨いで誤配しない。他 backend は scope か正式名称で明示する。
            candidates.retain(|(pane, _)| pane.backend == origin.backend);
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
                    "agent-talk: 宛先 '{addr}' の候補が複数あります。<scope>/<name> か、backend を跨ぐ同名なら tmux/<scope>/<name> ・ herdr/<scope>/<name> で指定してください:\n"
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
        // 送信元 pane ごとに1通へ集約する。10通未受領でも呼び鈴は送信元あたり1回
        // (BTreeMap の values は ID 昇順なので、group 内も ID 昇順が保たれる)。
        let mut groups: BTreeMap<String, Vec<Message>> = BTreeMap::new();
        for message in messages {
            groups
                .entry(message.sender.clone())
                .or_default()
                .push(message);
        }
        for (sender, originals) in groups {
            // suspect の送信元は snapshot に一瞬映らないだけかもしれない。
            // 通知は捨てず queue へ入れる (dispatch が suspect を Queued にする) —
            // 復帰すれば drain が届け、evict されれば通知ごと terminal に退役する。
            let sender_suspect = self
                .state
                .agents
                .get(&sender)
                .is_some_and(|agent| agent.suspect);
            let sender_target = BackendKind::of(&sender)
                .is_some()
                .then(|| {
                    self.state
                        .agents
                        .get(&sender)
                        .map(|agent| agent.name.clone())
                })
                .flatten()
                .filter(|expected| {
                    // 生存判定は backend 別 (`pane_backs_registration`)。tmux は
                    // 表示ミラーの drift で失敗通知を silent に退役させない。
                    // herdr は占有者が入れ替わった pane へ旧名宛て通知を送らない。
                    Some(sender.as_str()) != excluded_pane
                        && (sender_suspect
                            || panes.iter().any(|pane| {
                                pane.pane_id == sender && pane_backs_registration(pane, expected)
                            }))
                });
            let Some(expected) = sender_target else {
                // 通知先が居ない場合も、残った `Pending` を terminal `Acked` にする。
                for original in &originals {
                    if let Err(error) = self.journal.append(&Record::Consumed { id: original.id }) {
                        error!(%error, id = original.id, "cannot retire unacked message");
                        return false;
                    }
                    self.state.ack(original.id);
                }
                continue;
            };
            let listed = originals
                .iter()
                .map(|original| format!("#{}", original.id))
                .collect::<Vec<_>>()
                .join(" ");
            // 「配達されなかった」ではなく受領報告の欠如を表す文言にする
            // (docs/decisions/0002-message-retention-ack.md「pane 消滅時の掃除」)。
            let mut failure_brief = format!(
                "# agent-talk 未受領通知\n- from: system\n- to: {expected}\n- reply: 不要\n- original: {listed}\n- reason: 受領報告されないまま{reason}\n",
            );
            // 集約しても journal record を単発肥大させない: 収録する元本文の合計を
            // 送信本文と同じ 1MiB 上限に収め、超過分は ID を残して本文だけ省略する。
            let mut body_budget = MAX_BODY_BYTES;
            for original in &originals {
                if original.brief.len() <= body_budget {
                    body_budget -= original.brief.len();
                    let _ = write!(
                        failure_brief,
                        "\n## 元の依頼 #{}\n{}\n",
                        original.id, original.brief
                    );
                } else {
                    let _ = write!(
                        failure_brief,
                        "\n## 元の依頼 #{}\n(本文 {} bytes は集約通知の上限を超えるため省略)\n",
                        original.id,
                        original.brief.len()
                    );
                }
            }
            let dispatch = self.state.dispatch(
                &sender,
                Origin::new("system", "system"),
                failure_brief,
                &expected,
                |id| {
                    format!(
                        "[agent-talk] 未受領のまま終了: message {listed} は受領報告されないまま{reason}ため回収されました。read_message {id} で元の依頼内容を確認してください。",
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
                self.rollback_notice(&sender, id, dispatch);
                return false;
            };
            let message = stored.message.clone();
            // **通知の永続化と originals の退役を1回の append にまとめる。**
            // 2回に分けると、その間のクラッシュ / append 失敗で original が
            // `Pending` のまま通知だけ残り、次の reconcile がもう1通作ってしまう。
            let mut retire_ids = originals.iter().map(|original| original.id);
            if let Err(error) = self.journal.append(&Record::Enqueue {
                pane: sender.clone(),
                message,
                retires: retire_ids.next(),
                also_retires: retire_ids.collect(),
            }) {
                // 永続化前なので何も起きていない。再試行が唯一の通知を作る。
                self.rollback_notice(&sender, id, dispatch);
                error!(%error, "cannot persist failure notification");
                return false;
            }
            for original in &originals {
                self.state.ack(original.id);
            }
            // ここから先は通知が durable。以後の失敗は queue 済み通知の
            // **配達の再試行**であって、新しい通知の生成ではない。
            self.deliver_notice(&sender, id, dispatch).await;
        }
        true
    }

    /// 配達済みのまま受領報告が無い message について、宛先が idle のときだけ
    /// 受領催促の呼び鈴を送る。催促は message を新規作成しない
    /// (催促自体が受領報告の対象になる再帰を避ける)。
    /// タイマーは memory のみで、restart 後は配達時刻から数え直す。
    async fn nag_unacked(&mut self) {
        let now = tokio::time::Instant::now();
        // pane ごとに1回の呼び鈴へ集約する: (id, 読了済みか) の列。
        let mut due: BTreeMap<String, Vec<(u64, bool)>> = BTreeMap::new();
        for stored in self.state.messages.values() {
            if stored.acked || !stored.delivered {
                continue;
            }
            let Some(agent) = self.state.agents.get(&stored.target_pane) else {
                continue;
            };
            // busy 中・identity が変わった pane・suspect の pane には撃たない。
            if agent.state != AgentState::Idle
                || agent.name != stored.message.target_name
                || agent.suspect
            {
                continue;
            }
            let Some(delivered_at) = stored.delivered_at else {
                continue;
            };
            if now.duration_since(delivered_at) < NAG_AFTER {
                continue;
            }
            if let Some(last) = stored.last_nag_at
                && now.duration_since(last) < NAG_COOLDOWN
            {
                continue;
            }
            due.entry(stored.target_pane.clone())
                .or_default()
                .push((stored.message.id, stored.read));
        }
        for (pane, items) in due {
            let bell = nag_bell(&items);
            // 通常配達と同じく、鍵盤へ触れる前に Busy を永続化する (steer 安全)。
            if let Err(error) = self.journal.append(&Record::State {
                pane: pane.clone(),
                state: AgentState::Busy,
            }) {
                warn!(%error, %pane, "cannot persist nag state; skipping reminder");
                continue;
            }
            self.state.set_state(&pane, AgentState::Busy);
            let delivered = self.mux.deliver(&pane, &bell).await.is_ok();
            if !delivered {
                self.state.set_state(&pane, AgentState::Idle);
                if let Err(error) = self.journal.append(&Record::State {
                    pane: pane.clone(),
                    state: AgentState::Idle,
                }) {
                    warn!(%error, %pane, "cannot restore state after failed reminder");
                }
            }
            // 失敗時も cooldown は消費する。2秒ごとの health tick で連打しない。
            for (id, _) in &items {
                if let Some(stored) = self.state.messages.get_mut(id) {
                    stored.last_nag_at = Some(now);
                }
            }
            if delivered {
                let ids: Vec<u64> = items.iter().map(|(id, _)| *id).collect();
                info!(%pane, ?ids, source = "nag", "receipt reminder delivered");
            }
        }
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
        // 不完全な一覧を「pane 不在」と読むと、落ちている backend 側の生存登録を
        // 全滅させる。evict は全 backend が答えたときにだけ判定する。
        let Ok(panes) = self.mux.panes_from_all_backends().await else {
            return;
        };
        // tmux の evict 条件は **pane の消滅だけ**。daemon memory が唯一の真実であり、
        // `@agent` は表示用ミラーにすぎない (docs/design.md「状態と配送」)。
        // ミラーの欠落・不一致を根拠に生存 pane の登録を消すと、drift のたびに
        // その pane は理由の表示なく MCP 全拒否になる。
        // herdr は native identity の一致まで要求する (`pane_backs_registration`)。
        // herdr pane の生殺与奪は sync_herdr_registry (suspect + 2連続欠落) が
        // 所有する。ここで即 evict すると debounce を迂回してしまう。
        let stale: Vec<_> = self
            .state
            .agents
            .iter()
            .filter(|(pane_id, agent)| {
                matches!(BackendKind::of(pane_id), Some(BackendKind::Tmux))
                    && !panes.iter().any(|pane| {
                        pane.pane_id == **pane_id && pane_backs_registration(pane, &agent.name)
                    })
            })
            .map(|(pane, _)| pane.clone())
            .collect();
        for pane in stale {
            self.remove_agent(&pane, "宛先が不在の").await;
        }
        // 生存 pane のミラーが drift していたら、memory を正として修復する。
        // 修復失敗は表示の問題でしかないので warn に留める。
        // 対象は tmux だけ — herdr の agent 欄はミラーではなく native identity。
        for pane in &panes {
            if pane.backend != BackendKind::Tmux {
                continue;
            }
            if let Some(agent) = self.state.agents.get(&pane.pane_id)
                && pane.agent.as_deref() != Some(agent.name.as_str())
            {
                let name = agent.name.clone();
                if let Err(error) = self
                    .mux
                    .set_option(&pane.pane_id, "@agent", Some(&name))
                    .await
                {
                    warn!(pane = %pane.pane_id, %error, source = "reconcile", "mirror repair failed");
                }
            }
        }
        if !startup {
            return;
        }
        for pane in &panes {
            if self.state.agents.contains_key(&pane.pane_id) {
                continue;
            }
            let Some(name) = pane.agent.clone() else {
                continue;
            };
            self.register_observed(&pane.pane_id, &name, "startup-mirror");
        }
    }

    /// 起動時の registry 復旧。mirror 復旧 (reconcile) の後、要求の受付前に
    /// herdr snapshot と必ず1回同期する — journal の identity が古いまま最初の
    /// health tick まで addressable だと、旧名宛の呼び鈴を pane の新しい
    /// 占有者へ送ってしまう (最大2秒の誤配窓)。
    async fn startup(&mut self) {
        self.reconcile(true).await;
        self.sync_herdr_registry().await;
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

/// pane がその登録の生存根拠になるか (backend 別)。
///
/// tmux の `PaneInfo.agent` は daemon 自身が書く表示ミラー (`@agent`) なので、
/// 欠落・不一致は drift にすぎず pane の存在だけで生存とみなす (ミラーは修復する)。
/// herdr の `PaneInfo.agent` は herdr が返す **native identity** で、pane ID は
/// 位置依存のため占有者が入れ替わりうる。identity の一致まで要求しないと、
/// 旧名宛ての通知を新しい占有者へ送り続けることになる。
fn pane_backs_registration(pane: &PaneInfo, registered_name: &str) -> bool {
    match pane.backend {
        BackendKind::Tmux => true,
        BackendKind::Herdr => pane.agent.as_deref() == Some(registered_name),
    }
}

/// 先受け版号時代の旧 wire 名を canonical 名へ正規化する互換 alias。
/// daemon は update.timer により agent session の途中でも差し替わるため、
/// 稼働中の旧 adapter を壊さないための期限付き措置。次の minor で削除する。
fn canonical_command(command: &str) -> &str {
    match command {
        "peers-v1" => "list-peers",
        "read-v1" => "read-message",
        "ack-v1" => "ack-message",
        "send-message-v1" => "send-message",
        "mailbox-list-v1" => "mailbox-list",
        other => other,
    }
}

/// 受領催促の呼び鈴文言。読了済みと未読で促す操作を変える
/// (user 原文「読んだんじゃないの？早くackしてくれ、読んでないなら読んでくれ」)。
fn nag_bell(items: &[(u64, bool)]) -> String {
    let list = |read: bool| {
        items
            .iter()
            .filter(|(_, item_read)| *item_read == read)
            .map(|(id, _)| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let read_ids = list(true);
    let unread_ids = list(false);
    if unread_ids.is_empty() {
        format!(
            "[agent-talk] 受領催促: message {read_ids} は読まれたまま受領報告がありません。ack_message で受領報告してください。"
        )
    } else if read_ids.is_empty() {
        format!(
            "[agent-talk] 受領催促: message {unread_ids} が未読のままです。read_message で本文を確認し、ack_message で受領報告してください。"
        )
    } else {
        format!(
            "[agent-talk] 受領催促: message {read_ids} は未 ack、{unread_ids} は未読です。read_message / ack_message で処理してください。"
        )
    }
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
                    "send_message で '{}' 宛に返信する (pane ID 指定は曖昧にならない)",
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
        Broker, HttpRoute, Journal, MAX_BODY_BYTES, MailboxPageError, Request, SendIntent,
        SendOptions, WebAgent, capture_failure, classify_http, decode_path_segment,
        parse_mailbox_page, parse_mailbox_query, peer_uid_allowed, rfc3339, static_response,
    };
    use crate::{
        backend::{Multiplexer, PaneInfo},
        config::Config,
        herdr::Herdr,
        state::AgentState,
        tmux::Tmux,
    };

    fn pane_info(pane_id: &str, agent: Option<&str>) -> PaneInfo {
        PaneInfo {
            session: "test".into(),
            scope_alias: None,
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
            herdr_misses: std::collections::HashMap::new(),
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
            command: "send-message".into(),
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
            ("send-message", vec!["claude"]),
            ("read-message", vec![id.to_string().as_str()]),
            ("ack-message", vec![id.to_string().as_str()]),
            ("list-peers", vec![]),
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
            .handle(request("ack-message", Some("%9"), &["4242"]))
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
    async fn send_message_rpc_returns_versioned_json_for_both_paths() {
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
                .handle(request("read-message", Some("%2"), &[&id.to_string()]))
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
                .handle(request("read-message", Some("%2"), &[&id.to_string()]))
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
                .handle(request("read-message", Some("%2"), &[&id.to_string()]))
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
            .handle(request("ack-message", Some("%2"), &[&id.to_string()]))
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
            .handle(request("read-message", Some("%2"), &[&id.to_string()]))
            .await;
        assert_eq!(reread.code, 0, "{}", reread.stderr);
        assert!(json(&reread)["body"].as_str().unwrap().contains("body"));

        // (d) 後から ack すると成功する。
        let acked = json(
            &broker
                .handle(request("ack-message", Some("%2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(acked["outcome"], "acked");
        assert!(broker.state.message(id).unwrap().acked);
        assert!(broker.state.pending_to_me("%2").is_empty());
    }

    /// `%1` に届いた、`original` に対する未受領通知の数。
    fn notices_for(broker: &Broker, original: u64) -> usize {
        notices_addressed_to(broker, "%1", original)
    }

    fn notices_addressed_to(broker: &Broker, pane: &str, original: u64) -> usize {
        let marker = format!("- original: #{original}");
        broker
            .state
            .messages
            .values()
            .filter(|stored| stored.target_pane == pane && stored.message.brief.contains(&marker))
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

    /// scripted tmux が配達した (pane, bell) の記録。
    fn bells(broker: &Broker) -> Vec<(String, String)> {
        broker.mux.tmux().unwrap().delivered.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn reclaimed_messages_collapse_into_one_notice_per_sender() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(
            &dir,
            vec![
                pane_info("%1", Some("codex")),
                pane_info("%2", Some("claude")),
                pane_info("%3", Some("cursor")),
            ],
        );
        for (pane, name) in [("%1", "codex"), ("%2", "claude"), ("%3", "cursor")] {
            let response = broker
                .handle(request("register", Some(pane), &[name]))
                .await;
            assert_eq!(response.code, 0, "{}", response.stderr);
        }
        let mut from_codex = Vec::new();
        for body in ["first", "second", "third"] {
            from_codex.push(
                json(&broker.handle(send_request("%1", "claude", body)).await)["id"]
                    .as_u64()
                    .unwrap(),
            );
        }
        let from_cursor = json(&broker.handle(send_request("%3", "claude", "fourth")).await)["id"]
            .as_u64()
            .unwrap();

        // %2 が4通 (未受領) を抱えたまま消える。
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![
                pane_info("%1", Some("codex")),
                pane_info("%3", Some("cursor")),
            ])),
            None,
        );
        let before = bells(&broker).len();
        assert!(
            broker
                .remove_agent("%2", "宛先エージェントが退出した")
                .await
        );

        // 通知は送信元ごとにちょうど1通 (bell も1回ずつ)。
        let to_codex: Vec<_> = broker
            .state
            .messages
            .values()
            .filter(|stored| stored.target_pane == "%1")
            .collect();
        assert_eq!(to_codex.len(), 1, "codex への通知は1通に集約される");
        let brief = &to_codex[0].message.brief;
        for id in &from_codex {
            assert!(
                brief.contains(&format!("## 元の依頼 #{id}")),
                "回収された全 message の本文を含む: {brief}"
            );
        }
        assert!(brief.contains(&format!(
            "- original: #{} #{} #{}",
            from_codex[0], from_codex[1], from_codex[2]
        )));
        assert!(!brief.contains(&format!("#{from_cursor}")));

        let to_cursor: Vec<_> = broker
            .state
            .messages
            .values()
            .filter(|stored| stored.target_pane == "%3")
            .collect();
        assert_eq!(to_cursor.len(), 1, "cursor への通知は独立に1通");
        assert!(to_cursor[0].message.brief.contains("## 元の依頼 #"));

        let new_bells: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(new_bells.len(), 2, "send-keys は送信元 pane ごとに1回だけ");
        let codex_bell = &new_bells
            .iter()
            .find(|(pane, _)| pane == "%1")
            .expect("codex bell")
            .1;
        assert!(
            codex_bell.contains(&format!(
                "message #{} #{} #{}",
                from_codex[0], from_codex[1], from_codex[2]
            )),
            "{codex_bell}"
        );
        assert!(
            codex_bell.contains("read_message") && !codex_bell.contains("agent-talk read"),
            "呼び鈴は MCP 形式で案内する: {codex_bell}"
        );

        // originals は全件退役済み。
        for id in from_codex.iter().chain([&from_cursor]) {
            assert!(broker.state.message(*id).unwrap().acked);
        }
    }

    #[tokio::test]
    async fn a_batched_notice_retires_every_original_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let ids = {
            let mut broker = registered_pair(&dir).await;
            let ids: Vec<u64> = {
                let mut ids = Vec::new();
                for body in ["first", "second"] {
                    ids.push(
                        json(&broker.handle(send_request("%1", "claude", body)).await)["id"]
                            .as_u64()
                            .unwrap(),
                    );
                }
                ids
            };
            broker.mux = Multiplexer::new(
                Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
                None,
            );
            assert!(
                broker
                    .remove_agent("%2", "宛先エージェントが退出した")
                    .await
            );
            ids
        };
        // 再起動 (replay): also_retires の分も含めて Pending へ戻らない。
        let restarted = broker(&dir, vec![pane_info("%1", Some("codex"))]);
        for id in ids {
            assert!(
                restarted
                    .state
                    .message(id)
                    .is_none_or(|stored| stored.acked),
                "message #{id} が replay で Pending に戻ってはならない"
            );
        }
        let notices = restarted
            .state
            .messages
            .values()
            .filter(|stored| stored.target_pane == "%1" && !stored.acked)
            .count();
        assert_eq!(notices, 1, "通知は restart 後も1通のまま");
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_but_unacked_message_draws_an_ack_reminder_after_a_minute() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let id = json(&broker.handle(send_request("%1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();
        // 配達済み → %2 が読んだが ack せず turn を終えた。
        let read = broker
            .handle(request("read-message", Some("%2"), &[&id.to_string()]))
            .await;
        assert_eq!(read.code, 0, "{}", read.stderr);
        broker.turn_end(Some("%2".into())).await.unwrap();

        let before = bells(&broker).len();
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before, "1分経過前は催促しない");

        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        broker.nag_unacked().await;
        let after: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(after.len(), 1, "催促はちょうど1回");
        let (pane, bell) = &after[0];
        assert_eq!(pane, "%2", "催促は受信者へ送る");
        assert!(
            bell.contains("受領催促") && bell.contains(&format!("#{id}")),
            "{bell}"
        );
        assert!(
            bell.contains("読まれたまま") && bell.contains("ack_message"),
            "読了済みには ack を促す: {bell}"
        );
        assert!(
            broker.state.is_busy("%2"),
            "催促も通常配達と同じく Busy を先に立てる"
        );

        // busy の間も、idle へ戻って cooldown 中も、追い討ちしない。
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        broker.nag_unacked().await;
        broker.turn_end(Some("%2".into())).await.unwrap();
        broker.nag_unacked().await;
        assert_eq!(
            bells(&broker).len(),
            before + 1,
            "cooldown 中は再催促しない"
        );

        // cooldown が明ければもう一度だけ催促する。
        tokio::time::advance(std::time::Duration::from_mins(5)).await;
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before + 2);

        // ack すれば止まる。
        broker.turn_end(Some("%2".into())).await.unwrap();
        let acked = broker
            .handle(request("ack-message", Some("%2"), &[&id.to_string()]))
            .await;
        assert_eq!(acked.code, 0, "{}", acked.stderr);
        tokio::time::advance(std::time::Duration::from_mins(10)).await;
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before + 2, "ack 後は催促しない");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unread_message_is_nagged_to_read_and_a_busy_target_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let id = json(&broker.handle(send_request("%1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();

        // 配達直後の %2 は Busy のまま。催促は撃たれない。
        let before = bells(&broker).len();
        tokio::time::advance(std::time::Duration::from_mins(2)).await;
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before, "busy 中は催促しない");

        // idle に戻ると未読向けの文言で催促される。
        broker.turn_end(Some("%2".into())).await.unwrap();
        broker.nag_unacked().await;
        let after: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(after.len(), 1);
        let bell = &after[0].1;
        assert!(
            bell.contains("未読")
                && bell.contains("read_message")
                && bell.contains(&format!("#{id}")),
            "未読には read を促す: {bell}"
        );
    }

    #[tokio::test]
    async fn a_letter_over_http_reuses_the_external_send_gate() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;

        // 既定 deny: allowlist が空なら 403 で、journal にも state にも痕跡が無い。
        let error = broker
            .web_letter("mobile".into(), "claude".into(), "letter body".into())
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.code, "source_not_allowed");
        assert_eq!(broker.state.messages.len(), 0, "拒否は mutation を残さない");

        // 許可すると CLI の send --from と同一経路で受理される (versioned JSON)。
        broker.config.allowed_sources.insert("mobile".into());
        let accepted = broker
            .web_letter("mobile".into(), "claude".into(), "letter body".into())
            .await
            .unwrap();
        assert_eq!(accepted["version"], 1);
        assert_eq!(accepted["path"], "sent");
        assert_eq!(accepted["name"], "claude");
        let id = accepted["id"].as_u64().unwrap();
        assert!(broker.state.message(id).is_some(), "journal-first で永続化");
        assert_eq!(
            broker.state.mailbox_events("mobile", None, 10).len(),
            1,
            "mailbox 履歴に out event が残り、LettersPanel から見える"
        );

        // 実在しない宛先は 404。
        let error = broker
            .web_letter("mobile".into(), "ghost".into(), "x".into())
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cross_backend_scopes_resolve_by_label_alias_and_formal_name() {
        let dir = tempfile::tempdir().unwrap();
        // tmux session "settings" と herdr label "settings" が同名で共存し、
        // herdr には label "knowledge" (workspace_id w2) も居る、という実在の配置。
        let tmux_codex = PaneInfo {
            session: "settings".into(),
            ..pane_info("%1", Some("codex"))
        };
        let tmux_claude = PaneInfo {
            session: "settings".into(),
            ..pane_info("%2", Some("claude"))
        };
        let herdr_codex = PaneInfo {
            session: "settings".into(),
            scope_alias: Some("w1".into()),
            window_id: "w1:t1".into(),
            backend: crate::backend::BackendKind::Herdr,
            ..pane_info("w1:p1", Some("codex"))
        };
        let herdr_claude = PaneInfo {
            session: "knowledge".into(),
            scope_alias: Some("w2".into()),
            window_id: "w2:t1".into(),
            cwd: "/home/miyabi/.local/share/arona-knowledge".into(),
            backend: crate::backend::BackendKind::Herdr,
            ..pane_info("w2:p1", Some("claude"))
        };
        let mut broker = broker(
            &dir,
            vec![tmux_codex, tmux_claude, herdr_codex, herdr_claude],
        );
        for (pane, name) in [
            ("%1", "codex"),
            ("%2", "claude"),
            ("w1:p1", "codex"),
            ("w2:p1", "claude"),
        ] {
            let response = broker
                .handle(request("register", Some(pane), &[name]))
                .await;
            assert_eq!(response.code, 0, "{}", response.stderr);
        }

        // label で backend を跨いで引ける (user 目的の本丸)。
        let resolved = broker
            .resolve("knowledge/claude", Some("%1"))
            .await
            .unwrap();
        assert_eq!(resolved.0, "w2:p1");
        // workspace_id alias の後方互換。
        let resolved = broker.resolve("w2/claude", Some("%1")).await.unwrap();
        assert_eq!(resolved.0, "w2:p1");
        // cwd basename でも従来どおり。
        let resolved = broker
            .resolve("arona-knowledge/claude", Some("%1"))
            .await
            .unwrap();
        assert_eq!(resolved.0, "w2:p1");

        // backend を跨いだ同名 scope は曖昧 fail + 正式名称の案内。
        let error = broker
            .resolve("settings/codex", Some("%2"))
            .await
            .unwrap_err();
        assert!(
            error.stderr.contains("tmux/<scope>/<name>"),
            "{}",
            error.stderr
        );
        // 正式名称なら一意。
        let resolved = broker
            .resolve("tmux/settings/codex", Some("%2"))
            .await
            .unwrap();
        assert_eq!(resolved.0, "%1");
        let resolved = broker
            .resolve("herdr/settings/codex", Some("%2"))
            .await
            .unwrap();
        assert_eq!(resolved.0, "w1:p1");
        // 不明な backend は明示エラー。
        let error = broker
            .resolve("wayland/settings/codex", Some("%2"))
            .await
            .unwrap_err();
        assert!(error.stderr.contains("不明"), "{}", error.stderr);

        // bare 名は自 backend 限定: tmux の %2 から "codex" は herdr の
        // settings (w1:p1) を拾わず、同 session の %1 に一意化する。
        let resolved = broker.resolve("codex", Some("%2")).await.unwrap();
        assert_eq!(resolved.0, "%1");
        // herdr 側からの bare 名も tmux を拾わない (同 session 候補なし = 不在)。
        let error = broker.resolve("claude", Some("w1:p1")).await.unwrap_err();
        assert!(error.stderr.contains("見つかりません"), "{}", error.stderr);
    }

    fn herdr_agent_pane(pane_id: &str, agent: Option<&str>) -> crate::herdr::HerdrPane {
        crate::herdr::HerdrPane {
            pane_id: pane_id.into(),
            terminal_id: format!("term_{pane_id}"),
            workspace_id: "w1".into(),
            workspace_label: Some("settings".into()),
            tab_id: "w1:t1".into(),
            cwd: "/tmp".into(),
            agent: agent.map(str::to_owned),
            status: crate::herdr::AgentStatus::Idle,
        }
    }

    /// scripted herdr の一覧を差し替える (tick の合間の変化を模す)。
    fn set_herdr_panes(broker_ref: &Broker, panes: Vec<crate::herdr::HerdrPane>) {
        *broker_ref
            .mux
            .herdr()
            .unwrap()
            .scripted
            .as_ref()
            .unwrap()
            .lock()
            .unwrap() = panes;
    }

    #[tokio::test]
    async fn herdr_agents_are_pulled_in_without_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(&dir, vec![pane_info("%9", Some("cursor"))]);
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%9", Some("cursor"))])),
            Some(Herdr::scripted(vec![
                herdr_agent_pane("w1:p7", Some("grok")),
                herdr_agent_pane("w1:p8", None),
            ])),
        );
        broker.sync_herdr_registry().await;
        assert!(
            broker.state.agents.contains_key("w1:p7"),
            "hook を持たない agent が pull 登録される"
        );
        assert_eq!(broker.state.agents["w1:p7"].name, "grok");
        assert!(
            !broker.state.agents.contains_key("w1:p8"),
            "identity の無い pane は登録しない"
        );
        assert!(
            !broker.state.agents.contains_key("%9"),
            "tmux の @agent mirror からは稼働中に登録しない"
        );
        // idempotent: 何度回しても journal に Register が増えない。
        let journal_path = broker.config.journal.clone();
        let registers = move || {
            std::fs::read_to_string(&journal_path)
                .unwrap()
                .lines()
                .filter(|line| line.contains("register") && line.contains("w1:p7"))
                .count()
        };
        let before = registers();
        broker.sync_herdr_registry().await;
        broker.sync_herdr_registry().await;
        assert_eq!(registers(), before, "再同期で Register が増殖しない");
    }

    #[tokio::test]
    async fn a_flapping_herdr_detection_suspects_then_evicts() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            Some(Herdr::scripted(vec![herdr_agent_pane(
                "w1:p7",
                Some("grok"),
            )])),
        );
        broker.sync_herdr_registry().await;
        // %1 から grok 宛てに送る (scripted herdr は配達 socket を持たないので queue 行き)。
        let sent = json(&broker.handle(send_request("%1", "w1:p7", "for grok")).await);
        let pending_id = sent["id"].as_u64().unwrap();

        // 欠落1回目: suspect — 登録は残るが、配達も message RPC も止まる。
        set_herdr_panes(&broker, vec![herdr_agent_pane("w1:p7", None)]);
        broker.sync_herdr_registry().await;
        assert!(
            broker.state.agents.contains_key("w1:p7"),
            "1回の欠落では消えない"
        );
        assert!(broker.state.agents["w1:p7"].suspect);
        let refused = broker
            .handle(request("read-message", Some("w1:p7"), &["1"]))
            .await;
        assert!(
            refused.stderr.contains("登録済みのagent pane"),
            "suspect 中の caller は message RPC を使えない: {}",
            refused.stderr
        );
        let before = bells(&broker).len();
        broker.drain_queued().await;
        assert_eq!(bells(&broker).len(), before, "suspect 中は配達しない");

        // 同一 identity が戻れば解除、mail は無傷。
        set_herdr_panes(&broker, vec![herdr_agent_pane("w1:p7", Some("grok"))]);
        broker.sync_herdr_registry().await;
        assert!(!broker.state.agents["w1:p7"].suspect);
        assert!(!broker.state.message(pending_id).unwrap().acked);

        // 2連続の欠落で evict + 集約回収。
        set_herdr_panes(&broker, vec![herdr_agent_pane("w1:p7", None)]);
        broker.sync_herdr_registry().await;
        broker.sync_herdr_registry().await;
        assert!(!broker.state.agents.contains_key("w1:p7"));
        assert_eq!(
            notices_for(&broker, pending_id),
            1,
            "未受領は送信元へ回収通知される"
        );
    }

    #[tokio::test]
    async fn an_identity_swap_takes_over_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            Some(Herdr::scripted(vec![herdr_agent_pane(
                "w1:p7",
                Some("grok"),
            )])),
        );
        broker.sync_herdr_registry().await;
        let sent = json(&broker.handle(send_request("%1", "w1:p7", "for grok")).await);
        let pending_id = sent["id"].as_u64().unwrap();

        // 別 identity は強い証拠: debounce せず旧登録を回収して新 identity を登録。
        set_herdr_panes(&broker, vec![herdr_agent_pane("w1:p7", Some("gemini"))]);
        broker.sync_herdr_registry().await;
        assert_eq!(broker.state.agents["w1:p7"].name, "gemini");
        assert!(!broker.state.agents["w1:p7"].suspect);
        assert_eq!(
            notices_for(&broker, pending_id),
            1,
            "旧宛の mail は回収される"
        );
    }

    #[tokio::test]
    async fn snapshot_errors_freeze_the_pull_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            Some(Herdr::scripted(vec![herdr_agent_pane(
                "w1:p7",
                Some("grok"),
            )])),
        );
        broker.sync_herdr_registry().await;
        // snapshot が取れない間は、何度 tick が回っても判定を進めない。
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            Some(Herdr::new("/nonexistent/herdr.sock".into())),
        );
        for _ in 0..3 {
            broker.sync_herdr_registry().await;
        }
        assert!(broker.state.agents.contains_key("w1:p7"));
        assert!(!broker.state.agents["w1:p7"].suspect);
    }

    #[tokio::test]
    async fn startup_syncs_a_swapped_herdr_identity_before_serving() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut broker = broker(&dir, vec![]);
            broker.mux = Multiplexer::new(
                Some(Tmux::scripted(vec![])),
                Some(Herdr::scripted(vec![herdr_agent_pane(
                    "w1:p7",
                    Some("grok"),
                )])),
            );
            broker.sync_herdr_registry().await;
            assert_eq!(broker.state.agents["w1:p7"].name, "grok");
        }
        // 再起動: journal は grok を復元するが、pane の占有者は既に gemini。
        let mut broker = broker(&dir, vec![]);
        assert_eq!(
            broker.state.agents["w1:p7"].name, "grok",
            "journal 復元は旧 identity"
        );
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![])),
            Some(Herdr::scripted(vec![herdr_agent_pane(
                "w1:p7",
                Some("gemini"),
            )])),
        );
        broker.startup().await;
        assert_eq!(
            broker.state.agents["w1:p7"].name, "gemini",
            "受付開始前に snapshot と同期し、最初の tick までの誤配窓を作らない"
        );
    }

    #[tokio::test]
    async fn a_suspect_target_still_honors_the_queue_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        broker.config.queue_limit = 1;
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            Some(Herdr::scripted(vec![herdr_agent_pane(
                "w1:p7",
                Some("grok"),
            )])),
        );
        broker.sync_herdr_registry().await;
        set_herdr_panes(&broker, vec![herdr_agent_pane("w1:p7", None)]);
        broker.sync_herdr_registry().await;
        assert!(broker.state.agents["w1:p7"].suspect);

        let first = broker.handle(send_request("%1", "w1:p7", "queued")).await;
        assert_eq!(first.code, 0, "{}", first.stderr);
        let journal_path = broker.config.journal.clone();
        let lines = move || {
            std::fs::read_to_string(&journal_path)
                .unwrap()
                .lines()
                .count()
        };
        let before = lines();
        let second = broker.handle(send_request("%1", "w1:p7", "over")).await;
        assert!(
            second.stderr.contains("キュー保持上限"),
            "suspect 宛でも上限は効く: {}",
            second.stderr
        );
        assert_eq!(lines(), before, "拒否は journal に何も書かない");
        assert_eq!(broker.state.queue_len("w1:p7"), 1);
    }

    #[tokio::test]
    async fn a_herdr_sender_receives_the_failure_notice() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            Some(Herdr::scripted(vec![
                herdr_agent_pane("w1:p6", Some("grok")),
                herdr_agent_pane("w1:p7", Some("gemini")),
            ])),
        );
        broker.sync_herdr_registry().await;
        let sent = json(
            &broker
                .handle(send_request("w1:p6", "w1:p7", "question"))
                .await,
        );
        let pending_id = sent["id"].as_u64().unwrap();

        set_herdr_panes(
            &broker,
            vec![
                herdr_agent_pane("w1:p6", Some("grok")),
                herdr_agent_pane("w1:p7", None),
            ],
        );
        broker.sync_herdr_registry().await;
        broker.sync_herdr_registry().await;
        assert!(!broker.state.agents.contains_key("w1:p7"));
        assert_eq!(
            notices_addressed_to(&broker, "w1:p6", pending_id),
            1,
            "herdr の送信元にも回収通知が返る"
        );
    }

    #[tokio::test]
    async fn a_suspect_sender_keeps_its_failure_notice_queued_until_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            Some(Herdr::scripted(vec![
                herdr_agent_pane("w1:p6", Some("grok")),
                herdr_agent_pane("w1:p7", Some("gemini")),
            ])),
        );
        broker.sync_herdr_registry().await;
        let sent = json(
            &broker
                .handle(send_request("w1:p6", "w1:p7", "question"))
                .await,
        );
        let pending_id = sent["id"].as_u64().unwrap();

        // 宛先が先に1回欠落し、次の tick で送信元も一瞬映らないまま宛先が evict。
        set_herdr_panes(
            &broker,
            vec![
                herdr_agent_pane("w1:p6", Some("grok")),
                herdr_agent_pane("w1:p7", None),
            ],
        );
        broker.sync_herdr_registry().await;
        set_herdr_panes(&broker, vec![]);
        broker.sync_herdr_registry().await;
        assert!(!broker.state.agents.contains_key("w1:p7"));
        assert!(broker.state.agents["w1:p6"].suspect);
        assert_eq!(
            notices_addressed_to(&broker, "w1:p6", pending_id),
            1,
            "suspect の送信元宛でも通知を捨てない"
        );
        assert_eq!(
            broker.state.queue_len("w1:p6"),
            1,
            "通知は復帰待ちの queue に入る"
        );

        // 復帰すれば隔離が解けて drain 対象に戻る。通知は queue に残ったまま。
        set_herdr_panes(&broker, vec![herdr_agent_pane("w1:p6", Some("grok"))]);
        broker.sync_herdr_registry().await;
        assert!(!broker.state.agents["w1:p6"].suspect);
        assert_eq!(broker.state.queue_len("w1:p6"), 1);
    }

    #[tokio::test]
    async fn a_takeover_that_cannot_be_journaled_leaves_the_old_identity_suspect() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            Some(Herdr::scripted(vec![herdr_agent_pane(
                "w1:p7",
                Some("grok"),
            )])),
        );
        broker.sync_herdr_registry().await;
        let sent = json(&broker.handle(send_request("%1", "w1:p7", "for grok")).await);
        let pending_id = sent["id"].as_u64().unwrap();

        // 旧 pending の回収を永続化できない間は、交代を一切進めない。
        broker.journal.fail_next_appends(10);
        set_herdr_panes(&broker, vec![herdr_agent_pane("w1:p7", Some("gemini"))]);
        broker.sync_herdr_registry().await;
        assert_eq!(
            broker.state.agents["w1:p7"].name, "grok",
            "回収が durable になるまで旧登録が残る"
        );
        assert!(
            broker.state.agents["w1:p7"].suspect,
            "残った旧登録は隔離される"
        );

        broker.journal.clear_failpoints();
        broker.sync_herdr_registry().await;
        assert_eq!(broker.state.agents["w1:p7"].name, "gemini");
        assert!(!broker.state.agents["w1:p7"].suspect);
        assert_eq!(notices_for(&broker, pending_id), 1);
    }

    #[tokio::test]
    async fn a_takeover_register_failure_defers_the_new_identity_to_the_next_sync() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            Some(Herdr::scripted(vec![herdr_agent_pane(
                "w1:p7",
                Some("grok"),
            )])),
        );
        broker.sync_herdr_registry().await;

        // Remove (1回目の append) は通り、直後の Register だけが落ちる。
        broker.journal.fail_appends_after(1);
        set_herdr_panes(&broker, vec![herdr_agent_pane("w1:p7", Some("gemini"))]);
        broker.sync_herdr_registry().await;
        assert!(
            !broker.state.agents.contains_key("w1:p7"),
            "durable でない登録を memory に置かない"
        );

        broker.journal.clear_failpoints();
        broker.sync_herdr_registry().await;
        assert_eq!(
            broker.state.agents["w1:p7"].name, "gemini",
            "次の sync が durable に登録し直す"
        );
    }

    #[tokio::test]
    async fn an_unaddressable_native_name_is_never_registered() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(&dir, vec![]);
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![])),
            Some(Herdr::scripted(vec![herdr_agent_pane(
                "w1:p7",
                Some("bad/name"),
            )])),
        );
        broker.sync_herdr_registry().await;
        assert!(
            !broker.state.agents.contains_key("w1:p7"),
            "宛先文法に載らない native 名は登録しない (CLI の register と同じ検証)"
        );
    }

    #[tokio::test]
    async fn herdr_registrations_refuse_manual_unregister() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(&dir, vec![]);
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![])),
            Some(Herdr::scripted(vec![herdr_agent_pane(
                "w1:p7",
                Some("grok"),
            )])),
        );
        broker.sync_herdr_registry().await;
        let refused = broker
            .handle(request("unregister", Some("w1:p7"), &[]))
            .await;
        assert_eq!(refused.code, 1);
        assert!(refused.stderr.contains("herdr"), "{}", refused.stderr);
        assert!(broker.state.agents.contains_key("w1:p7"));
    }

    #[tokio::test]
    async fn a_new_send_never_overtakes_the_queue_and_the_tick_drains_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        // send1 で %2 は Busy になり、send2 は queue に入る。
        let first = json(&broker.handle(send_request("%1", "claude", "first")).await);
        assert_eq!(first["path"], "sent");
        let second = json(&broker.handle(send_request("%1", "claude", "second")).await);
        assert_eq!(second["path"], "queued");
        let second_id = second["id"].as_u64().unwrap();

        // 配達失敗後に Idle へ戻った状態 (queue が残ったまま idle) を作る。
        broker.state.ack(first["id"].as_u64().unwrap());
        broker.state.set_state("%2", AgentState::Idle);

        // Idle でも queue が残っている間、新規 send は直接配達で追い越さない。
        let third = json(&broker.handle(send_request("%1", "claude", "third")).await);
        assert_eq!(
            third["path"], "queued",
            "queue が残る pane への新規 send が FIFO を追い越してはならない"
        );
        let third_id = third["id"].as_u64().unwrap();

        // health tick の drain が queue 先頭 (send2) だけを配達する。
        let before = bells(&broker).len();
        broker.drain_queued().await;
        let after: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(after.len(), 1, "1 tick で流すのは先頭の1件だけ");
        assert!(
            after[0].1.contains(&format!("read_message {second_id}")),
            "先頭 (send2) が先に配達される: {:?}",
            after[0]
        );
        assert!(broker.state.is_busy("%2"), "配達後は Busy に戻る");
        assert!(broker.state.is_queued("%2", third_id));

        // 受信側の turn-end で send3 が続き、順序が送信順と一致する。
        broker.turn_end(Some("%2".into())).await.unwrap();
        let drained: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(drained.len(), 2);
        assert!(
            drained[1].1.contains(&format!("read_message {third_id}")),
            "{drained:?}"
        );
        assert_eq!(broker.state.queue_len("%2"), 0, "queue は空になる");
    }

    #[tokio::test]
    async fn a_live_pane_survives_mirror_drift_and_gets_its_mirror_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        // %2 は生存しているが `@agent` mirror が消えている (drift)。
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![
                pane_info("%1", Some("codex")),
                pane_info("%2", None),
            ])),
            None,
        );
        broker.reconcile(false).await;
        assert!(
            broker.state.agents.contains_key("%2"),
            "mirror 欠落だけで生存 pane を evict してはならない"
        );
        let repairs: Vec<_> = broker
            .mux
            .tmux()
            .unwrap()
            .options
            .lock()
            .unwrap()
            .iter()
            .filter(|(pane, key, _)| pane == "%2" && key == "@agent")
            .map(|(_, _, value)| value.clone())
            .collect();
        assert_eq!(
            repairs,
            [Some("claude".to_owned())],
            "memory を正として mirror を修復する"
        );

        // mirror が別名へ drift しても、registry (唯一の真実) が勝つ。
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![
                pane_info("%1", Some("codex")),
                pane_info("%2", Some("cursor")),
            ])),
            None,
        );
        broker.reconcile(false).await;
        assert!(broker.state.agents.contains_key("%2"));
        assert_eq!(broker.state.agents["%2"].name, "claude");

        // pane そのものが消えたときは従来どおり evict する。
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            None,
        );
        broker.reconcile(false).await;
        assert!(!broker.state.agents.contains_key("%2"));
    }

    fn herdr_pane_info(pane_id: &str, agent: Option<&str>) -> PaneInfo {
        PaneInfo {
            backend: crate::backend::BackendKind::Herdr,
            ..pane_info(pane_id, agent)
        }
    }

    #[tokio::test]
    async fn reconcile_owns_tmux_lifecycle_and_leaves_herdr_to_the_pull_sync() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(
            &dir,
            vec![
                pane_info("%1", Some("codex")),
                herdr_pane_info("w2:p1", Some("claude")),
            ],
        );
        for (pane, name) in [("%1", "codex"), ("w2:p1", "claude")] {
            let response = broker
                .handle(request("register", Some(pane), &[name]))
                .await;
            assert_eq!(response.code, 0, "{}", response.stderr);
        }
        // tmux: mirror drift では消えない (従来どおり)。pane 消滅で evict。
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![
                pane_info("%1", None),
                herdr_pane_info("w2:p1", None),
            ])),
            None,
        );
        broker.reconcile(false).await;
        assert!(broker.state.agents.contains_key("%1"));
        // herdr: reconcile は識別不一致でも触らない (sync の debounce が所有)。
        assert!(
            broker.state.agents.contains_key("w2:p1"),
            "reconcile が herdr を即 evict すると debounce を迂回する"
        );
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![herdr_pane_info("w2:p1", None)])),
            None,
        );
        broker.reconcile(false).await;
        assert!(
            !broker.state.agents.contains_key("%1"),
            "tmux は pane 消滅で evict"
        );
        assert!(broker.state.agents.contains_key("w2:p1"));
    }

    #[tokio::test]
    async fn failure_notices_survive_sender_mirror_drift() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let original = json(&broker.handle(send_request("%1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();
        // 送信者 %1 は生存しているが mirror が欠落。宛先 %2 は消滅。
        broker.mux = Multiplexer::new(Some(Tmux::scripted(vec![pane_info("%1", None)])), None);
        assert!(
            broker
                .remove_agent("%2", "宛先エージェントが退出した")
                .await
        );
        assert_eq!(
            notices_for(&broker, original),
            1,
            "mirror drift した送信者にも失敗通知が届く (silent 退役の禁止)"
        );
        assert!(broker.state.message(original).unwrap().acked);
    }

    #[tokio::test]
    async fn reconcile_defers_eviction_while_a_backend_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        // herdr が応答しない間は、tmux 一覧に居ない %2 も evict しない
        // (不完全な一覧を「不在」と読まない)。
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            Some(Herdr::new("/nonexistent/herdr.sock".into())),
        );
        broker.reconcile(false).await;
        assert!(
            broker.state.agents.contains_key("%2"),
            "backend 部分失敗中の evict は見送る"
        );
        // 全 backend が答えたら通常どおり evict する。
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            None,
        );
        broker.reconcile(false).await;
        assert!(!broker.state.agents.contains_key("%2"));
    }

    #[tokio::test]
    async fn legacy_wire_names_still_reach_the_same_operations() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        // 旧 adapter が送る *-v1 名は canonical と同じ operation へ正規化される。
        let mut legacy_send = request("send-message-v1", Some("%1"), &["claude"]);
        legacy_send.stdin = "legacy body".into();
        legacy_send.send_options = Some(SendOptions::default());
        let sent = json(&broker.handle(legacy_send).await);
        assert_eq!(sent["path"], "sent");
        let id = sent["id"].as_u64().unwrap();

        let read = json(
            &broker
                .handle(request("read-v1", Some("%2"), &[&id.to_string()]))
                .await,
        );
        assert!(read["body"].as_str().unwrap().contains("legacy body"));

        let peers = json(&broker.handle(request("peers-v1", Some("%2"), &[])).await);
        assert_eq!(peers["pending_to_me"][0], id);

        let acked = json(
            &broker
                .handle(request("ack-v1", Some("%2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(acked["outcome"], "acked");

        // alias 表の5件目。外部 caller (pane なし) 専用 RPC も旧名で同じ operation に届く。
        broker.config.allowed_sources.insert("mobile".into());
        let mailbox = json(
            &broker
                .handle(request("mailbox-list-v1", None, &["mobile"]))
                .await,
        );
        assert_eq!(mailbox["version"], 1);
        assert_eq!(mailbox["mailbox"], "mobile");
    }

    #[tokio::test]
    async fn an_oversized_batch_notice_stays_within_the_body_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let big = "x".repeat(700_000);
        let mut ids = Vec::new();
        for _ in 0..2 {
            ids.push(
                json(&broker.handle(send_request("%1", "claude", &big)).await)["id"]
                    .as_u64()
                    .unwrap(),
            );
        }
        broker.mux = Multiplexer::new(
            Some(Tmux::scripted(vec![pane_info("%1", Some("codex"))])),
            None,
        );
        assert!(
            broker
                .remove_agent("%2", "宛先エージェントが退出した")
                .await
        );

        let notice = broker
            .state
            .messages
            .values()
            .find(|stored| stored.target_pane == "%1" && !stored.acked)
            .expect("collapsed notice");
        let brief = &notice.message.brief;
        assert!(
            brief.len() <= MAX_BODY_BYTES + 4096,
            "集約通知は単発肥大しない: {} bytes",
            brief.len()
        );
        // 予算内の1通目は全文、超過する2通目は ID を残して本文を省略する。
        assert!(brief.contains(&format!("## 元の依頼 #{}", ids[0])));
        assert!(brief.contains(&big));
        assert!(brief.contains(&format!("## 元の依頼 #{}", ids[1])));
        assert!(
            brief.contains("集約通知の上限を超えるため省略"),
            "超過は明示的に省略と書く"
        );
        // 省略された分も含め、originals は全件退役している。
        for id in ids {
            assert!(broker.state.message(id).unwrap().acked);
        }
    }

    #[test]
    fn http_routes_are_read_only_and_api_misses_do_not_fall_back() {
        assert_eq!(classify_http(&Method::GET, "/api/hello"), HttpRoute::Hello);
        assert_eq!(classify_http(&Method::GET, "/api/who"), HttpRoute::Who);
        assert_eq!(
            classify_http(&Method::GET, "/api/agents/%251/screen"),
            HttpRoute::Screen("%1".into())
        );
        // herdr の pane id も screen route に乗る (実管理画面の herdr 表示)。
        assert_eq!(
            classify_http(&Method::GET, "/api/agents/w2%3Ap4/screen"),
            HttpRoute::Screen("w2:p4".into())
        );
        assert_eq!(
            classify_http(&Method::GET, "/api/mailboxes"),
            HttpRoute::Mailboxes
        );
        assert_eq!(
            classify_http(&Method::GET, "/api/mailbox/review%3Asecurity"),
            HttpRoute::Mailbox("review:security".into())
        );
        assert_eq!(
            classify_http(&Method::GET, "/api/missing"),
            HttpRoute::ApiNotFound
        );
        assert_eq!(
            classify_http(&Method::GET, "/agents/one"),
            HttpRoute::Static
        );
        // 撤去済みの旧 /v1/* は SPA へ fallthrough させず、明示的に拒否する
        // (旧 updater の health probe に 200 を返す fail-open の防止)。
        assert_eq!(
            classify_http(&Method::GET, "/v1/hello"),
            HttpRoute::ApiNotFound
        );
        assert_eq!(
            classify_http(&Method::POST, "/api/who"),
            HttpRoute::MethodNotAllowed("GET")
        );
        // 手紙の投函だけが唯一の書き込み route。
        assert_eq!(
            classify_http(&Method::POST, "/api/letters"),
            HttpRoute::Letters
        );
        assert_eq!(
            classify_http(&Method::GET, "/api/letters"),
            HttpRoute::MethodNotAllowed("POST")
        );
        for path in [
            "/api/agents/screen",
            "/api/agents/%1/screen",
            "/api/agents//screen",
            "/api/mailbox/bad%2Fname",
        ] {
            assert_eq!(classify_http(&Method::GET, path), HttpRoute::BadRequest);
        }
        for path in [
            "/api/agents/%252F/screen",
            "/api/agents/%250x/screen",
            "/api/mailbox/Bad",
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
            backend: "tmux",
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
