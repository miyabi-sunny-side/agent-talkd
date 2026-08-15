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

use anyhow::{Context, Result};
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
    backend::{Backend, PaneInfo, usable_agent_name},
    config::{Config, is_safe_token},
    help,
    herdr::{AgentStatus, Herdr},
    journal::{Journal, Record},
    protocol::{Request, Response, SendOptions},
    state::{
        Agent, AgentState, BrokerState, Dispatch, ExternalMailboxEvent, Identity, MailboxDirection,
        Message, NON_PANE_SENDERS, Origin, SenderKind,
    },
};

const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 配達完了からこの時間未読なら受領催促を送る
/// (docs/design.md「受領報告と保持」の催促契約)。
const NAG_AFTER: std::time::Duration = std::time::Duration::from_mins(1);

/// 受領催促の再送間隔。連打で pane を荒らさないための下限。
const NAG_COOLDOWN: std::time::Duration = std::time::Duration::from_mins(5);

/// MCP adapter 経由の操作は、呼び出し元 pane が登録済み agent であることを要求する。
/// 呼び出し元 pane id は routing metadata でしかないが、**未登録 pane を拒否する
/// 既存境界は変更しない** (docs/decisions/0001-conversation-broker-scope.md 起動時 contract 5)。
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
                    Response::ok(format!("queued (waiting) -> {pane} ({addr}): #{id}\n"))
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
enum MessageAccess {
    /// 呼び出し元宛・未受領。配達済みでも queue 中でもよい（所有者 pull 可）。
    Pending,
    /// 存在しないか、既に受領報告済み。
    NotFound,
    NotMine,
}

impl MessageAccess {
    fn reject_reason(&self, id: u64) -> String {
        match self {
            // 呼び出し側が Pending を分岐で処理済みのため到達しない。daemon を落とさない。
            Self::Pending | Self::NotFound => {
                format!("message #{id} は見つかりません (受領報告済みの可能性があります)")
            }
            Self::NotMine => format!("message #{id} はこのpane宛ではありません"),
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
        skill: Option<String>,
        reply: oneshot::Sender<std::result::Result<serde_json::Value, WebError>>,
    },
    Screen {
        pane: String,
        reply: oneshot::Sender<std::result::Result<WebScreen, WebError>>,
    },
    Skills {
        pane: String,
        reply: oneshot::Sender<std::result::Result<Vec<String>, WebError>>,
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
    if let Some(parent) = config.rpc_socket.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let Some(listener) = bind_rpc_socket(&config.rpc_socket).await? else {
        // 既に生きた daemon が居る。二重起動しない。
        return Ok(());
    };
    remove_stale_socket(&config.http_socket)?;
    let http_listener = UnixListener::bind(&config.http_socket)
        .with_context(|| format!("cannot bind {}", config.http_socket.display()))?;

    let backend = Backend::new(Herdr::new(config.herdr_socket.clone()));
    backend.probe().await?;
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    spawn_accept_loop(listener, tx.clone());
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

    adopt_legacy_journal(&config.journal)?;
    let (journal, state) = Journal::open(config.journal.clone())?;
    let mut broker = Broker {
        state,
        journal,
        backend: backend.clone(),
        config,
        pane_resolver: crate::procid::resolve_from_peer,
    };
    broker.startup().await?;
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
            // herdr が応答しなくなったときだけ停止する。
            Event::ServerCheck => match backend.still_serving().await {
                Ok(()) => {
                    failed_health_checks = 0;
                    broker.sync_herdr_registry().await;
                    broker.drain_queued().await;
                    broker.nag_unacked().await;
                }
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
        info!(source = "health", "stopping after herdr went away");
    }
    let _ = fs::remove_file(&broker.config.rpc_socket);
    let _ = fs::remove_file(&broker.config.http_socket);
    Ok(())
}

/// 旧 (tmux 命名) journal の一回きりの引き継ぎ。
///
/// tmux 併存期は journal 名を tmux socket 名から導出していたため、herdr 単独に
/// なった daemon が別名の空 journal を開くと、未受領 message と採番済み ID を
/// 見失い **ID を再利用してしまう**。herdr 名の journal がまだ無く、旧 journal が
/// ちょうど 1 つ残っている場合に限り、rename で引き継ぐ。
///
/// 候補が複数・列挙失敗・rename 失敗では **daemon の起動を失敗させる** —
/// 推測して間違った journal を掴むのも、新しい空 journal で ID を再利用するのも、
/// どちらも durability 契約の破れであり、黙って続行してよい状態ではない。
fn adopt_legacy_journal(journal: &Path) -> Result<()> {
    if journal.exists() {
        return Ok(());
    }
    let Some(dir) = journal.parent() else {
        return Ok(());
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // directory 自体が無い = 初回起動。引き継ぐものが無い。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("state directory を列挙できません: {}", dir.display()));
        }
    };
    let legacy: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension() == Some(OsStr::new("journal")) && path != journal)
        .collect();
    match legacy.as_slice() {
        [] => Ok(()),
        [single] => {
            fs::rename(single, journal).with_context(|| {
                format!(
                    "旧 journal を引き継げません: {} -> {}",
                    single.display(),
                    journal.display()
                )
            })?;
            info!(from = %single.display(), to = %journal.display(),
                source = "journal", "adopted legacy journal");
            Ok(())
        }
        _ => {
            anyhow::bail!(
                "旧 journal の候補が複数あります ({})。引き継ぐものを {} へ手動で rename してください",
                legacy
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                journal.display()
            )
        }
    }
}

/// RPC socket を開く。既に生きた daemon が居るときは `None` を返す (二重起動しない)。
async fn bind_rpc_socket(path: &Path) -> Result<Option<UnixListener>> {
    if path.exists() && UnixStream::connect(path).await.is_ok() {
        return Ok(None);
    }
    remove_stale_socket(path)?;
    Ok(Some(UnixListener::bind(path).with_context(|| {
        format!("cannot bind {}", path.display())
    })?))
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
        HttpRoute::Skills(pane) => {
            if request.uri().query().is_some() {
                json_error(StatusCode::BAD_REQUEST, "invalid_query")
            } else {
                request_web_skills(&tx, pane).await
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
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "broker_unavailable")
            .with_header(CACHE_CONTROL, "no-store");
    }
    match receive.await {
        Ok(Ok(agents)) => json_response(StatusCode::OK, &serde_json::json!({ "agents": agents }))
            .with_header(CACHE_CONTROL, "no-store"),
        _ => json_error(StatusCode::SERVICE_UNAVAILABLE, "registry_unavailable")
            .with_header(CACHE_CONTROL, "no-store"),
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
/// source の許可は daemon 側の allowlist (`AGENT_TALK_ALLOWED_SOURCES`、既定 deny)
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
        #[serde(default)]
        skill: Option<String>,
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
            // 空文字・不正 token は None に正規化せず、既存 SendOptions 検証へ渡す。
            skill: letter.skill,
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

async fn request_web_skills(tx: &mpsc::Sender<Event>, pane: String) -> HttpResponse<Full<Bytes>> {
    let (reply, receive) = oneshot::channel();
    if tx
        .send(Event::Http(HttpEvent::Skills { pane, reply }))
        .await
        .is_err()
    {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "broker_unavailable");
    }
    match receive.await {
        Ok(Ok(skills)) => json_response(StatusCode::OK, &serde_json::json!({ "skills": skills })),
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
    Skills(String),
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
        path if path.starts_with("/api/agents/") && path.ends_with("/skills") => {
            let encoded = path
                .strip_prefix("/api/agents/")
                .and_then(|rest| rest.strip_suffix("/skills"))
                .unwrap_or_default();
            match decode_path_segment(encoded) {
                Some(pane) => HttpRoute::Skills(pane),
                None => HttpRoute::BadRequest,
            }
        }
        path if path.starts_with("/api/agents/") && path.ends_with("/screen") => {
            let encoded = path
                .strip_prefix("/api/agents/")
                .and_then(|rest| rest.strip_suffix("/screen"))
                .unwrap_or_default();
            // pane id は opaque — 文法検査せず、登録の有無は Screen handler が
            // registry で判定する (未登録は 404)。
            match decode_path_segment(encoded) {
                Some(pane) => HttpRoute::Screen(pane),
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
    // decode 後の `/` は許す — pane id は opaque で `/` を含みうる。route の
    // 構造は decode 前の raw path で既に確定しており、%2F は data にすぎない。
    // NUL だけは常に拒否する。
    if decoded.contains(&0) {
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
    // 呼び出し元 process の PID を接続から採取する。pane 申告の無い MCP RPC は
    // この PID の祖先から identity を解決する (env forward 不要化)。
    let peer_pid = stream
        .peer_cred()
        .ok()
        .and_then(|credentials| credentials.pid());
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).await?;
    let mut request: Request = serde_json::from_str(&line)?;
    request.peer_pid = peer_pid;
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
    backend: Backend,
    config: Config,
    /// peer PID → pane identity の解決関数。test では表引きに差し替える。
    pane_resolver: fn(i32, &Path) -> std::result::Result<String, String>,
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
                skill,
                reply,
            } => {
                let _ = reply.send(self.web_letter(source, target, body, skill).await);
            }
            HttpEvent::Screen { pane, reply } => {
                let _ = reply.send(self.web_screen(&pane).await);
            }
            HttpEvent::Skills { pane, reply } => {
                let _ = reply.send(self.web_skills(&pane));
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
        let panes = self.backend.panes().await?;
        let mut agents = Vec::new();
        for pane in panes {
            if let Some(agent) = self.state.agents.get(&pane.pane_id) {
                agents.push(WebAgent {
                    name: agent.name.clone(),
                    state: display_state(pane.status),
                    pane_id: pane.pane_id,
                    session: pane.session.clone(),
                    location: format!("{}:{}.{}", pane.session, pane.window_index, pane.pane_index),
                    cwd: pane.cwd,
                    backend: "herdr",
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
        skill: Option<String>,
    ) -> std::result::Result<serde_json::Value, WebError> {
        let request = Request {
            command: "send-v2".into(),
            args: vec![target],
            stdin: body,
            pane: None,
            send_options: Some(SendOptions {
                from: Some(source),
                skill,
                no_reply: false,
            }),
            peer_pid: None,
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
        if response.stderr.contains("AGENT_TALK_ALLOWED_SKILLS")
            || response.stderr.contains("skill名は")
            || response.stderr.contains("skill記法")
        {
            Err(WebError::new(StatusCode::BAD_REQUEST, "skill_rejected"))
        } else if response.stderr.contains("許可されていません") {
            Err(WebError::new(StatusCode::FORBIDDEN, "source_not_allowed"))
        } else if response.stderr.contains("見つかりません") || response.stderr.contains("退出済み")
        {
            Err(WebError::new(StatusCode::NOT_FOUND, "target_not_found"))
        } else {
            Err(WebError::new(StatusCode::BAD_REQUEST, "letter_rejected"))
        }
    }

    /// 対象 pane の agent に付けられる skill 候補。
    /// `skill_syntax` が無い runtime は空。一覧は home 上の installed ∩
    /// `AGENT_TALK_ALLOWED_SKILLS` (設定時) の積。
    fn web_skills(&self, pane: &str) -> std::result::Result<Vec<String>, WebError> {
        let Some(agent) = self.state.agents.get(pane) else {
            return Err(WebError::new(StatusCode::NOT_FOUND, "agent_not_found"));
        };
        // skill 記法・installed の走査はどちらも runtime 検出名で引く
        // (tab 名は宛先であって runtime ではない)。
        if !self.config.skill_syntax.contains_key(&agent.runtime) {
            return Ok(Vec::new());
        }
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        let mut skills = installed_skills_for_runtime(&home, &agent.runtime);
        if let Some(allowed) = &self.config.allowed_skills {
            skills.retain(|skill| allowed.contains(skill));
        }
        Ok(skills)
    }

    async fn web_screen(&self, pane: &str) -> std::result::Result<WebScreen, WebError> {
        if !self.state.agents.contains_key(pane) {
            return Err(WebError::new(StatusCode::NOT_FOUND, "agent_not_found"));
        }
        let panes =
            self.backend.panes().await.map_err(|_| {
                WebError::new(StatusCode::SERVICE_UNAVAILABLE, "backend_unavailable")
            })?;
        if !panes.iter().any(|candidate| candidate.pane_id == pane) {
            return Err(WebError::new(StatusCode::GONE, "pane_unavailable"));
        }
        if let Ok(screen) = self.backend.capture_pane(pane).await {
            Ok(WebScreen {
                pane_id: pane.to_owned(),
                screen,
            })
        } else {
            let pane_still_exists = self
                .backend
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
        let command = canonical_command(&request.command).to_owned();
        if matches!(
            command.as_str(),
            "send" | "send-v2" | "send-message" | "read-message" | "ack-message" | "list-peers"
        ) && let Err(error) = self.refresh_herdr_registry().await
        {
            return Response::error(format!("herdr snapshot を取得できません: {error}"));
        }
        let request = match self.attach_caller_pane(request) {
            Ok(request) => request,
            Err(response) => return response,
        };
        let result = match command.as_str() {
            "register" => self.register(request).await,
            "unregister" => Ok(Self::unregister(&request)),
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
            "ack-message" => Ok(self.ack(&request)),
            "list-peers" => self.peers_json(&request).await,
            "reply" => self.reply(request),
            "mailbox-list" => self.mailbox_list(&request),
            "internal-daemon-status" => Ok(Response::ok(format!(
                "{}\n",
                serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "pid": std::process::id(),
                    "ready": true,
                    // tmux 併存期の status 互換のため list のまま返す。
                    "backends": ["herdr"],
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
                self.sync_herdr_registry().await;
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

    /// pane 申告の無い MCP RPC に、接続の peer PID から解決した呼び出し元 pane を
    /// 付与する (env forward 不要化)。対象は agent 経路の 4 RPC だけ — 外部 caller
    /// 用の command (`mailbox-list` 等) は pane 無しのまま扱う。
    /// 解決の失敗は fail closed で、明示 forward という逃げ道を案内する。
    fn attach_caller_pane(&self, mut request: Request) -> std::result::Result<Request, Response> {
        let is_agent_rpc = matches!(
            canonical_command(&request.command),
            "send-message" | "read-message" | "ack-message" | "list-peers"
        );
        if !is_agent_rpc || request.pane.is_some() {
            return Ok(request);
        }
        let Some(pid) = request.peer_pid else {
            // peer PID が取れない接続は従来どおり未登録扱いへ落ちる。
            return Ok(request);
        };
        match (self.pane_resolver)(pid, &self.config.herdr_socket) {
            Ok(pane) => {
                request.pane = Some(pane);
                Ok(request)
            }
            Err(reason) => Err(Response::error(format!(
                "呼び出し元の pane を特定できません: {reason} (HERDR_SOCKET_PATH と HERDR_PANE_ID を forward すれば明示できます)"
            ))),
        }
    }

    /// 登録の正は herdr の native identity (pull 同期)。`register` は互換 command
    /// として残すが、**pull 同期と同じ規則で導出した名前 (tab label 由来、無ければ
    /// runtime 検出名) しか受理しない** — 一致しない登録を許すと、次の tick で
    /// pull が是正するまでの間、実際の占有者と違う名前が宛先として解決・
    /// 配達されてしまう。
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
        // snapshot が取れない間は受理も拒否もできない (fail closed)。
        let panes = self.backend.panes().await?;
        let identity = panes
            .iter()
            .find(|candidate| candidate.pane_id == pane)
            .and_then(|candidate| candidate.name.as_deref().zip(candidate.agent.as_deref()));
        let Some((derived, runtime)) = identity.filter(|(derived, _)| derived == name) else {
            return Ok(Response::error(format!(
                "pane {pane} の識別名 ({}) と一致しないため登録できません",
                identity.map_or("agent なし", |(derived, _)| derived)
            )));
        };
        match self.state.agents.get(&pane) {
            Some(agent) if agent.name == derived && agent.runtime == runtime => {}
            Some(_) => {
                // 交代: pull 同期の takeover と同じ経路 (回収が durable になってから登録)。
                if !self
                    .remove_agent(&pane, "宛先エージェントが入れ替わった")
                    .await
                    || !self.register_observed(&pane, derived, runtime, "register")
                {
                    return Ok(Response::error("登録を永続化できないため中止しました"));
                }
            }
            None => {
                if !self.register_observed(&pane, derived, runtime, "register") {
                    return Ok(Response::error("登録を永続化できないため中止しました"));
                }
            }
        }
        Ok(Response::ok(""))
    }

    fn unregister(request: &Request) -> Response {
        if request.pane.is_none() {
            return Response::ok("");
        }
        // herdr の登録は native 検出の pull 同期が管理する。手動 unregister を
        // 受けても次の tick で再登録されて混乱するだけなので、明示的に断る。
        Response::error(
            "herdr pane の登録は herdr の検出が管理します (外すには agent を停止してください)",
        )
    }

    /// queue 先頭を1件だけ配達する。send と health tick の再配達が共有する
    /// 唯一の queue 配送状態遷移 (成功: Complete / 失敗: 同 ID requeue)。
    /// 返り値は「配達対象があったか」。journal へ書けない場合だけ Err。
    async fn deliver_queued_head(
        &mut self,
        pane: &str,
        source: &'static str,
    ) -> std::result::Result<bool, String> {
        let message_id = loop {
            let Some(id) = self.state.take_queued_head(pane) else {
                break None;
            };
            match self.state.message(id) {
                // 所有者 pull 済み・既配達が queue に残っても呼び鈴を二重に鳴らさない。
                Some(stored) if stored.delivered => {}
                Some(stored) if stored.acked => {
                    if let Err(error) = self.journal.append(&Record::Complete {
                        pane: pane.to_owned(),
                        id,
                    }) {
                        return Err(format!(
                            "acked 残骸のskipをjournalへ書き込めません: {error}"
                        ));
                    }
                    self.state.complete_delivery(pane, id);
                }
                Some(_) => break Some(id),
                None => {
                    error!(%pane, id, "queued message body missing; skipping");
                    if let Err(error) = self.journal.append(&Record::Complete {
                        pane: pane.to_owned(),
                        id,
                    }) {
                        return Err(format!(
                            "欠損メッセージのskipをjournalへ書き込めません: {error}"
                        ));
                    }
                }
            }
        };
        let Some(id) = message_id else {
            return Ok(false);
        };
        let Some(stored) = self.state.message(id) else {
            error!(%pane, id, "message body disappeared before delivery");
            return Err(format!(
                "message #{id} の本文が見つからないため配達を中止しました"
            ));
        };
        let bell = stored.message.bell.clone();
        if self.backend.deliver(pane, &bell).await.is_ok() {
            if let Err(error) = self.journal.append(&Record::Complete {
                pane: pane.to_owned(),
                id,
            }) {
                self.state.requeue_after_delivery_failure(pane, id);
                return Err(error.to_string());
            }
            self.state.complete_delivery(pane, id);
            info!(%pane, id, source, "delivered");
        } else {
            self.state.requeue_after_delivery_failure(pane, id);
            // 配達失敗は正常系 (相手が blocked / unknown 判定など)。次の契機で再試行される。
            // silent にすると診断できないので debug には残す。
            debug!(%pane, id, source, "queued delivery failed; will retry");
        }
        Ok(true)
    }

    /// 観測に基づく登録の共通経路 (herdr snapshot / 起動時 mirror)。
    ///
    /// journal が durable になった時だけ memory を更新して true を返す。
    fn register_observed(
        &mut self,
        pane: &str,
        name: &str,
        runtime: &str,
        source: &'static str,
    ) -> bool {
        if !usable_agent_name(name) {
            warn!(%pane, %name, source, "observed agent name is not addressable; skipped");
            return false;
        }
        if self
            .journal
            .append(&Record::Register {
                pane: pane.to_owned(),
                name: name.to_owned(),
                runtime: Some(runtime.to_owned()),
                state: AgentState::Idle,
            })
            .is_err()
        {
            return false;
        }
        self.state.restore_agent(
            pane.to_owned(),
            name.to_owned(),
            runtime.to_owned(),
            AgentState::Idle,
        );
        info!(%pane, %name, %runtime, source, "registered");
        true
    }

    /// 登録を herdr の native identity から pull 同期する (2秒 tick)。
    ///
    /// herdr 自身が agent を検出する側なので、hook を持たない agent (grok 等) も
    /// 現れた数秒後には peer になる。lifecycle は次の3規則:
    /// - **出現** (未登録 pane に identity): 即 pull 登録。idempotent。
    /// - **交代** (登録名と別の identity): 強い証拠なので debounce せず即 takeover —
    ///   旧登録を回収してから新 identity を登録する。
    /// - **欠落** (pane 消滅 or identity 無し): 成功 snapshot を正として即 evict。
    ///   snapshot の RPC 失敗は判定を一切進めない (不完全な証拠で消さない)。
    async fn sync_herdr_registry(&mut self) {
        // 定期 tick は取得失敗で判定を進めない (不完全な証拠で消さない)。
        let _ = self.refresh_herdr_registry().await;
    }

    async fn refresh_herdr_registry(&mut self) -> Result<()> {
        let panes = self.backend.panes().await?;
        self.apply_herdr_snapshot(panes).await;
        Ok(())
    }

    async fn apply_herdr_snapshot(&mut self, panes: Vec<PaneInfo>) {
        // identity は (name, runtime)。name は tab label 由来 (無ければ runtime
        // 検出名) で、宛先文法に載らない名前は identity 無しとして扱う。
        let detected: std::collections::HashMap<String, Option<(String, String)>> = panes
            .into_iter()
            .map(|pane| {
                let identity = pane
                    .name
                    .filter(|name| usable_agent_name(name))
                    .zip(pane.agent);
                (pane.pane_id, identity)
            })
            .collect();
        // 出現・確認・交代。
        for (pane_id, identity) in &detected {
            let Some((name, runtime)) = identity else {
                continue;
            };
            match self.state.agents.get(pane_id) {
                None => {
                    self.register_observed(pane_id, name, runtime, "herdr-pull");
                }
                // tab rename も runtime 交代も takeover 経路に乗せる。
                Some(agent) if agent.name == *name && agent.runtime == *runtime => {}
                Some(_) => {
                    // 交代: 旧登録の回収が durable になった後にだけ新 identity を登録。
                    // Register の append に失敗しても旧登録は既に消えており、
                    // 次 tick の None 分岐が同じ helper で durable 登録し直す。
                    if self
                        .remove_agent(pane_id, "宛先エージェントが入れ替わった")
                        .await
                    {
                        self.register_observed(pane_id, name, runtime, "herdr-pull");
                    }
                }
            }
        }
        // 欠落 (登録済み pane が identity 付きで見えない)。成功 snapshot は
        // herdr の現在そのものなので debounce せず即 evict する。
        let missing: Vec<String> = self
            .state
            .agents
            .keys()
            .filter(|pane| detected.get(*pane).is_none_or(Option::is_none))
            .cloned()
            .collect();
        for pane in missing {
            self.remove_agent(&pane, "宛先エージェントが退出した").await;
        }
    }

    /// queue が残っている pane の先頭を、health tick ごとに拾い直す。
    ///
    /// 送信時に herdr が working を返した message を保持し、idle の正の証拠が
    /// 次に得られた時点 (最大 tick 間隔 + API 時間) で同じ ID を再配達する。
    async fn drain_queued(&mut self) {
        let candidates: Vec<String> = self
            .state
            .agents
            .iter()
            .filter(|(_, agent)| !agent.queue.is_empty())
            .map(|(pane, _)| pane.clone())
            .collect();
        for pane in candidates {
            if let Err(reason) = self.deliver_queued_head(&pane, "queued-retry").await {
                warn!(%pane, %reason, source = "queued-retry", "queued delivery attempt failed");
            }
        }
    }

    async fn who(&self, request: &Request) -> Result<Response> {
        let panes = self.backend.panes().await?;
        let mut output = String::new();
        // herdr 自身が持つ状態を、表示用の idle/busy と生値で併記する。
        // backend 列は tmux 併存期の名残だが、表を読む既存クライアントを
        // 壊さないため列自体は残す (値は常に herdr)。
        for pane in &panes {
            if let Some(agent) = self.state.agents.get(&pane.pane_id) {
                let _ = writeln!(
                    output,
                    "{:<10} {:<11} {:<5} {}:{}.{} ({})  {}",
                    agent.name,
                    format!(
                        "{}/{}",
                        display_state(pane.status).as_str(),
                        pane.status.as_str()
                    ),
                    "herdr",
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
                .map(|agent| (pane.to_owned(), agent.name.clone(), agent.runtime.clone()))
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
                    "skill '{skill}' は AGENT_TALK_ALLOWED_SKILLS で許可されていません"
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
                    "送信元 '{source}' は AGENT_TALK_ALLOWED_SOURCES で許可されていません"
                )));
            }
            if NON_PANE_SENDERS.contains(&source)
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
            // skill 記法は宛先名 (tab 名) ではなく runtime 検出名で引く。
            // tab 名 fable の claude にも /skill が届くように。
            let runtime = self
                .state
                .agents
                .get(&pane)
                .map_or_else(|| expected.clone(), |agent| agent.runtime.clone());
            let Some(syntax) = self.config.skill_syntax.get(&runtime) else {
                return Ok(Response::error(format!(
                    "agent '{expected}' (runtime '{runtime}') のskill記法が AGENT_TALK_SKILL_SYNTAX にありません"
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
        // dispatch と同じ predicate (`defers_delivery`) で判定する。
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

        let panes = self.backend.panes().await?;
        let from_info = request
            .pane
            .as_deref()
            .and_then(|id| panes.iter().find(|p| p.pane_id == id));
        let from_agent = registered_sender
            .as_ref()
            .map(|(_, name, _)| name.clone())
            .or_else(|| intent.source().map(str::to_owned))
            .unwrap_or_else(|| "human".into());
        let reply_info = registered_sender.as_ref().and(from_info);
        // 送信受理時点の workspace label を捕捉する。**登録内容 (`state.agents`) と
        // pane snapshot の identity が一致した pane からしか採らない** — pane の
        // 占有者が入れ替わっている間に、別人の workspace を差出人へ結合しないため。
        // 一致しなければ拒否する: 捕捉できないまま受理すると、呼び鈴が canonical
        // full label にならず `sender_workspace` も持たない record が恒久的に残る。
        // bare fallback を許すのは、旧 journal record と pane 由来でない送信者
        // (`human` / `system` / `--from` の外部送信元) だけ。
        let sender_workspace = match registered_sender.as_ref() {
            Some((pane_id, name, runtime)) => {
                let backing = panes.iter().find(|pane| {
                    pane.pane_id == *pane_id
                        && pane_backs_registration(pane, Identity { name, runtime })
                });
                let Some(pane) = backing else {
                    // snapshot は取り直さない。同期 tick が短時間で追いつくので、
                    // その場で拒否して同期後の再送を求めるほうが単純で決定的。
                    return Ok(Response::error(format!(
                        "送信元 pane {pane_id} の登録 ({name}) と herdr の現在の状態が食い違うため送信できません。同期後に再送してください"
                    )));
                };
                Some(pane.session.clone())
            }
            None => None,
        };
        // 呼び鈴と brief の文言を分ける軸。`--from` / `--skill` は登録 pane から
        // 指定できない (`SendIntent::classify`) ので、この2つは排他になる。
        let sender_kind = if registered_sender.is_some() {
            SenderKind::Peer
        } else {
            SenderKind::Human
        };
        let brief_mode = if external_source.is_some() {
            BriefMode::External(0)
        } else if intent.no_reply() {
            BriefMode::NoReply
        } else {
            BriefMode::Normal
        };
        let brief = build_brief(
            addr,
            &from_agent,
            from_info,
            reply_info,
            &body,
            brief_mode,
            sender_kind,
        );
        // 呼び鈴の差出人は canonical full label。読み出し側
        // (`Message::sender_full_label`) と同じ helper を通して組み立てる。
        let from_agent_full = crate::state::full_label(sender_workspace.as_deref(), &from_agent);
        // 送信時点の identity を捕捉する。後からレジストリを引き直さない。
        // pane 由来でない送信者 (`human` / 外部 source) は name を runtime として扱う。
        let origin = registered_sender.map_or_else(
            || Origin::new("human", from_agent.clone(), from_agent.clone()),
            |(pane, name, runtime)| {
                Origin::new(pane, name, runtime).with_workspace(sender_workspace)
            },
        );
        let dispatch = self.state.dispatch(
            &pane,
            origin,
            brief,
            &expected,
            |id| {
                if intent.no_reply() {
                    format!(
                        "{skill_prefix}[agent-talk] {from_agent_full} から連絡が届きました。read_message {id} で本文を確認してください。返信は不要です。"
                    )
                } else if sender_kind == SenderKind::Peer {
                    format!(
                        "{skill_prefix}[agent-talk] {from_agent_full} から連絡が届きました。read_message {id} で本文を確認してください。"
                    )
                } else {
                    format!(
                        "{skill_prefix}[agent-talk] {from_agent_full} から依頼が届きました。read_message {id} で本文を確認してから対応してください。"
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
                            sender_kind,
                        ),
                    );
                }
                let Some(stored) = self.state.message(id) else {
                    error!(%pane, id, "new message body missing before persistence");
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
                    self.state.discard_message(id);
                    return Ok(Response::error(format!(
                        "本文を journal に書き込めず配達できません: {error}"
                    )));
                }
                if matches!(dispatch, Dispatch::Queued(_)) {
                    info!(%pane, id, source = "send", "queued");
                    return Ok(report.accepted(SendPath::Queued, id, &pane, addr, &expected));
                }
                let Some(stored) = self.state.message(id) else {
                    error!(%pane, id, "persisted message body missing before delivery");
                    return Ok(Response::error(format!(
                        "message #{id} の本文が見つからないため配達を中止しました"
                    )));
                };
                let bell = stored.message.bell.clone();
                if self.backend.deliver(&pane, &bell).await.is_ok() {
                    if let Err(error) = self.journal.append(&Record::Complete {
                        pane: pane.clone(),
                        id,
                    }) {
                        warn!(%error, %pane, id, "cannot complete delivery; it will be retried");
                        self.state.requeue_after_delivery_failure(&pane, id);
                        return Ok(report.accepted(SendPath::Queued, id, &pane, addr, &expected));
                    }
                    self.state.complete_delivery(&pane, id);
                    info!(%pane, id, source = "send", "delivered");
                    Ok(report.accepted(SendPath::Sent, id, &pane, addr, &expected))
                } else {
                    let target_is_live = self
                        .backend
                        .panes()
                        .await
                        .is_ok_and(|panes| panes.iter().any(|item| item.pane_id == pane));
                    if !target_is_live {
                        // 配達不能な terminal tombstone。受領報告待ちにはしない。
                        self.journal.append(&Record::Consumed { id })?;
                        self.state.ack(id);
                        self.remove_agent(&pane, "宛先が退出した").await;
                        return Ok(Response::error(format!(
                            "宛先 {pane} ({addr}) は退出済みです (message #{id})"
                        )));
                    }
                    self.state.requeue_after_delivery_failure(&pane, id);
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

    /// `read` / `ack` が共有する宛先検査
    /// (docs/decisions/0002-message-retention-ack.md「`ack_message` の契約」)。
    /// 所有者なら queue 中でも Pending。配達状態は `ensure_pull_delivery` / ack 側で扱う。
    fn access(&self, id: u64, pane: &str) -> MessageAccess {
        // 受領報告済み (`Acked`) は存在しないものとして扱う。以後 read は not-found、
        // ack は mutation なしで冪等成功になる。
        let Some(stored) = self.state.message(id).filter(|stored| !stored.acked) else {
            return MessageAccess::NotFound;
        };
        // 生存判定は (name, runtime) の組。タブ名を保ったまま runtime だけ
        // 交代した pane の新しい住人に旧宛の本文を見せない。
        let current = self.state.agents.get(pane).map(Agent::identity);
        if stored.target_pane != pane || current != Some(stored.message.target_identity()) {
            return MessageAccess::NotMine;
        }
        MessageAccess::Pending
    }

    /// 宛先本人の pull 読み: 未配達なら journal `Complete` で durable に配達完了し、
    /// queue から外す。後から同じ ID の呼び鈴を鳴らさない。fsync 前に本文は返さない。
    /// queue 中の pull は **先頭 ID のみ**（push 配達と同じ FIFO。後続を追い越さない）。
    fn ensure_pull_delivery(&mut self, pane: &str, id: u64) -> std::result::Result<(), String> {
        let Some(stored) = self.state.message(id) else {
            return Err(format!(
                "message #{id} は見つかりません (受領報告済みの可能性があります)"
            ));
        };
        if stored.delivered {
            return Ok(());
        }
        if stored.target_pane != pane {
            return Err(format!("message #{id} はこのpane宛ではありません"));
        }
        // queue に載っている間は先頭以外を Complete しない（FIFO 維持）。
        if self.state.is_queued(pane, id) {
            match self.state.queued_head(pane) {
                Some(head) if head == id => {}
                Some(head) => {
                    return Err(format!(
                        "message #{id} は queue 先頭ではありません (先に #{head} を read_message してください)"
                    ));
                }
                None => {}
            }
        }
        self.journal
            .append(&Record::Complete {
                pane: pane.to_owned(),
                id,
            })
            .map_err(|error| error.to_string())?;
        self.state.complete_delivery(pane, id);
        info!(%pane, id, source = "pull", "delivered");
        Ok(())
    }

    /// 本文を返し、読了だけを記録する。受領報告が来るまで何度でも読める。
    /// 未配達の自分宛は pull 配達してから返す（呼び鈴を待たない）。
    fn read(&mut self, request: &Request) -> Response {
        let (id, pane) = match request_target(request, "read") {
            Ok(target) => target,
            Err(response) => return response,
        };
        match self.access(id, &pane) {
            MessageAccess::Pending => {}
            other => return Response::error(other.reject_reason(id)),
        }
        if let Err(reason) = self.ensure_pull_delivery(&pane, id) {
            return Response::error(reason);
        }
        if let Err(reason) = self.persist_seen(id) {
            return Response::error(reason);
        }
        let Some(stored) = self.state.message(id) else {
            return Response::error(format!(
                "message #{id} は見つかりません (受領報告済みの可能性があります)"
            ));
        };
        let brief = stored.message.brief.clone();
        Response::ok(brief)
    }

    /// 構造化 read。MCP adapter の `read_message` が使う。
    /// 未配達の自分宛は pull 配達してから返す（呼び鈴を待たない）。
    fn read_json(&mut self, request: &Request) -> Response {
        let (id, pane) = match request_target(request, "read-message") {
            Ok(target) => target,
            Err(response) => return response,
        };
        match self.access(id, &pane) {
            MessageAccess::Pending => {}
            other => return Response::error(other.reject_reason(id)),
        }
        if let Err(reason) = self.ensure_pull_delivery(&pane, id) {
            return Response::error(reason);
        }
        if let Err(reason) = self.persist_seen(id) {
            return Response::error(reason);
        }
        let Some(stored) = self.state.message(id) else {
            return Response::error(format!(
                "message #{id} は見つかりません (受領報告済みの可能性があります)"
            ));
        };
        // 送信時点で捕捉した名前を返す。現在のレジストリを引き直さない。
        let from = stored.message.sender_label().to_owned();
        // canonical full label (`<workspace>/<name>`)。旧 journal 由来など
        // workspace 未捕捉の message は bare 名へ fallback する。
        let from_full = stored.message.sender_full_label();
        // 返信先は、捕捉時と同じ identity で今も登録中の pane のときだけ。
        let reply_to = self.state.reply_target(&stored.message);
        let body = stored.message.brief.clone();
        Response::ok(format!(
            "{}\n",
            serde_json::json!({
                "version": 1,
                "id": id,
                "from": from,
                "from_full": from_full,
                "reply_to": reply_to,
                "body": body,
            })
        ))
    }

    /// 互換 no-op。受領は `read` が担う。状態・journal は変えない。
    fn ack(&self, request: &Request) -> Response {
        let (id, pane) = match request_target(request, "ack-message") {
            Ok(target) => target,
            Err(response) => return response,
        };
        let outcome = match self.access(id, &pane) {
            MessageAccess::NotFound => "no_pending_message",
            MessageAccess::Pending => "acked",
            other @ MessageAccess::NotMine => {
                return Response::error(other.reject_reason(id));
            }
        };
        Response::ok(format!(
            "{}\n",
            serde_json::json!({ "version": 1, "id": id, "outcome": outcome })
        ))
    }

    /// 未 Seen なら journal に `Seen` を fsync してから印を立てる。失敗時は本文を返さない。
    fn persist_seen(&mut self, id: u64) -> std::result::Result<(), String> {
        let Some(stored) = self.state.message(id) else {
            return Err(format!(
                "message #{id} は見つかりません (受領報告済みの可能性があります)"
            ));
        };
        if stored.seen {
            return Ok(());
        }
        self.journal
            .append(&Record::Seen { id })
            .map_err(|error| error.to_string())?;
        self.state.mark_seen(id);
        Ok(())
    }

    /// 登録 agent 一覧と両方向の未受領 ID。MCP adapter の `list_peers` が使う。
    async fn peers_json(&self, request: &Request) -> Result<Response> {
        let panes = self.backend.panes().await?;
        let caller = request.pane.as_deref();
        let pending_to_me = caller.map(|pane| self.state.pending_to_me(pane));
        let pending_from_me = caller.map(|pane| self.state.pending_from_me(pane));
        let peers: Vec<_> = panes
            .iter()
            .filter_map(|pane| {
                let agent = self.state.agents.get(&pane.pane_id)?;
                Some(serde_json::json!({
                    "name": agent.name,
                    "state": display_state(pane.status),
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
                "reply は登録済みのagent pane内で実行してください",
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
                "mailbox-list は外部caller (pane外) 専用です",
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
        let panes = self.backend.panes().await.map_err(|error| {
            Response::error(format!(
                "herdr に接続できません (sandbox 内なら承認付きで再実行): {error}"
            ))
        })?;
        // pane id の直接指定。id は herdr 発行の opaque 文字列なので文法では
        // 判定せず、**registry に完全一致すれば pane 直指定**として最優先で解決する。
        // 一致しない文字列は名前/scope として解釈へ落ちる。
        if let Some(agent) = self.state.agents.get(addr) {
            return Ok((addr.to_owned(), agent.name.clone()));
        }
        // `herdr/<scope>/<name>` は tmux 併存期の正式名称の互換 alias。
        let rest = match addr.split_once('/') {
            Some((prefix, rest)) if rest.contains('/') => match prefix {
                "herdr" => rest,
                other => {
                    return Err(Response::error(format!(
                        "backend '{other}' は不明です (<scope>/<name> で指定してください)"
                    )));
                }
            },
            _ => addr,
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
        if let Some(scope) = scope {
            // 主名 (workspace label)、workspace_id alias (`w2/codex` の後方互換)、
            // cwd の basename のどれでも引ける。
            candidates.retain(|(pane, _)| {
                pane.session == scope
                    || pane.scope_alias.as_deref() == Some(scope)
                    || Path::new(&pane.cwd).file_name() == Some(OsStr::new(scope))
            });
        } else if let Some(self_pane) = self_pane
            && let Some(origin) = panes.iter().find(|pane| pane.pane_id == self_pane)
        {
            // bare 名は自分の workspace 内 (self 除外後) だけを見る。同名候補が
            // 2件以上残ったら同一タブの近接でも自動選択せず、下の曖昧エラーで
            // 候補の pane id を案内する — 近接選択は、もう一方の同名 pane を
            // 黙って落として誤配する。
            candidates.retain(|(pane, _)| pane.session == origin.session);
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
                    "agent-talk: 宛先 '{addr}' の候補が複数あります。<scope>/<name> か pane id の直指定 (括弧内) で指定してください:\n"
                );
                for (pane, agent) in candidates {
                    stderr.push_str(&pretty(agent.name.as_str(), pane));
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
                output.push_str(&pretty(&agent.name, pane));
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
        let panes = self.backend.panes().await.unwrap_or_default();
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
            // `human` / `system` は registry の key に現れないので、
            // 登録の有無だけで pane 送信者かどうかが決まる。
            let sender_target = self
                .state
                .agents
                .get(&sender)
                .filter(|agent| {
                    // 生存判定は native identity (name, runtime) の一致まで要求する
                    // (`pane_backs_registration`) — 占有者が入れ替わった pane へ
                    // 旧名宛て通知を送らない。
                    Some(sender.as_str()) != excluded_pane
                        && panes.iter().any(|pane| {
                            pane.pane_id == sender
                                && pane_backs_registration(pane, agent.identity())
                        })
                })
                .map(|agent| agent.name.clone());
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
                        "\n## 元のメッセージ #{}\n{}\n",
                        original.id, original.brief
                    );
                } else {
                    let _ = write!(
                        failure_brief,
                        "\n## 元のメッセージ #{}\n(本文 {} bytes は集約通知の上限を超えるため省略)\n",
                        original.id,
                        original.brief.len()
                    );
                }
            }
            let dispatch = self.state.dispatch(
                &sender,
                Origin::new("system", "system", "system"),
                failure_brief,
                &expected,
                |id| {
                    format!(
                        "[agent-talk] 未受領のまま終了: message {listed} は受領報告されないまま{reason}ため回収されました。read_message {id} で元の本文を確認してください。",
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

    /// 配達済みのまま受領報告が無い message について、herdr の観測が
    /// ターンとターンの間 (idle/done) のときだけ受領催促の呼び鈴を送る。
    /// ここでの `accepts_reminder` は候補抽出。最終ゲートは
    /// `backend.deliver_reminder` が送信直前に同じ predicate を再確認する。
    /// 催促は message を新規作成しない
    /// (催促自体が受領報告の対象になる再帰を避ける)。
    /// タイマーは memory のみで、restart 後は配達時刻から数え直す。
    async fn nag_unacked(&mut self) {
        let now = tokio::time::Instant::now();
        let Ok(panes) = self.backend.panes().await else {
            return;
        };
        // pane ごとに1回の呼び鈴へ集約する。
        let mut due: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for stored in self.state.messages.values() {
            if stored.acked || stored.seen || !stored.delivered {
                continue;
            }
            let Some(agent) = self.state.agents.get(&stored.target_pane) else {
                continue;
            };
            if agent.identity() != stored.message.target_identity() {
                continue;
            }
            if !panes.iter().any(|pane| {
                pane.pane_id == stored.target_pane
                    && pane_backs_registration(pane, stored.message.target_identity())
                    && pane.status.accepts_reminder()
            }) {
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
                .push(stored.message.id);
        }
        for (pane, ids) in due {
            let bell = nag_bell(&ids);
            let delivered = self.backend.deliver_reminder(&pane, &bell).await.is_ok();
            // 失敗時も cooldown は消費する。2秒ごとの health tick で連打しない。
            for id in &ids {
                if let Some(stored) = self.state.messages.get_mut(id) {
                    stored.last_nag_at = Some(now);
                }
            }
            if delivered {
                info!(%pane, ?ids, source = "nag", "receipt reminder delivered");
            }
        }
    }

    /// 永続化前に作りかけた通知を in-memory から取り消す。
    fn rollback_notice(&mut self, _sender: &str, id: u64, _dispatch: Dispatch) {
        self.state.discard_message(id);
    }

    /// durable な通知を配達する。失敗しても通知は queue に残り、次の tick で鳴る。
    /// **新しい通知は作らない。**
    async fn deliver_notice(&mut self, sender: &str, id: u64, dispatch: Dispatch) {
        if !matches!(dispatch, Dispatch::Deliver(_)) {
            return;
        }
        let Some(stored) = self.state.message(id) else {
            error!(id, "failure notification body missing before delivery");
            return;
        };
        let bell = stored.message.bell.clone();
        if self.backend.deliver(sender, &bell).await.is_ok() {
            if let Err(error) = self.journal.append(&Record::Complete {
                pane: sender.to_owned(),
                id,
            }) {
                // journal 上は未配達のまま。再起動後に同じ通知が再配達される。
                warn!(%error, id, "cannot complete notice delivery; it will be retried");
                self.state.requeue_after_delivery_failure(sender, id);
                return;
            }
            self.state.complete_delivery(sender, id);
        } else {
            self.state.requeue_after_delivery_failure(sender, id);
        }
    }

    /// 起動時の registry 復旧。要求の受付前に herdr snapshot と必ず1回同期する —
    /// journal の identity が古いまま addressable になると、旧名宛の呼び鈴を
    /// pane の新しい占有者へ送ってしまう。定期 tick と違い、ここでの snapshot
    /// 取得失敗は握り潰さず **起動を失敗させる** (誤配窓を開けたまま ready に
    /// ならない)。
    async fn startup(&mut self) -> Result<()> {
        let panes = self
            .backend
            .panes()
            .await
            .context("起動時の herdr snapshot を取得できません")?;
        self.apply_herdr_snapshot(panes).await;
        Ok(())
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
            "{command} は登録済みのagent pane内で実行してください"
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

/// HOME 上に installed された skill 名 (SKILL.md を持つ dir)。runtime 別 root。
fn installed_skills_for_runtime(home: &Path, runtime: &str) -> Vec<String> {
    use std::collections::BTreeSet;
    let roots: Vec<PathBuf> = match runtime {
        "claude" => vec![home.join(".claude/skills")],
        "codex" => vec![
            home.join(".agents/skills"),
            home.join(".codex/skills"),
            home.join(".codex/skills/.system"),
        ],
        "grok" => vec![home.join(".grok/skills")],
        _ => return Vec::new(),
    };
    let mut skills = BTreeSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if is_safe_token(&name) && entry.path().join("SKILL.md").is_file() {
                skills.insert(name);
            }
        }
    }
    skills.into_iter().collect()
}

fn display_state(status: AgentStatus) -> AgentState {
    match status {
        AgentStatus::Idle | AgentStatus::Done => AgentState::Idle,
        AgentStatus::Working | AgentStatus::Blocked | AgentStatus::Unknown => AgentState::Busy,
    }
}

fn pretty(name: &str, pane: &PaneInfo) -> String {
    format!(
        "{name:<10} {:<5} {}:{}.{} ({})  {}\n",
        display_state(pane.status).as_str(),
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

/// pane がその登録の生存根拠になるか。
///
/// `PaneInfo.name` は herdr の native 検出 (tab label 由来、無ければ runtime
/// 検出名) から導出した identity で、pane ID は位置依存のため占有者が
/// 入れ替わりうる。identity は (name, runtime) の組で一致まで要求する —
/// name だけでは、タブ名を保ったまま runtime が交代した新しい占有者へ
/// 旧名宛ての通知を送り続けることになる。
fn pane_backs_registration(pane: &PaneInfo, registered: Identity<'_>) -> bool {
    pane.name.as_deref() == Some(registered.name)
        && pane.agent.as_deref() == Some(registered.runtime)
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

/// 未読（未 Seen）向けの受領催促。
fn nag_bell(ids: &[u64]) -> String {
    let unread_ids = ids
        .iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[agent-talk] 受領催促: message {unread_ids} が未読のままです。read_message で本文を確認してください。"
    )
}

fn build_brief(
    addr: &str,
    from: &str,
    origin_info: Option<&PaneInfo>,
    reply_info: Option<&PaneInfo>,
    body: &str,
    mode: BriefMode,
    sender: SenderKind,
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
    let title = sender.brief_title();
    format!("{title}\n- from: {from}{origin}\n- to: {addr}\n- reply: {reply}\n\n{body}\n")
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

    use hyper::{
        Method, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    };
    use tempfile::TempDir;

    use super::{
        Broker, Event, HttpEvent, HttpRoute, Journal, MAX_BODY_BYTES, MailboxPageError, Request,
        SendIntent, SendOptions, SendReport, WebAgent, adopt_legacy_journal, capture_failure,
        classify_http, decode_path_segment, installed_skills_for_runtime, parse_mailbox_page,
        parse_mailbox_query, peer_uid_allowed, request_web_agents, rfc3339, static_response,
    };
    use crate::{
        backend::Backend,
        config::Config,
        herdr::{AgentStatus, HerdrPane},
        state::AgentState,
    };

    fn pane_info(pane_id: &str, agent: Option<&str>) -> HerdrPane {
        let workspace = pane_id.split(':').next().unwrap_or("w1");
        HerdrPane {
            pane_id: pane_id.into(),
            terminal_id: format!("term_{pane_id}"),
            workspace_id: workspace.into(),
            workspace_label: Some("test".into()),
            tab_id: format!("{workspace}:t1"),
            tab_label: None,
            cwd: "/tmp".into(),
            agent: agent.map(str::to_owned),
            status: AgentStatus::Idle,
        }
    }

    /// tab label 付きの pane。tab ごとの識別を検証するために `tab_id` も指定する。
    fn tabbed_pane(pane_id: &str, agent: &str, tab_id: &str, label: Option<&str>) -> HerdrPane {
        HerdrPane {
            tab_id: tab_id.into(),
            tab_label: label.map(str::to_owned),
            ..pane_info(pane_id, Some(agent))
        }
    }

    /// workspace label 付きの pane。workspace ごとの sender identity を
    /// 検証するために使う (`tabbed_pane` は workspace label を `test` に固定する)。
    fn workspaced_pane(pane_id: &str, workspace: &str, tab_id: &str, label: &str) -> HerdrPane {
        HerdrPane {
            workspace_label: Some(workspace.into()),
            ..tabbed_pane(pane_id, "claude", tab_id, Some(label))
        }
    }

    /// herdr socket を持たない in-process broker。
    /// journal の durability 契約を failpoint で検証するために必要。
    fn broker(dir: &TempDir, panes: Vec<HerdrPane>) -> Broker {
        let journal_path = dir.path().join("queue.journal");
        let (journal, state) = Journal::open(journal_path.clone()).unwrap();
        Broker {
            state,
            journal,
            pane_resolver: |_, _| Err("tests は pane_resolver を明示注入する".to_owned()),
            backend: Backend::scripted(panes),
            config: Config {
                herdr_socket: PathBuf::new(),
                rpc_socket: PathBuf::new(),
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
            peer_pid: None,
        }
    }

    /// tmux 併存期の journal (tmux socket 名で命名) を herdr 名の journal へ
    /// 一回きり引き継ぐ。未受領 message と採番済み ID を見失って ID を
    /// 再利用しないための移行経路。
    #[tokio::test]
    async fn a_single_legacy_journal_is_adopted_with_its_ids_and_registrations() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("default.journal");
        let (last_id, agents) = {
            let journal_path = legacy_path.clone();
            let (journal, state) = Journal::open(journal_path.clone()).unwrap();
            let mut legacy = Broker {
                state,
                journal,
                pane_resolver: |_, _| Err("tests は pane_resolver を明示注入する".to_owned()),
                backend: Backend::scripted(vec![
                    pane_info("w1:p1", Some("codex")),
                    pane_info("w1:p2", Some("claude")),
                ]),
                config: Config {
                    herdr_socket: PathBuf::new(),
                    rpc_socket: PathBuf::new(),
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
            };
            legacy.sync_herdr_registry().await;
            let sent = json(
                &legacy
                    .handle(send_request("w1:p1", "claude", "carried"))
                    .await,
            );
            (sent["id"].as_u64().unwrap(), legacy.state.agents.len())
        };

        // herdr 名の journal を開く前に、旧 journal がちょうど1つなら引き継ぐ。
        let herdr_path = dir.path().join("herdr.journal");
        adopt_legacy_journal(&herdr_path).unwrap();
        assert!(!legacy_path.exists(), "旧 journal は rename で引き継がれる");
        let (_, mut state) = Journal::open(herdr_path.clone()).unwrap();
        assert_eq!(state.agents.len(), agents, "登録が引き継がれる");
        assert!(
            state.message(last_id).is_some(),
            "未受領 message が引き継がれる"
        );
        assert!(
            state.allocate_id() > last_id,
            "採番が続きから振られ、ID を再利用しない"
        );

        // 既に herdr 名の journal がある場合は何もしない (冪等)。
        std::fs::write(dir.path().join("other.journal"), "junk\n").unwrap();
        adopt_legacy_journal(&herdr_path).unwrap();
        assert!(herdr_path.exists());

        // 候補が複数のときは推測も新規開始もせず、起動を失敗させる
        // (新しい空 journal は ID を再利用してしまう)。
        let fresh = dir.path().join("fresh.journal");
        std::fs::write(dir.path().join("second.journal"), "junk\n").unwrap();
        let error = adopt_legacy_journal(&fresh).unwrap_err();
        assert!(error.to_string().contains("候補が複数"), "{error}");
        assert!(!fresh.exists(), "失敗時に新しい journal を作らない");

        // 候補ゼロ (初回起動) は新規開始でよい。
        let empty = tempfile::tempdir().unwrap();
        adopt_legacy_journal(&empty.path().join("herdr.journal")).unwrap();
    }

    /// pane 申告の無い MCP RPC は、接続の peer PID から呼び出し元 pane を
    /// daemon 側で解決する (env forward 不要化)。
    #[tokio::test]
    async fn a_missing_pane_claim_is_resolved_from_the_peer_process() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;

        broker.pane_resolver = |pid, _| {
            assert_eq!(pid, 4242, "接続から採取した peer PID で解決する");
            Ok("w1:p1".to_owned())
        };
        let mut peers = request("list-peers", None, &[]);
        peers.peer_pid = Some(4242);
        let response = broker.handle(peers).await;
        assert_eq!(response.code, 0, "{}", response.stderr);
        assert_eq!(json(&response)["self"], "w1:p1");

        // 解決の失敗は fail closed で、明示 forward の逃げ道を案内する。
        broker.pane_resolver = |_, _| Err("祖先に identity が見つかりません".to_owned());
        let mut denied = request("list-peers", None, &[]);
        denied.peer_pid = Some(4242);
        let response = broker.handle(denied).await;
        assert_eq!(response.code, 1, "{response:?}");
        assert!(
            response.stderr.contains("HERDR_PANE_ID"),
            "{}",
            response.stderr
        );

        // 外部 caller 用 command は pane 無しのまま扱い、resolver を呼ばない。
        broker.pane_resolver = |_, _| panic!("外部 caller 経路で resolver を呼んではならない");
        broker.config.allowed_sources.insert("mobile".into());
        let mut mailbox = request("mailbox-list", None, &["mobile"]);
        mailbox.peer_pid = Some(4242);
        let response = broker.handle(mailbox).await;
        assert_eq!(response.code, 0, "{}", response.stderr);
    }

    /// herdr 発行の id は opaque な文字列 — 文法検証はせず、registry への
    /// 完全一致だけが pane 直指定になる (65c83bb の全停止事故の再発防止)。
    #[tokio::test]
    async fn herdr_issued_ids_are_opaque_and_resolve_by_exact_registry_match() {
        let dir = tempfile::tempdir().unwrap();
        // 将来の採番を模した、旧文法に一切載らない id。
        let weird = "pane/α:next?";
        let mut broker = broker(
            &dir,
            vec![
                pane_info(weird, Some("codex")),
                pane_info("w1:p2", Some("claude")),
            ],
        );
        broker.sync_herdr_registry().await;
        assert!(broker.state.agents.contains_key(weird), "登録に載る");

        // registry 完全一致は pane 直指定として解決する。
        let resolved = broker.resolve(weird, Some("w1:p2")).await.unwrap();
        assert_eq!(resolved.1, "codex");
        // registry に無い文字列は名前として解釈され、daemon は落ちずに不在エラー。
        let error = broker.resolve("w9:p9", Some("w1:p2")).await.unwrap_err();
        assert!(error.stderr.contains("見つかりません"), "{}", error.stderr);

        // 配達も opaque id のまま herdr へ渡って成立する。
        let sent = json(&broker.handle(send_request("w1:p2", weird, "hello")).await);
        assert_eq!(sent["path"], "sent");
        assert_eq!(sent["to"], weird);
    }

    /// register は herdr の native identity と一致する名前しか受理しない。
    #[tokio::test]
    async fn register_refuses_a_name_that_contradicts_the_native_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(&dir, vec![pane_info("w1:p1", Some("codex"))]);
        broker.sync_herdr_registry().await;

        let refused = broker
            .handle(request("register", Some("w1:p1"), &["gemini"]))
            .await;
        assert_eq!(refused.code, 1, "{refused:?}");
        assert!(refused.stderr.contains("一致しない"), "{}", refused.stderr);
        assert_eq!(
            broker.state.agents["w1:p1"].name, "codex",
            "native identity と違う名前で登録が上書きされてはならない"
        );

        // agent の居ない pane・未知の pane も拒否する。
        let empty = broker
            .handle(request("register", Some("w1:p9"), &["codex"]))
            .await;
        assert_eq!(empty.code, 1, "{empty:?}");

        // 一致する register は冪等成功 (hook 互換)。
        let ok = broker
            .handle(request("register", Some("w1:p1"), &["codex"]))
            .await;
        assert_eq!(ok.code, 0, "{}", ok.stderr);
        assert_eq!(broker.state.agents["w1:p1"].name, "codex");
    }

    /// 起動時は snapshot が取れない限り ready にならない (誤配窓を開けない)。
    #[tokio::test]
    async fn startup_refuses_to_serve_without_a_herdr_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(&dir, vec![]);
        broker.backend = Backend::new(crate::herdr::Herdr::new("/nonexistent/herdr.sock".into()));
        assert!(broker.startup().await.is_err());
    }

    fn send_request(pane: &str, to: &str, body: &str) -> Request {
        Request {
            command: "send-message".into(),
            args: vec![to.to_owned()],
            stdin: body.to_owned(),
            pane: Some(pane.to_owned()),
            send_options: Some(SendOptions::default()),
            peer_pid: None,
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
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p2", Some("claude")),
            ],
        );
        for (pane, name) in [("w1:p1", "codex"), ("w1:p2", "claude")] {
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
        let sent = broker.handle(send_request("w1:p1", "claude", "body")).await;
        assert_eq!(sent.code, 0, "{}", sent.stderr);
        let id = json(&sent)["id"].as_u64().unwrap();

        // %9 は登録されていない。4つの RPC すべてが拒否される。
        for (command, args) in [
            ("send-message", vec!["claude"]),
            ("read-message", vec![id.to_string().as_str()]),
            ("ack-message", vec![id.to_string().as_str()]),
            ("list-peers", vec![]),
        ] {
            let mut denied = request(command, Some("w1:p9"), &args);
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
            .handle(request("ack-message", Some("w1:p9"), &["4242"]))
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
        // pane 由来でない送信者は workspace を捕捉しないので、呼び鈴も bare のまま。
        assert_eq!(stored.message.sender_workspace, None);
        assert_eq!(stored.message.sender_full_label(), "human");
        assert!(
            stored.message.bell.contains("human から依頼が届きました"),
            "{}",
            stored.message.bell
        );
    }

    /// 呼び鈴と brief の文言は送信元で分かれる。登録 agent pane からの送信は
    /// 「連絡」として提示し (peer message は user 権限を運ばないので作業指示に
    /// しない)、未登録の human caller からの送信は「依頼」のまま維持する。
    #[tokio::test]
    async fn peer_sends_read_as_a_notice_while_human_sends_stay_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;

        // 登録 agent pane (w1:p1 = codex) からの送信。
        let peer = json(&broker.handle(send_request("w1:p1", "claude", "body")).await);
        let peer_id = peer["id"].as_u64().unwrap();
        let peer_message = broker.state.message(peer_id).unwrap().message.clone();
        assert_eq!(
            peer_message.bell,
            format!(
                "[agent-talk] test/codex から連絡が届きました。read_message {peer_id} で本文を確認してください。"
            )
        );
        assert!(
            peer_message
                .brief
                .starts_with("# agent-talk 連絡\n- from: codex"),
            "{}",
            peer_message.brief
        );

        // 未登録の human caller からの送信。
        let mut human = request("send", None, &["claude", "body"]);
        human.pane = None;
        let response = broker.handle(human).await;
        assert_eq!(response.code, 0, "{}", response.stderr);
        let human_message = broker
            .state
            .messages
            .values()
            .find(|stored| stored.message.sender == "human")
            .expect("human caller の message")
            .message
            .clone();
        assert!(
            human_message
                .bell
                .contains("human から依頼が届きました。read_message"),
            "{}",
            human_message.bell
        );
        assert!(
            human_message
                .brief
                .starts_with("# agent-talk 依頼書\n- from: human"),
            "{}",
            human_message.brief
        );
    }

    /// `--no-reply` の呼び鈴は byte 単位で維持する (docs/design.md「既定の送信文言は
    /// byte単位で維持し、no-reply時だけ『返信は不要』と明示する」)。
    #[tokio::test]
    async fn a_no_reply_peer_send_keeps_its_exact_bell() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let mut oneway = send_request("w1:p1", "claude", "body");
        oneway.send_options = Some(SendOptions {
            no_reply: true,
            ..SendOptions::default()
        });
        let sent = json(&broker.handle(oneway).await);
        let id = sent["id"].as_u64().unwrap();
        let message = broker.state.message(id).unwrap().message.clone();
        assert_eq!(
            message.bell,
            format!(
                "[agent-talk] test/codex から連絡が届きました。read_message {id} で本文を確認してください。返信は不要です。"
            )
        );
        assert!(
            message
                .brief
                .starts_with("# agent-talk 連絡\n- from: codex"),
            "{}",
            message.brief
        );
    }

    #[tokio::test]
    async fn send_message_rpc_returns_versioned_json_for_both_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let sent = json(&broker.handle(send_request("w1:p1", "claude", "body")).await);
        assert_eq!(sent["version"], 1);
        assert_eq!(sent["path"], "sent");
        assert_eq!(sent["to"], "w1:p2");
        assert_eq!(sent["name"], "claude");

        let second = json(
            &broker
                .handle(send_request("w1:p1", "claude", "second"))
                .await,
        );
        assert_eq!(second["version"], 1);
        assert_eq!(second["path"], "sent");
        assert!(second["id"].as_u64().unwrap() > sent["id"].as_u64().unwrap());
    }

    #[tokio::test]
    async fn read_v1_reports_the_identity_captured_at_send_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let id = json(&broker.handle(send_request("w1:p1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();

        let read = json(
            &broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(read["from"], "codex");
        assert_eq!(read["reply_to"], "w1:p1");

        // w1:p1 が別 agent に置き換わっても from は変わらず、reply_to は消える。
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("gemini")),
                pane_info("w1:p2", Some("claude")),
            ],
        );
        broker.sync_herdr_registry().await;
        let after = json(
            &broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
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
            json(&broker.handle(send_request("w1:p1", "claude", "body")).await)["id"]
                .as_u64()
                .unwrap()
        };
        let mut restarted = broker(
            &dir,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p2", Some("claude")),
            ],
        );
        let read = json(
            &restarted
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(read["from"], "codex");
        assert_eq!(read["reply_to"], "w1:p1");
    }

    #[tokio::test]
    async fn a_seen_journal_append_failure_stays_unseen_and_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let id = json(&broker.handle(send_request("w1:p1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();
        let journal_len = std::fs::metadata(&broker.config.journal).unwrap().len();

        broker.journal.fail_next_appends(1);
        let failed = broker
            .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
            .await;
        assert_eq!(failed.code, 1, "{failed:?}");
        assert!(failed.stderr.contains("injected journal append failure"));
        assert!(!broker.state.message(id).unwrap().seen);
        assert_eq!(broker.state.pending_to_me("w1:p2"), vec![id]);
        assert_eq!(
            std::fs::metadata(&broker.config.journal).unwrap().len(),
            journal_len
        );

        let reread = broker
            .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
            .await;
        assert_eq!(reread.code, 0, "{}", reread.stderr);
        assert!(json(&reread)["body"].as_str().unwrap().contains("body"));
        assert!(broker.state.message(id).unwrap().seen);
        assert!(broker.state.pending_to_me("w1:p2").is_empty());
    }

    #[tokio::test]
    async fn owner_can_pull_read_a_queued_message_without_a_doorbell() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                HerdrPane {
                    status: AgentStatus::Blocked,
                    ..pane_info("w1:p2", Some("claude"))
                },
            ],
        );
        let id = json(
            &broker
                .handle(send_request("w1:p1", "claude", "pull me"))
                .await,
        )["id"]
            .as_u64()
            .unwrap();
        assert!(!broker.state.message(id).unwrap().delivered);
        assert!(broker.state.is_queued("w1:p2", id));
        assert_eq!(
            broker.state.pending_to_me("w1:p2"),
            vec![id],
            "queue 中でも pending_to_me に現れる"
        );
        let bells_before = bells(&broker).len();

        // 他 pane は拒否。
        let denied = broker
            .handle(request("read-message", Some("w1:p1"), &[&id.to_string()]))
            .await;
        assert_eq!(denied.code, 1);
        assert!(
            denied.stderr.contains("このpane宛ではありません"),
            "{}",
            denied.stderr
        );

        // 未読の ack は空操作。queue も pending も動かない。
        let early_ack = json(
            &broker
                .handle(request("ack-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(early_ack["outcome"], "acked");
        assert!(!broker.state.message(id).unwrap().seen);
        assert!(broker.state.is_queued("w1:p2", id));
        assert_eq!(broker.state.pending_to_me("w1:p2"), vec![id]);

        // 宛先本人の pull read: Complete が durable になり本文が返る。呼び鈴は増えない。
        let journal_len = std::fs::metadata(&broker.config.journal).unwrap().len();
        let read = json(
            &broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert!(
            read["body"].as_str().unwrap().contains("pull me"),
            "{}",
            read["body"]
        );
        assert_eq!(read["from"], "codex");
        assert!(broker.state.message(id).unwrap().delivered);
        assert!(broker.state.message(id).unwrap().seen);
        assert!(
            !broker.state.is_queued("w1:p2", id),
            "pull で queue から外れる"
        );
        assert!(
            std::fs::metadata(&broker.config.journal).unwrap().len() > journal_len,
            "Complete が journal に残る"
        );
        assert_eq!(
            bells(&broker).len(),
            bells_before,
            "pull は agent.prompt を撃たない"
        );

        // その後の drain も同じ ID の呼び鈴を鳴らさない。
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p2", Some("claude")),
            ],
        );
        broker.drain_queued().await;
        assert_eq!(bells(&broker).len(), bells_before);

        // ack は pull 後に成功し、sender の pending から消える。
        let acked = json(
            &broker
                .handle(request("ack-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(acked["outcome"], "acked");
        assert!(broker.state.pending_to_me("w1:p2").is_empty());
        assert!(
            broker
                .state
                .pending_from_me("w1:p1")
                .get("w1:p2")
                .is_none_or(Vec::is_empty)
        );
    }

    #[tokio::test]
    async fn pull_delivery_survives_restart_and_still_requires_ack() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let mut broker = registered_pair(&dir).await;
            set_herdr_panes(
                &broker,
                vec![
                    pane_info("w1:p1", Some("codex")),
                    HerdrPane {
                        status: AgentStatus::Blocked,
                        ..pane_info("w1:p2", Some("claude"))
                    },
                ],
            );
            let id = json(
                &broker
                    .handle(send_request("w1:p1", "claude", "durable pull"))
                    .await,
            )["id"]
                .as_u64()
                .unwrap();
            let read = broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await;
            assert_eq!(read.code, 0, "{}", read.stderr);
            id
        };
        let mut restarted = broker(
            &dir,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p2", Some("claude")),
            ],
        );
        assert!(
            restarted.state.message(id).unwrap().delivered,
            "Complete が replay される"
        );
        assert!(!restarted.state.is_queued("w1:p2", id));
        assert!(
            restarted.state.pending_to_me("w1:p2").is_empty(),
            "pull read の Seen は restart 後も残る"
        );
        let reread = json(
            &restarted
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert!(
            reread["body"].as_str().unwrap().contains("durable pull"),
            "{}",
            reread["body"]
        );
        let acked = json(
            &restarted
                .handle(request("ack-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(acked["outcome"], "acked");
        let still = json(
            &restarted
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert!(
            still["body"].as_str().unwrap().contains("durable pull"),
            "ack しても本文は残る"
        );
    }

    #[tokio::test]
    async fn pull_read_refuses_to_overtake_an_earlier_queued_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                HerdrPane {
                    status: AgentStatus::Blocked,
                    ..pane_info("w1:p2", Some("claude"))
                },
            ],
        );
        let first = json(
            &broker
                .handle(send_request("w1:p1", "claude", "first"))
                .await,
        )["id"]
            .as_u64()
            .unwrap();
        let second = json(
            &broker
                .handle(send_request("w1:p1", "claude", "second"))
                .await,
        )["id"]
            .as_u64()
            .unwrap();
        assert_eq!(broker.state.queued_head("w1:p2"), Some(first));
        assert!(broker.state.is_queued("w1:p2", second));
        let journal_before = std::fs::read(&broker.config.journal).unwrap();

        // 2件目の先取り pull は拒否。journal / queue は不変。
        let skipped = broker
            .handle(request(
                "read-message",
                Some("w1:p2"),
                &[&second.to_string()],
            ))
            .await;
        assert_eq!(skipped.code, 1, "{skipped:?}");
        assert!(
            skipped.stderr.contains("queue 先頭ではありません"),
            "{}",
            skipped.stderr
        );
        assert!(skipped.stderr.contains(&format!("#{first}")));
        assert!(!broker.state.message(second).unwrap().delivered);
        assert!(!broker.state.message(second).unwrap().seen);
        assert!(broker.state.is_queued("w1:p2", first));
        assert!(broker.state.is_queued("w1:p2", second));
        assert_eq!(
            std::fs::read(&broker.config.journal).unwrap(),
            journal_before,
            "先取り拒否は journal を書かない"
        );

        // 先頭 → 2件目の順なら成功。
        let read_first = broker
            .handle(request(
                "read-message",
                Some("w1:p2"),
                &[&first.to_string()],
            ))
            .await;
        assert_eq!(read_first.code, 0, "{}", read_first.stderr);
        assert!(broker.state.message(first).unwrap().delivered);
        assert_eq!(broker.state.queued_head("w1:p2"), Some(second));
        let read_second = broker
            .handle(request(
                "read-message",
                Some("w1:p2"),
                &[&second.to_string()],
            ))
            .await;
        assert_eq!(read_second.code, 0, "{}", read_second.stderr);
        assert!(broker.state.message(second).unwrap().delivered);
        assert!(!broker.state.is_queued("w1:p2", second));
    }

    #[tokio::test]
    async fn pull_read_journal_failure_leaves_queue_and_returns_no_body() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                HerdrPane {
                    status: AgentStatus::Blocked,
                    ..pane_info("w1:p2", Some("claude"))
                },
            ],
        );
        let id = json(&broker.handle(send_request("w1:p1", "claude", "hold")).await)["id"]
            .as_u64()
            .unwrap();
        broker.journal.fail_next_appends(1);
        let failed = broker
            .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
            .await;
        assert_eq!(failed.code, 1, "{failed:?}");
        assert!(
            failed.stderr.contains("injected journal append failure"),
            "{}",
            failed.stderr
        );
        assert!(!broker.state.message(id).unwrap().delivered);
        assert!(!broker.state.message(id).unwrap().seen);
        assert!(broker.state.is_queued("w1:p2", id));
    }

    /// `%1` に届いた、`original` に対する未受領通知の数。
    fn notices_for(broker: &Broker, original: u64) -> usize {
        notices_addressed_to(broker, "w1:p1", original)
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
        let original = json(&broker.handle(send_request("w1:p1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();
        set_herdr_panes(&broker, vec![pane_info("w1:p1", Some("codex"))]);
        (broker, original)
    }

    #[tokio::test]
    async fn a_failed_notice_append_creates_no_notice_and_blocks_the_removal() {
        let dir = tempfile::tempdir().unwrap();
        let (mut broker, original) = pending_then_target_vanishes(&dir).await;

        broker.journal.fail_next_appends(1);
        assert!(
            !broker.remove_agent("w1:p2", "宛先が退出した").await,
            "通知を永続化できない間は remove しない"
        );
        assert!(
            broker.state.agents.contains_key("w1:p2"),
            "早すぎる Remove が起きてはならない"
        );
        assert!(
            !broker.state.message(original).unwrap().acked,
            "original は Pending のまま"
        );
        assert_eq!(notices_for(&broker, original), 0, "通知はまだ作られない");

        // 通常の retry 経路 (pull 同期の欠落 evict) がちょうど1通だけ作る。
        broker.sync_herdr_registry().await;
        broker.sync_herdr_registry().await;
        assert_eq!(notices_for(&broker, original), 1);
        assert!(broker.state.message(original).unwrap().acked);
        assert!(!broker.state.agents.contains_key("w1:p2"));

        // 何度同期しても増えない。
        for _ in 0..3 {
            broker.sync_herdr_registry().await;
        }
        assert_eq!(notices_for(&broker, original), 1);
    }

    #[tokio::test]
    async fn notice_delivery_failures_never_produce_a_second_notice() {
        let dir = tempfile::tempdir().unwrap();
        let (mut broker, original) = pending_then_target_vanishes(&dir).await;

        // Enqueue{retires} だけ通し、配達系の append をすべて失敗させる。
        broker.journal.fail_appends_after(1);
        broker.remove_agent("w1:p2", "宛先が退出した").await;
        broker.journal.clear_failpoints();

        assert_eq!(notices_for(&broker, original), 1);
        assert!(
            broker.state.message(original).unwrap().acked,
            "通知が durable になった時点で original は退役している"
        );
        // 配達に失敗した通知は queue に残り、次の health tick で再試行する。
        assert!(broker.state.agents["w1:p1"].queue.len() == 1);

        for _ in 0..3 {
            broker.sync_herdr_registry().await;
        }
        assert_eq!(
            notices_for(&broker, original),
            1,
            "pull 同期が2通目を作ってはならない"
        );
    }

    #[tokio::test]
    async fn the_notice_and_the_retirement_survive_a_restart_as_one_unit() {
        let dir = tempfile::tempdir().unwrap();
        let original = {
            let (mut broker, original) = pending_then_target_vanishes(&dir).await;
            // 通知が durable になった直後にプロセスが落ちる状況を模す。
            broker.journal.fail_appends_after(1);
            broker.remove_agent("w1:p2", "宛先が退出した").await;
            original
        };
        // 再起動: replay 後も original は退役済み、通知はちょうど1通。
        let mut restarted = broker(&dir, vec![pane_info("w1:p1", Some("codex"))]);
        assert!(
            restarted
                .state
                .message(original)
                .is_none_or(|stored| stored.acked),
            "replay 後も original は Pending に戻らない"
        );
        assert_eq!(notices_for(&restarted, original), 1);
        restarted.sync_herdr_registry().await;
        restarted.sync_herdr_registry().await;
        assert_eq!(
            notices_for(&restarted, original),
            1,
            "再起動後の pull 同期も2通目を作らない"
        );
    }

    /// scripted herdr が配達した (pane, bell) の記録。
    fn bells(broker: &Broker) -> Vec<(String, String)> {
        broker.backend.herdr().delivered.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn reclaimed_messages_collapse_into_one_notice_per_sender() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(
            &dir,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p2", Some("claude")),
                pane_info("w1:p3", Some("cursor")),
            ],
        );
        for (pane, name) in [("w1:p1", "codex"), ("w1:p2", "claude"), ("w1:p3", "cursor")] {
            let response = broker
                .handle(request("register", Some(pane), &[name]))
                .await;
            assert_eq!(response.code, 0, "{}", response.stderr);
        }
        let mut from_codex = Vec::new();
        for body in ["first", "second", "third"] {
            from_codex.push(
                json(&broker.handle(send_request("w1:p1", "claude", body)).await)["id"]
                    .as_u64()
                    .unwrap(),
            );
        }
        let from_cursor = json(
            &broker
                .handle(send_request("w1:p3", "claude", "fourth"))
                .await,
        )["id"]
            .as_u64()
            .unwrap();

        // %2 が4通 (未受領) を抱えたまま消える。
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p3", Some("cursor")),
            ],
        );
        let before = bells(&broker).len();
        assert!(
            broker
                .remove_agent("w1:p2", "宛先エージェントが退出した")
                .await
        );

        // 通知は送信元ごとにちょうど1通 (bell も1回ずつ)。
        let to_codex: Vec<_> = broker
            .state
            .messages
            .values()
            .filter(|stored| stored.target_pane == "w1:p1")
            .collect();
        assert_eq!(to_codex.len(), 1, "codex への通知は1通に集約される");
        let brief = &to_codex[0].message.brief;
        for id in &from_codex {
            assert!(
                brief.contains(&format!("## 元のメッセージ #{id}")),
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
            .filter(|stored| stored.target_pane == "w1:p3")
            .collect();
        assert_eq!(to_cursor.len(), 1, "cursor への通知は独立に1通");
        assert!(to_cursor[0].message.brief.contains("## 元のメッセージ #"));

        let new_bells: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(new_bells.len(), 2, "send-keys は送信元 pane ごとに1回だけ");
        let codex_bell = &new_bells
            .iter()
            .find(|(pane, _)| pane == "w1:p1")
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
                        json(&broker.handle(send_request("w1:p1", "claude", body)).await)["id"]
                            .as_u64()
                            .unwrap(),
                    );
                }
                ids
            };
            set_herdr_panes(&broker, vec![pane_info("w1:p1", Some("codex"))]);
            assert!(
                broker
                    .remove_agent("w1:p2", "宛先エージェントが退出した")
                    .await
            );
            ids
        };
        // 再起動 (replay): also_retires の分も含めて Pending へ戻らない。
        let restarted = broker(&dir, vec![pane_info("w1:p1", Some("codex"))]);
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
            .filter(|stored| stored.target_pane == "w1:p1" && !stored.acked)
            .count();
        assert_eq!(notices, 1, "通知は restart 後も1通のまま");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unread_message_respects_nag_delay_and_cooldown_then_stops_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let id = json(&broker.handle(send_request("w1:p1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();

        let before = bells(&broker).len();
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before, "1分経過前は催促しない");

        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        broker.nag_unacked().await;
        let after: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(after.len(), 1, "催促はちょうど1回");
        assert_eq!(after[0].0, "w1:p2");
        assert!(
            after[0].1.contains("受領催促")
                && after[0].1.contains(&format!("#{id}"))
                && after[0].1.contains("read_message"),
            "{}",
            after[0].1
        );

        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        broker.nag_unacked().await;
        broker.nag_unacked().await;
        assert_eq!(
            bells(&broker).len(),
            before + 1,
            "cooldown 中は再催促しない"
        );

        tokio::time::advance(std::time::Duration::from_mins(5)).await;
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before + 2, "cooldown 明けに1回");

        let read = broker
            .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
            .await;
        assert_eq!(read.code, 0, "{}", read.stderr);
        tokio::time::advance(std::time::Duration::from_mins(10)).await;
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before + 2, "read 後は催促しない");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unread_message_is_nagged_to_read_and_a_working_target_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let id = json(&broker.handle(send_request("w1:p1", "claude", "body")).await)["id"]
            .as_u64()
            .unwrap();

        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                HerdrPane {
                    status: AgentStatus::Working,
                    ..pane_info("w1:p2", Some("claude"))
                },
            ],
        );
        // herdr が working の間は催促しない。
        let before = bells(&broker).len();
        tokio::time::advance(std::time::Duration::from_mins(2)).await;
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before, "working 中は催促しない");

        // herdr が idle に戻ると未読向けの文言で催促される。
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p2", Some("claude")),
            ],
        );
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
            .web_letter("mobile".into(), "claude".into(), "letter body".into(), None)
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.code, "source_not_allowed");
        assert_eq!(broker.state.messages.len(), 0, "拒否は mutation を残さない");

        // 許可すると CLI の send --from と同一経路で受理される (versioned JSON)。
        broker.config.allowed_sources.insert("mobile".into());
        let accepted = broker
            .web_letter("mobile".into(), "claude".into(), "letter body".into(), None)
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
            .web_letter("mobile".into(), "ghost".into(), "x".into(), None)
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn installed_skills_collect_only_safe_skill_md_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".claude/skills");
        std::fs::create_dir_all(root.join("deliver")).unwrap();
        std::fs::write(root.join("deliver/SKILL.md"), "# deliver\n").unwrap();
        std::fs::create_dir_all(root.join("polish")).unwrap();
        std::fs::write(root.join("polish/SKILL.md"), "# polish\n").unwrap();
        // SKILL.md が無い dir は無視。
        std::fs::create_dir_all(root.join("no-md")).unwrap();
        // unsafe token は無視。
        std::fs::create_dir_all(root.join("Bad Name")).unwrap();
        std::fs::write(root.join("Bad Name/SKILL.md"), "# bad\n").unwrap();

        let found = installed_skills_for_runtime(dir.path(), "claude");
        assert_eq!(found, vec!["deliver".to_owned(), "polish".to_owned()]);
        assert!(installed_skills_for_runtime(dir.path(), "unknown").is_empty());
    }

    #[tokio::test]
    async fn web_skills_filters_by_syntax_allowlist_and_unknown_pane() {
        let dir = tempfile::tempdir().unwrap();
        let skill_root = dir.path().join(".claude/skills");
        std::fs::create_dir_all(skill_root.join("deliver")).unwrap();
        std::fs::write(skill_root.join("deliver/SKILL.md"), "# d\n").unwrap();
        std::fs::create_dir_all(skill_root.join("polish")).unwrap();
        std::fs::write(skill_root.join("polish/SKILL.md"), "# p\n").unwrap();

        let mut broker = registered_pair(&dir).await;
        // 一時 HOME を差し替え、installed 走査を tempdir へ向ける。
        // SAFETY: 単体テスト内で HOME を差し替える。他テストと並列に HOME を
        // 読む経路へ影響し得るが、本 file の web_skills テストは直列で十分。
        let previous_home = std::env::var_os("HOME");
        // env 変更は process 全体に効く。後始末を確実に。
        unsafe {
            std::env::set_var("HOME", dir.path());
        }

        // skill_syntax が無いと空。
        assert_eq!(broker.web_skills("w1:p2").unwrap(), Vec::<String>::new());

        broker
            .config
            .skill_syntax
            .insert("claude".into(), crate::config::SkillSyntax::Slash);
        let listed = broker.web_skills("w1:p2").unwrap();
        assert_eq!(listed, vec!["deliver".to_owned(), "polish".to_owned()]);

        broker.config.allowed_skills = Some(["deliver".into()].into_iter().collect());
        assert_eq!(
            broker.web_skills("w1:p2").unwrap(),
            vec!["deliver".to_owned()]
        );

        let err = broker.web_skills("missing:p9").unwrap_err();
        assert_eq!(err.status, StatusCode::NOT_FOUND);

        if let Some(home) = previous_home {
            unsafe {
                std::env::set_var("HOME", home);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
    }

    #[tokio::test]
    async fn web_letter_skill_uses_send_path_and_rejects_unsafe() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        broker.config.allowed_sources.insert("mobile".into());
        broker
            .config
            .skill_syntax
            .insert("claude".into(), crate::config::SkillSyntax::Slash);
        broker.config.allowed_skills = Some(["deliver".into()].into_iter().collect());

        // 許可 skill は sent になり、mailbox event に skill が載る。
        let accepted = broker
            .web_letter(
                "mobile".into(),
                "claude".into(),
                "with skill".into(),
                Some("deliver".into()),
            )
            .await
            .unwrap();
        assert_eq!(accepted["path"], "sent");
        let events = broker.state.mailbox_events("mobile", None, 10);
        assert_eq!(events[0].skill.as_deref(), Some("deliver"));
        let bells = bells(&broker);
        assert!(
            bells
                .last()
                .is_some_and(|(_, text)| text.contains("/deliver ")),
            "bell に skill prefix が付く: {bells:?}"
        );

        let before = broker.state.messages.len();
        // 空 skill は safe-token で拒否。
        let err = broker
            .web_letter(
                "mobile".into(),
                "claude".into(),
                "empty skill".into(),
                Some(String::new()),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, "skill_rejected");
        assert_eq!(broker.state.messages.len(), before, "拒否は痕跡を残さない");

        // allowlist 外も拒否。
        let err = broker
            .web_letter(
                "mobile".into(),
                "claude".into(),
                "bad skill".into(),
                Some("polish".into()),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, "skill_rejected");
        assert_eq!(broker.state.messages.len(), before);
    }

    #[tokio::test]
    async fn scopes_resolve_by_label_alias_cwd_and_compat_prefix() {
        let dir = tempfile::tempdir().unwrap();
        // label "settings" (w1) に codex と claude、label "knowledge" (w2) に
        // claude が居る、という実在に近い配置。
        let settings_codex = HerdrPane {
            workspace_label: Some("settings".into()),
            ..pane_info("w1:p1", Some("codex"))
        };
        let settings_claude = HerdrPane {
            workspace_label: Some("settings".into()),
            ..pane_info("w1:p2", Some("claude"))
        };
        let knowledge_claude = HerdrPane {
            workspace_label: Some("knowledge".into()),
            cwd: "/home/miyabi/.local/share/arona-knowledge".into(),
            ..pane_info("w2:p1", Some("claude"))
        };
        let mut broker = broker(
            &dir,
            vec![settings_codex, settings_claude, knowledge_claude],
        );
        broker.sync_herdr_registry().await;

        // workspace label で引ける。
        let resolved = broker
            .resolve("knowledge/claude", Some("w1:p1"))
            .await
            .unwrap();
        assert_eq!(resolved.0, "w2:p1");
        // workspace_id alias の後方互換。
        let resolved = broker.resolve("w2/claude", Some("w1:p1")).await.unwrap();
        assert_eq!(resolved.0, "w2:p1");
        // cwd basename でも従来どおり。
        let resolved = broker
            .resolve("arona-knowledge/claude", Some("w1:p1"))
            .await
            .unwrap();
        assert_eq!(resolved.0, "w2:p1");
        // tmux 併存期の正式名称 `herdr/<scope>/<name>` は互換 alias として通る。
        let resolved = broker
            .resolve("herdr/knowledge/claude", Some("w1:p1"))
            .await
            .unwrap();
        assert_eq!(resolved.0, "w2:p1");
        // 不明な backend prefix は明示エラー。
        let error = broker
            .resolve("wayland/settings/codex", Some("w1:p2"))
            .await
            .unwrap_err();
        assert!(error.stderr.contains("不明"), "{}", error.stderr);

        // bare 名は同一 workspace 内で一意なら解決する。
        let resolved = broker.resolve("claude", Some("w1:p1")).await.unwrap();
        assert_eq!(resolved.0, "w1:p2");
        // scope 外の bare 名は不在エラー (暗黙に workspace を跨がない)。
        let error = broker.resolve("codex", Some("w2:p1")).await.unwrap_err();
        assert!(error.stderr.contains("見つかりません"), "{}", error.stderr);

        // 同一 workspace に候補が複数並ぶ bare 名は曖昧エラーで指定方法を案内する。
        set_herdr_panes(
            &broker,
            vec![
                HerdrPane {
                    workspace_label: Some("settings".into()),
                    ..pane_info("w1:p1", Some("codex"))
                },
                HerdrPane {
                    workspace_label: Some("settings".into()),
                    ..pane_info("w1:p2", Some("claude"))
                },
                HerdrPane {
                    workspace_label: Some("settings".into()),
                    ..pane_info("w1:p3", Some("claude"))
                },
            ],
        );
        broker.sync_herdr_registry().await;
        let error = broker.resolve("claude", Some("w1:p1")).await.unwrap_err();
        assert!(error.stderr.contains("<scope>/<name>"), "{}", error.stderr);
    }

    /// scripted herdr の一覧を差し替える (tick の合間の変化を模す)。
    fn set_herdr_panes(broker_ref: &Broker, panes: Vec<HerdrPane>) {
        *broker_ref
            .backend
            .herdr()
            .scripted
            .as_ref()
            .unwrap()
            .lock()
            .unwrap() = panes;
    }

    #[tokio::test]
    async fn herdr_agents_are_pulled_in_without_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(
            &dir,
            vec![pane_info("w1:p7", Some("grok")), pane_info("w1:p8", None)],
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
    async fn a_successful_snapshot_evicts_a_missing_agent_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p7", Some("grok")),
            ],
        );
        broker.sync_herdr_registry().await;
        // %1 から grok 宛てに送る (scripted herdr は配達 socket を持たないので queue 行き)。
        let sent = json(
            &broker
                .handle(send_request("w1:p1", "w1:p7", "for grok"))
                .await,
        );
        let pending_id = sent["id"].as_u64().unwrap();

        // 成功 snapshot から消えた時点で退出確定。pen-cli が同じ space を
        // 高速再作成しても、古い宛先を次 tick まで残さない。
        set_herdr_panes(
            &broker,
            vec![pane_info("w1:p1", Some("codex")), pane_info("w1:p7", None)],
        );
        broker.sync_herdr_registry().await;
        assert!(!broker.state.agents.contains_key("w1:p7"));
        assert_eq!(
            notices_for(&broker, pending_id),
            1,
            "未受領は送信元へ回収通知される"
        );
    }

    #[tokio::test]
    async fn every_send_rechecks_herdr_and_delivers_without_turn_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;

        let first = json(
            &broker
                .handle(send_request("w1:p1", "claude", "first"))
                .await,
        );
        let second = json(
            &broker
                .handle(send_request("w1:p1", "claude", "second"))
                .await,
        );

        assert_eq!(first["path"], "sent");
        assert_eq!(second["path"], "sent", "herdr は引き続き idle");
        let delivered = bells(&broker);
        assert_eq!(delivered.len(), 2, "hook を待たず2通ともpromptする");
        assert!(delivered[0].1.contains("read_message 0"), "{delivered:?}");
        assert!(delivered[1].1.contains("read_message 1"), "{delivered:?}");
    }

    #[tokio::test]
    async fn a_waiting_target_still_honors_the_queue_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        broker.config.queue_limit = 1;
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                HerdrPane {
                    status: AgentStatus::Blocked,
                    ..pane_info("w1:p2", Some("claude"))
                },
            ],
        );

        let first = json(
            &broker
                .handle(send_request("w1:p1", "claude", "first"))
                .await,
        );
        assert_eq!(first["path"], "queued");
        let journal_before = std::fs::read(&broker.config.journal).unwrap();

        let second = broker
            .handle(send_request("w1:p1", "claude", "second"))
            .await;
        assert!(second.stderr.contains("キュー保持上限"), "{second:?}");
        assert_eq!(broker.state.queue_len("w1:p2"), 1);
        assert_eq!(
            std::fs::read(&broker.config.journal).unwrap(),
            journal_before,
            "拒否した message を journal に追記しない"
        );
    }

    #[tokio::test]
    async fn an_identity_swap_takes_over_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p7", Some("grok")),
            ],
        );
        broker.sync_herdr_registry().await;
        let sent = json(
            &broker
                .handle(send_request("w1:p1", "w1:p7", "for grok"))
                .await,
        );
        let pending_id = sent["id"].as_u64().unwrap();

        // 別 identity は強い証拠: debounce せず旧登録を回収して新 identity を登録。
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p7", Some("gemini")),
            ],
        );
        broker.sync_herdr_registry().await;
        assert_eq!(broker.state.agents["w1:p7"].name, "gemini");
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
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p7", Some("grok")),
            ],
        );
        broker.sync_herdr_registry().await;
        // snapshot が取れない間は、何度 tick が回っても判定を進めない。
        broker.backend = Backend::new(crate::herdr::Herdr::new("/nonexistent/herdr.sock".into()));
        for _ in 0..3 {
            broker.sync_herdr_registry().await;
        }
        assert!(broker.state.agents.contains_key("w1:p7"));
    }

    #[tokio::test]
    async fn startup_syncs_a_swapped_herdr_identity_before_serving() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut broker = broker(&dir, vec![pane_info("w1:p7", Some("grok"))]);
            broker.sync_herdr_registry().await;
            assert_eq!(broker.state.agents["w1:p7"].name, "grok");
        }
        // 再起動: journal は grok を復元するが、pane の占有者は既に gemini。
        let mut broker = broker(&dir, vec![]);
        assert_eq!(
            broker.state.agents["w1:p7"].name, "grok",
            "journal 復元は旧 identity"
        );
        set_herdr_panes(&broker, vec![pane_info("w1:p7", Some("gemini"))]);
        broker.startup().await.unwrap();
        assert_eq!(
            broker.state.agents["w1:p7"].name, "gemini",
            "受付開始前に snapshot と同期し、最初の tick までの誤配窓を作らない"
        );
    }

    #[tokio::test]
    async fn a_herdr_sender_receives_the_failure_notice() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p6", Some("grok")),
                pane_info("w1:p7", Some("gemini")),
            ],
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
            vec![pane_info("w1:p6", Some("grok")), pane_info("w1:p7", None)],
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
    async fn a_takeover_register_failure_defers_the_new_identity_to_the_next_sync() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p7", Some("grok")),
            ],
        );
        broker.sync_herdr_registry().await;

        // Remove (1回目の append) は通り、直後の Register だけが落ちる。
        broker.journal.fail_appends_after(1);
        set_herdr_panes(&broker, vec![pane_info("w1:p7", Some("gemini"))]);
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
        set_herdr_panes(&broker, vec![pane_info("w1:p7", Some("bad/name"))]);
        broker.sync_herdr_registry().await;
        assert!(
            !broker.state.agents.contains_key("w1:p7"),
            "宛先文法に載らない native 名は登録しない (CLI の register と同じ検証)"
        );
    }

    /// 受け入れ T1: 同一 workspace・同一 runtime (claude) の 2 pane が tab 名
    /// (fable / opus) で別 identity として登録され、bare 名・`<workspace>/<タブ名>`・
    /// pane id 直指定のどの宛先でも正しい pane へ届く。純数字 label は custom 名
    /// なしとして runtime 名へ fallback する。
    #[tokio::test]
    async fn tab_labels_identify_same_runtime_panes_and_resolve_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(
            &dir,
            vec![
                tabbed_pane("w1:p1", "claude", "w1:t1", Some("fable")),
                tabbed_pane("w1:p2", "claude", "w1:t2", Some("opus")),
                tabbed_pane("w1:p3", "claude", "w1:t3", Some("4")),
                tabbed_pane("w1:p4", "codex", "w1:t4", None),
            ],
        );
        broker.sync_herdr_registry().await;
        assert_eq!(broker.state.agents["w1:p1"].name, "fable");
        assert_eq!(broker.state.agents["w1:p1"].runtime, "claude");
        assert_eq!(broker.state.agents["w1:p2"].name, "opus");
        assert_eq!(broker.state.agents["w1:p2"].runtime, "claude");
        assert_eq!(
            broker.state.agents["w1:p3"].name, "claude",
            "純数字 label は custom 名なしなので runtime 名で登録する"
        );
        assert_eq!(
            broker.state.agents["w1:p4"].name, "codex",
            "tab label の無い pane は従来どおり runtime 名"
        );

        // peers_json の name も tab 名になる。
        let peers = json(
            &broker
                .handle(request("list-peers", Some("w1:p4"), &[]))
                .await,
        );
        let names: Vec<&str> = peers["peers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|peer| peer["name"].as_str())
            .collect();
        assert!(
            names.contains(&"fable") && names.contains(&"opus"),
            "{names:?}"
        );

        // bare 名は同一 runtime の同居があっても tab 名で一意に解決する。
        let sent = json(&broker.handle(send_request("w1:p4", "fable", "hi")).await);
        assert_eq!(sent["to"], "w1:p1", "{sent}");
        assert_eq!(sent["name"], "fable");
        // `<workspace>/<タブ名>` (workspace label は "test")。
        let sent = json(
            &broker
                .handle(send_request("w1:p4", "test/opus", "hi"))
                .await,
        );
        assert_eq!(sent["to"], "w1:p2", "{sent}");
        assert_eq!(sent["name"], "opus");
        // pane id 直指定。
        let sent = json(&broker.handle(send_request("w1:p4", "w1:p3", "hi")).await);
        assert_eq!(sent["name"], "claude", "{sent}");
    }

    /// 受け入れ T2: 同一 workspace で tab 名が重複したら bare / scoped とも
    /// 自動近接選択せず曖昧エラーにし、候補の pane id を案内する。pane id
    /// 直指定なら届く。tab.list 相当の取得に失敗した tick は snapshot を
    /// 適用せず、既存の identity を保つ。
    #[tokio::test]
    async fn duplicate_tab_names_error_with_pane_ids_and_failed_snapshots_freeze() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(
            &dir,
            vec![
                tabbed_pane("w1:p1", "claude", "w1:t1", Some("fable")),
                tabbed_pane("w1:p2", "claude", "w1:t2", Some("fable")),
                tabbed_pane("w1:p3", "codex", "w1:t3", None),
            ],
        );
        broker.sync_herdr_registry().await;

        for addr in ["fable", "test/fable"] {
            let error = broker.resolve(addr, Some("w1:p3")).await.unwrap_err();
            assert!(
                error.stderr.contains("候補が複数"),
                "{addr}: {}",
                error.stderr
            );
            assert!(error.stderr.contains("pane id"), "{addr}: {}", error.stderr);
            assert!(
                error.stderr.contains("w1:p1") && error.stderr.contains("w1:p2"),
                "{addr}: 候補の pane id を案内する: {}",
                error.stderr
            );
        }

        // pane id 直指定は曖昧にならず配送できる。
        let sent = json(
            &broker
                .handle(send_request("w1:p3", "w1:p2", "direct"))
                .await,
        );
        assert_eq!(sent["to"], "w1:p2", "{sent}");
        assert_eq!(sent["name"], "fable");

        // 取得失敗 tick は snapshot を適用しない (登録の追加・交代・回収なし)。
        *broker.backend.herdr().scripted_fail.lock().unwrap() = true;
        set_herdr_panes(&broker, vec![]);
        broker.sync_herdr_registry().await;
        assert_eq!(broker.state.agents.len(), 3, "既存 identity を保つ");
        assert_eq!(broker.state.agents["w1:p1"].name, "fable");
        assert_eq!(broker.state.agents["w1:p2"].name, "fable");
    }

    /// 受け入れ T3: 宛先名が tab 名になっても runtime 結合は保たれる —
    /// skill 記法は runtime (claude) 側で解決されて送信拒否にならず、手動
    /// register は導出名 (tab 名) だけを受理し、journal の Register は
    /// {name, runtime} で書かれる。
    #[tokio::test]
    async fn tab_named_agents_keep_their_runtime_binding() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(
            &dir,
            vec![tabbed_pane("w1:p1", "claude", "w1:t1", Some("fable"))],
        );
        broker.config.allowed_sources.insert("mobile".into());
        // config.rs の既定 mapping と同じ: claude は slash 記法。
        broker
            .config
            .skill_syntax
            .insert("claude".into(), crate::config::SkillSyntax::Slash);
        broker.sync_herdr_registry().await;

        let accepted = broker
            .web_letter(
                "mobile".into(),
                "fable".into(),
                "please deliver".into(),
                Some("deliver".into()),
            )
            .await
            .unwrap();
        assert_eq!(accepted["name"], "fable", "{accepted}");
        let delivered = bells(&broker);
        assert!(
            delivered
                .last()
                .is_some_and(|(pane, text)| pane == "w1:p1" && text.starts_with("/deliver ")),
            "skill 記法は runtime 側で解決される: {delivered:?}"
        );

        // 手動 register は導出名 (tab 名) だけを受理する。
        let ok = broker
            .handle(request("register", Some("w1:p1"), &["fable"]))
            .await;
        assert_eq!(ok.code, 0, "{}", ok.stderr);
        let refused = broker
            .handle(request("register", Some("w1:p1"), &["claude"]))
            .await;
        assert_eq!(refused.code, 1, "{refused:?}");
        assert!(refused.stderr.contains("一致しない"), "{}", refused.stderr);
        assert_eq!(broker.state.agents["w1:p1"].name, "fable");
        assert_eq!(broker.state.agents["w1:p1"].runtime, "claude");

        // journal の Register は {name, runtime} で書かれる。
        let journal = std::fs::read_to_string(&broker.config.journal).unwrap();
        assert!(
            journal.lines().any(|line| {
                line.contains("\"type\":\"register\"")
                    && line.contains("\"name\":\"fable\"")
                    && line.contains("\"runtime\":\"claude\"")
            }),
            "{journal}"
        );
    }

    /// identity takeover 回帰: タブ名 fable のまま runtime だけ claude → codex に
    /// 交代した pane へ、旧 runtime 時代の message の `reply_to` を返さない。
    /// 捕捉した (name, runtime) は journal replay (restart) 後も保たれる —
    /// identity が変わっていなければ `reply_to` は返り続ける。
    #[tokio::test]
    async fn a_runtime_swap_behind_the_same_tab_name_never_offers_a_reply_target() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let mut broker = broker(
                &dir,
                vec![
                    tabbed_pane("w1:p1", "claude", "w1:t1", Some("fable")),
                    tabbed_pane("w1:p2", "claude", "w1:t2", Some("opus")),
                ],
            );
            broker.sync_herdr_registry().await;
            json(
                &broker
                    .handle(send_request("w1:p1", "opus", "question"))
                    .await,
            )["id"]
                .as_u64()
                .unwrap()
        };

        // 再起動 (journal replay): identity 不変なら reply_to は保たれる。
        // 捕捉 runtime が durable でないとここで既に返信先を失う。
        let mut broker = broker(
            &dir,
            vec![
                tabbed_pane("w1:p1", "claude", "w1:t1", Some("fable")),
                tabbed_pane("w1:p2", "claude", "w1:t2", Some("opus")),
            ],
        );
        broker.startup().await.unwrap();
        let read = json(
            &broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(read["from"], "fable");
        assert_eq!(read["reply_to"], "w1:p1", "identity 不変なら返信できる");

        // 同じ pane がタブ名 fable のまま runtime だけ codex に交代 → sync。
        set_herdr_panes(
            &broker,
            vec![
                tabbed_pane("w1:p1", "codex", "w1:t1", Some("fable")),
                tabbed_pane("w1:p2", "claude", "w1:t2", Some("opus")),
            ],
        );
        broker.sync_herdr_registry().await;
        assert_eq!(
            broker.state.agents["w1:p1"].runtime, "codex",
            "takeover 済み"
        );
        let read = json(
            &broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(read["from"], "fable", "捕捉した表示名は変わらない");
        assert!(
            read["reply_to"].is_null(),
            "runtime だけ交代した pane へ返信させてはならない: {read}"
        );
    }

    /// 重複タブ名は同一タブの近接でも自動選択しない: sender と同じタブに片方が
    /// 居ても bare 名は曖昧エラーになり、候補の pane id を案内する。
    #[tokio::test]
    async fn duplicate_tab_names_stay_ambiguous_even_from_an_adjacent_tab() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(
            &dir,
            vec![
                tabbed_pane("w1:p1", "claude", "w1:t1", Some("fable")),
                tabbed_pane("w1:p2", "claude", "w1:t2", Some("fable")),
                // sender は p1 と同一タブに居る。
                tabbed_pane("w1:p3", "codex", "w1:t1", None),
            ],
        );
        broker.sync_herdr_registry().await;
        let error = broker.resolve("fable", Some("w1:p3")).await.unwrap_err();
        assert!(error.stderr.contains("候補が複数"), "{}", error.stderr);
        assert!(
            error.stderr.contains("w1:p1") && error.stderr.contains("w1:p2"),
            "候補の pane id を案内する: {}",
            error.stderr
        );
    }

    /// sender 自身が重複名の一方のとき、self 除外後の候補は1件なので bare 名は
    /// 他方へ一意配送される (この挙動を契約として固定する)。
    #[tokio::test]
    async fn a_duplicate_named_sender_resolves_bare_to_the_other_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(
            &dir,
            vec![
                tabbed_pane("w1:p1", "claude", "w1:t1", Some("fable")),
                tabbed_pane("w1:p2", "claude", "w1:t2", Some("fable")),
            ],
        );
        broker.sync_herdr_registry().await;
        let resolved = broker.resolve("fable", Some("w1:p1")).await.unwrap();
        assert_eq!(resolved.0, "w1:p2");
        let sent = json(&broker.handle(send_request("w1:p1", "fable", "hi")).await);
        assert_eq!(sent["to"], "w1:p2", "{sent}");
    }

    /// 呼び鈴・brief・journal の from/to がタブ名 (fable / opus) に追随し、
    /// journal replay (restart) 後も保たれる。
    #[tokio::test]
    async fn bells_briefs_and_journal_carry_tab_names_for_custom_named_agents() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let mut broker = broker(
                &dir,
                vec![
                    tabbed_pane("w1:p1", "claude", "w1:t1", Some("fable")),
                    tabbed_pane("w1:p2", "claude", "w1:t2", Some("opus")),
                ],
            );
            broker.sync_herdr_registry().await;
            let sent = json(
                &broker
                    .handle(send_request("w1:p1", "opus", "question"))
                    .await,
            );
            let id = sent["id"].as_u64().unwrap();
            assert_eq!(sent["name"], "opus");
            // 呼び鈴 (agent.prompt で配送されるテキスト) の sender 表記は
            // workspace label 付きのタブ名 (canonical full label)。
            let delivered = bells(&broker);
            assert!(
                delivered.last().is_some_and(|(pane, bell)| {
                    pane == "w1:p2" && bell.contains("test/fable から連絡が届きました")
                }),
                "{delivered:?}"
            );
            // Message の sender_name / target_name はタブ名。
            let stored = broker.state.message(id).unwrap();
            assert_eq!(stored.message.sender_label(), "fable");
            assert_eq!(stored.message.target_name, "opus");
            // brief テンプレートの from/to header。
            assert!(
                stored.message.brief.contains("- from: fable"),
                "{}",
                stored.message.brief
            );
            assert!(
                stored.message.brief.contains("- to: opus"),
                "{}",
                stored.message.brief
            );
            // journal の Enqueue も同じ from/to を持つ。
            let journal = std::fs::read_to_string(&broker.config.journal).unwrap();
            assert!(
                journal.lines().any(|line| {
                    line.contains("\"type\":\"enqueue\"")
                        && line.contains("\"sender_name\":\"fable\"")
                        && line.contains("\"target_name\":\"opus\"")
                }),
                "{journal}"
            );
            // read_message の from もタブ名。
            let read = json(
                &broker
                    .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                    .await,
            );
            assert_eq!(read["from"], "fable");
            id
        };
        // journal replay (restart) 後も保たれる。
        let mut restarted = broker(
            &dir,
            vec![
                tabbed_pane("w1:p1", "claude", "w1:t1", Some("fable")),
                tabbed_pane("w1:p2", "claude", "w1:t2", Some("opus")),
            ],
        );
        restarted.startup().await.unwrap();
        let read = json(
            &restarted
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(read["from"], "fable");
        assert_eq!(read["reply_to"], "w1:p1");
        let stored = restarted.state.message(id).unwrap();
        assert!(
            stored.message.brief.contains("- from: fable"),
            "{}",
            stored.message.brief
        );
        assert_eq!(stored.message.target_name, "opus");
    }

    /// 呼び鈴の差出人は canonical full label (`<workspace>/<name>`)。同名 (`intake`)
    /// の agent が別 workspace に居ても呼び鈴で区別できる。`read_message` の
    /// `from` は互換のため bare 名のままで、`from_full` が workspace 付きを返す。
    #[tokio::test]
    async fn bells_name_the_sender_with_its_workspace_qualified_label() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(
            &dir,
            vec![
                workspaced_pane("w1:p1", "task", "w1:t1", "intake"),
                workspaced_pane("w2:p1", "knowledge", "w2:t1", "intake"),
                workspaced_pane("w3:p1", "ops", "w3:t1", "boss"),
            ],
        );
        broker.sync_herdr_registry().await;

        for (sender, expected_full) in [("w1:p1", "task/intake"), ("w2:p1", "knowledge/intake")] {
            let sent = json(
                &broker
                    .handle(send_request(sender, "w3:p1", "question"))
                    .await,
            );
            let id = sent["id"].as_u64().unwrap();
            let delivered = bells(&broker);
            assert!(
                delivered.last().is_some_and(|(pane, bell)| {
                    pane == "w3:p1"
                        && bell.contains(&format!("{expected_full} から連絡が届きました"))
                }),
                "{delivered:?}"
            );
            let read = json(
                &broker
                    .handle(request("read-message", Some("w3:p1"), &[&id.to_string()]))
                    .await,
            );
            assert_eq!(read["version"], 1, "additive な追加で版は上げない");
            assert_eq!(read["from"], "intake", "既存の from は bare 名のまま");
            assert_eq!(read["from_full"], expected_full);
        }
    }

    /// 送信受理時点で捕捉した workspace は journal に載り、送信者が消えた後の
    /// 再起動でも同じ identity を返す (live peer の再検索に依存しない)。
    #[tokio::test]
    async fn a_captured_sender_workspace_survives_restart_and_the_senders_exit() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let mut broker = broker(
                &dir,
                vec![
                    workspaced_pane("w1:p1", "task", "w1:t1", "intake"),
                    workspaced_pane("w3:p1", "ops", "w3:t1", "boss"),
                ],
            );
            broker.sync_herdr_registry().await;
            let sent = json(
                &broker
                    .handle(send_request("w1:p1", "w3:p1", "question"))
                    .await,
            );
            let id = sent["id"].as_u64().unwrap();
            let journal = std::fs::read_to_string(&broker.config.journal).unwrap();
            assert!(
                journal.lines().any(|line| {
                    line.contains("\"type\":\"enqueue\"")
                        && line.contains("\"sender_workspace\":\"task\"")
                }),
                "{journal}"
            );
            id
        };

        // 送信者 pane が消えた状態で daemon を再起動する。
        let mut restarted = broker(&dir, vec![workspaced_pane("w3:p1", "ops", "w3:t1", "boss")]);
        restarted.startup().await.unwrap();
        let bell = restarted.state.message(id).unwrap().message.bell.clone();
        assert!(bell.contains("task/intake から連絡が届きました"), "{bell}");
        let read = json(
            &restarted
                .handle(request("read-message", Some("w3:p1"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(read["from"], "intake");
        assert_eq!(read["from_full"], "task/intake");
        assert_eq!(
            read["reply_to"],
            serde_json::Value::Null,
            "退出した送信者へは返信先を出さない"
        );
    }

    /// `sender_workspace` を持たない旧 journal の `Enqueue` は従来どおり読め、
    /// `from` は従来値、`from_full` は bare fallback になる。cwd や現在の
    /// peer 一覧から workspace を推測しない。
    #[tokio::test]
    async fn a_legacy_enqueue_without_a_sender_workspace_falls_back_to_the_bare_label() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("queue.journal"),
            concat!(
                r#"{"type":"register","pane":"w1:p1","name":"fable","runtime":"claude","state":"idle"}"#,
                "\n",
                r#"{"type":"register","pane":"w1:p2","name":"opus","runtime":"claude","state":"idle"}"#,
                "\n",
                r##"{"type":"enqueue","pane":"w1:p2","message":{"id":7,"sender":"w1:p1","sender_name":"fable","sender_runtime":"claude","brief":"# body\n","bell":"[agent-talk] fable から依頼が届きました。read_message 7 で本文を確認してから対応してください。","target_name":"opus","target_runtime":"claude"}}"##,
                "\n",
                r#"{"type":"complete","pane":"w1:p2","id":7}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut broker = broker(
            &dir,
            vec![
                workspaced_pane("w1:p1", "task", "w1:t1", "fable"),
                workspaced_pane("w1:p2", "task", "w1:t2", "opus"),
            ],
        );
        broker.startup().await.unwrap();
        let stored = broker.state.message(7).unwrap();
        assert_eq!(stored.message.sender_workspace, None);
        assert_eq!(stored.message.sender_full_label(), "fable");
        assert!(
            stored.message.bell.contains("fable から依頼が届きました")
                && !stored.message.bell.contains("task/fable"),
            "保存済みの呼び鈴は書き換えない: {}",
            stored.message.bell
        );
        let read = json(
            &broker
                .handle(request("read-message", Some("w1:p2"), &["7"]))
                .await,
        );
        assert_eq!(read["from"], "fable");
        assert_eq!(read["from_full"], "fable", "workspace を推測しない");
    }

    /// 登録済み送信者なのに、`send` が引いた snapshot が登録 identity を裏付けない
    /// 間の送信は受理しない。受理してしまうと、呼び鈴が canonical full label に
    /// ならず `sender_workspace` も持たない record が恒久的に残る。journal・state・
    /// 配達のどれも変えずに拒否し、同期後の再送を求める。
    ///
    /// `handle` 経由ではなく `send` を直接呼ぶ: `handle` は送信前に registry を
    /// 同期し直すので、この race は **その同期が使った snapshot と `send` 自身が
    /// 引く snapshot が別物** であることから生じる。snapshot を差し替えてから
    /// `send` を呼ぶのが、その2枚目が食い違った状態そのものを再現する。
    #[tokio::test]
    async fn a_send_whose_sender_pane_no_longer_backs_its_registration_is_refused() {
        for drifted in [
            // tab rename (name が食い違う)。
            workspaced_pane("w1:p1", "task", "w1:t1", "intake-renamed"),
            // runtime 交代 (name は同じでも runtime が食い違う)。
            HerdrPane {
                agent: Some("codex".into()),
                ..workspaced_pane("w1:p1", "task", "w1:t1", "intake")
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut broker = broker(
                &dir,
                vec![
                    workspaced_pane("w1:p1", "task", "w1:t1", "intake"),
                    workspaced_pane("w3:p1", "ops", "w3:t1", "boss"),
                ],
            );
            broker.sync_herdr_registry().await;
            // 再 sync せずに snapshot だけ差し替える (tick の合間の変化)。
            set_herdr_panes(
                &broker,
                vec![drifted, workspaced_pane("w3:p1", "ops", "w3:t1", "boss")],
            );

            let response = broker
                .send(send_request("w1:p1", "w3:p1", "question"), SendReport::Json)
                .await
                .unwrap();
            assert_ne!(response.code, 0, "{response:?}");
            assert!(
                response.stderr.contains("再送"),
                "同期後の再送を促す: {}",
                response.stderr
            );
            assert!(broker.state.messages.is_empty(), "message を採番しない");
            let journal = std::fs::read_to_string(&broker.config.journal).unwrap();
            assert!(
                !journal.contains("\"type\":\"enqueue\""),
                "journal を変えない: {journal}"
            );
            assert!(bells(&broker).is_empty(), "呼び鈴を配達しない");
        }
    }

    #[tokio::test]
    async fn herdr_registrations_refuse_manual_unregister() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = broker(&dir, vec![]);
        set_herdr_panes(&broker, vec![pane_info("w1:p7", Some("grok"))]);
        broker.sync_herdr_registry().await;
        let refused = broker
            .handle(request("unregister", Some("w1:p7"), &[]))
            .await;
        assert_eq!(refused.code, 1);
        assert!(refused.stderr.contains("herdr"), "{}", refused.stderr);
        assert!(broker.state.agents.contains_key("w1:p7"));
    }

    /// herdr が `done` (完了出力の未閲覧バッジ) と報告する pane にも、
    /// 直配・queue drain・受領催促のすべてが user の閲覧を待たずに届く。
    /// steer-safety の拒否対象は blocked / unknown。催促は working にも撃たない。
    #[tokio::test(start_paused = true)]
    async fn a_done_pane_receives_mail_drain_and_reminders_without_being_viewed() {
        let dir = tempfile::tempdir().unwrap();
        let done_claude = HerdrPane {
            status: AgentStatus::Done,
            ..pane_info("w1:p2", Some("claude"))
        };
        let mut broker = broker(
            &dir,
            vec![pane_info("w1:p1", Some("codex")), done_claude.clone()],
        );
        broker.sync_herdr_registry().await;

        // 直配: done pane へは queue 行きにならず即配達される。
        let first = json(
            &broker
                .handle(send_request("w1:p1", "claude", "first"))
                .await,
        );
        assert_eq!(first["path"], "sent", "done は配達可能");

        // drain: queue が残った done pane も health tick が先頭から流す。
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                HerdrPane {
                    status: AgentStatus::Blocked,
                    ..pane_info("w1:p2", Some("claude"))
                },
            ],
        );
        let second = json(
            &broker
                .handle(send_request("w1:p1", "claude", "second"))
                .await,
        );
        assert_eq!(second["path"], "queued");
        let second_id = second["id"].as_u64().unwrap();
        set_herdr_panes(
            &broker,
            vec![pane_info("w1:p1", Some("codex")), done_claude],
        );
        let before = bells(&broker).len();
        broker.drain_queued().await;
        let drained: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(drained.len(), 1, "{drained:?}");
        assert!(
            drained[0].1.contains(&format!("read_message {second_id}")),
            "{drained:?}"
        );

        // 催促: 配達済み・未 ack の message への nag も done pane に届く。
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        let before = bells(&broker).len();
        broker.nag_unacked().await;
        let nags: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(nags.len(), 1, "done pane にも催促が届く: {nags:?}");
        assert!(nags[0].1.contains("受領催促"), "{nags:?}");

        // 催促は working には撃たない (初回配達とは predicate が違う)。
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                HerdrPane {
                    status: AgentStatus::Working,
                    ..pane_info("w1:p2", Some("claude"))
                },
            ],
        );
        tokio::time::advance(std::time::Duration::from_mins(6)).await;
        let before = bells(&broker).len();
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before, "working には催促を撃たない");
    }

    #[tokio::test]
    async fn a_new_send_never_overtakes_the_queue_and_the_tick_drains_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                HerdrPane {
                    status: AgentStatus::Blocked,
                    ..pane_info("w1:p2", Some("claude"))
                },
            ],
        );
        // blocked 中はsend1/send2ともqueueに入り、FIFOを作る。
        let first = json(
            &broker
                .handle(send_request("w1:p1", "claude", "first"))
                .await,
        );
        assert_eq!(first["path"], "queued");
        let first_id = first["id"].as_u64().unwrap();
        let second = json(
            &broker
                .handle(send_request("w1:p1", "claude", "second"))
                .await,
        );
        assert_eq!(second["path"], "queued");
        let second_id = second["id"].as_u64().unwrap();

        // queue が残っている間、新規sendは追い越さない。
        let third = json(
            &broker
                .handle(send_request("w1:p1", "claude", "third"))
                .await,
        );
        assert_eq!(
            third["path"], "queued",
            "queue が残る pane への新規 send が FIFO を追い越してはならない"
        );
        let third_id = third["id"].as_u64().unwrap();

        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                HerdrPane {
                    status: AgentStatus::Working,
                    ..pane_info("w1:p2", Some("claude"))
                },
            ],
        );
        // health tick の drain は working にも先頭から1通ずつ配達する。
        let before = bells(&broker).len();
        broker.drain_queued().await;
        let after: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(after.len(), 1, "1 tick で流すのは先頭の1件だけ");
        assert!(
            after[0].1.contains(&format!("read_message {first_id}")),
            "先頭 (send1) が先に配達される: {:?}",
            after[0]
        );
        assert!(broker.state.is_queued("w1:p2", second_id));
        assert!(broker.state.is_queued("w1:p2", third_id));

        broker.drain_queued().await;
        broker.drain_queued().await;
        let drained: Vec<_> = bells(&broker)[before..].to_vec();
        assert_eq!(drained.len(), 3);
        assert!(
            drained[1].1.contains(&format!("read_message {second_id}"))
                && drained[2].1.contains(&format!("read_message {third_id}")),
            "{drained:?}"
        );
        assert_eq!(broker.state.queue_len("w1:p2"), 0, "queue は空になる");
    }

    #[test]
    fn working_stays_busy_in_the_display_state() {
        assert_eq!(super::display_state(AgentStatus::Idle), AgentState::Idle);
        assert_eq!(super::display_state(AgentStatus::Done), AgentState::Idle);
        assert_eq!(super::display_state(AgentStatus::Working), AgentState::Busy);
        assert_eq!(super::display_state(AgentStatus::Blocked), AgentState::Busy);
        assert_eq!(super::display_state(AgentStatus::Unknown), AgentState::Busy);
    }

    #[tokio::test(start_paused = true)]
    async fn a_working_pane_receives_mail_immediately_but_is_not_nagged() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        set_herdr_panes(
            &broker,
            vec![
                pane_info("w1:p1", Some("codex")),
                HerdrPane {
                    status: AgentStatus::Working,
                    ..pane_info("w1:p2", Some("claude"))
                },
            ],
        );

        let sent = json(
            &broker
                .handle(send_request("w1:p1", "claude", "while working"))
                .await,
        );
        assert_eq!(sent["path"], "sent", "working は初回配達できる");
        let id = sent["id"].as_u64().unwrap();
        let delivered = bells(&broker);
        assert_eq!(delivered.len(), 1, "{delivered:?}");
        assert!(
            delivered[0].1.contains(&format!("read_message {id}")),
            "{delivered:?}"
        );

        let peers = json(
            &broker
                .handle(request("list-peers", Some("w1:p1"), &[]))
                .await,
        );
        let claude = peers["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|peer| peer["pane"] == "w1:p2")
            .unwrap();
        assert_eq!(claude["state"], "busy", "配達可能でも working は busy 表示");

        let who = broker.handle(request("who", Some("w1:p1"), &[])).await;
        assert!(
            who.stdout.contains("busy/working"),
            "who も working を busy のまま出す: {}",
            who.stdout
        );

        tokio::time::advance(std::time::Duration::from_mins(6)).await;
        let before = bells(&broker).len();
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before, "working には催促を撃たない");
    }

    #[tokio::test(start_paused = true)]
    async fn a_successful_read_is_receipt_and_the_body_stays() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let id = json(
            &broker
                .handle(send_request("w1:p1", "claude", "keep this body"))
                .await,
        )["id"]
            .as_u64()
            .unwrap();

        let first = json(
            &broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert!(
            first["body"].as_str().unwrap().contains("keep this body"),
            "{}",
            first["body"]
        );

        let recipient = json(
            &broker
                .handle(request("list-peers", Some("w1:p2"), &[]))
                .await,
        );
        assert!(
            recipient["pending_to_me"].as_array().unwrap().is_empty(),
            "read で自分宛 pending が消える: {recipient}"
        );
        let sender = json(
            &broker
                .handle(request("list-peers", Some("w1:p1"), &[]))
                .await,
        );
        let from_me = sender["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|peer| peer["pane"] == "w1:p2")
            .unwrap();
        assert!(
            from_me["pending_from_me"].as_array().unwrap().is_empty(),
            "read で送り手 pending も消える: {from_me}"
        );

        let again = json(
            &broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert!(
            again["body"].as_str().unwrap().contains("keep this body"),
            "本文は残る: {}",
            again["body"]
        );

        tokio::time::advance(std::time::Duration::from_mins(6)).await;
        let before = bells(&broker).len();
        broker.nag_unacked().await;
        assert_eq!(bells(&broker).len(), before, "read 後は催促しない");
    }

    #[tokio::test]
    async fn ack_is_a_mutation_free_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        let id = json(
            &broker
                .handle(send_request("w1:p1", "claude", "unread"))
                .await,
        )["id"]
            .as_u64()
            .unwrap();
        let journal_before = std::fs::read(&broker.config.journal).unwrap();

        let early = json(
            &broker
                .handle(request("ack-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(early["outcome"], "acked");
        assert_eq!(
            std::fs::read(&broker.config.journal).unwrap(),
            journal_before,
            "read 前の ack は journal を書かない"
        );
        assert_eq!(broker.state.pending_to_me("w1:p2"), vec![id]);

        let read = json(
            &broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert!(read["body"].as_str().unwrap().contains("unread"));
        let journal_after_read = std::fs::read(&broker.config.journal).unwrap();

        let late = json(
            &broker
                .handle(request("ack-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert_eq!(late["outcome"], "acked");
        assert_eq!(
            std::fs::read(&broker.config.journal).unwrap(),
            journal_after_read,
            "read 後の ack も journal を書かない"
        );
        let reread = json(
            &broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert!(reread["body"].as_str().unwrap().contains("unread"));

        let missing = json(
            &broker
                .handle(request("ack-message", Some("w1:p2"), &["99"]))
                .await,
        );
        assert_eq!(missing["outcome"], "no_pending_message");
    }

    #[tokio::test(start_paused = true)]
    async fn a_seen_receipt_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let mut broker = registered_pair(&dir).await;
            let id = json(
                &broker
                    .handle(send_request("w1:p1", "claude", "survive restart"))
                    .await,
            )["id"]
                .as_u64()
                .unwrap();
            let read = broker
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await;
            assert_eq!(read.code, 0, "{}", read.stderr);
            id
        };
        let mut restarted = broker(
            &dir,
            vec![
                pane_info("w1:p1", Some("codex")),
                pane_info("w1:p2", Some("claude")),
            ],
        );
        assert!(
            restarted.state.pending_to_me("w1:p2").is_empty(),
            "Seen は restart 後も pending に戻らない"
        );
        let reread = json(
            &restarted
                .handle(request("read-message", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert!(
            reread["body"].as_str().unwrap().contains("survive restart"),
            "{}",
            reread["body"]
        );
        tokio::time::advance(std::time::Duration::from_mins(6)).await;
        let before = bells(&restarted).len();
        restarted.nag_unacked().await;
        assert_eq!(bells(&restarted).len(), before, "restart 後も催促しない");
    }

    #[tokio::test]
    async fn legacy_wire_names_still_reach_the_same_operations() {
        let dir = tempfile::tempdir().unwrap();
        let mut broker = registered_pair(&dir).await;
        // 旧 adapter が送る *-v1 名は canonical と同じ operation へ正規化される。
        let mut legacy_send = request("send-message-v1", Some("w1:p1"), &["claude"]);
        legacy_send.stdin = "legacy body".into();
        legacy_send.send_options = Some(SendOptions::default());
        let sent = json(&broker.handle(legacy_send).await);
        assert_eq!(sent["path"], "sent");
        let id = sent["id"].as_u64().unwrap();

        let read = json(
            &broker
                .handle(request("read-v1", Some("w1:p2"), &[&id.to_string()]))
                .await,
        );
        assert!(read["body"].as_str().unwrap().contains("legacy body"));

        let peers = json(&broker.handle(request("peers-v1", Some("w1:p2"), &[])).await);
        assert!(
            peers["pending_to_me"].as_array().unwrap().is_empty(),
            "read-v1 で受領になる: {peers}"
        );

        let acked = json(
            &broker
                .handle(request("ack-v1", Some("w1:p2"), &[&id.to_string()]))
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
                json(&broker.handle(send_request("w1:p1", "claude", &big)).await)["id"]
                    .as_u64()
                    .unwrap(),
            );
        }
        set_herdr_panes(&broker, vec![pane_info("w1:p1", Some("codex"))]);
        assert!(
            broker
                .remove_agent("w1:p2", "宛先エージェントが退出した")
                .await
        );

        let notice = broker
            .state
            .messages
            .values()
            .find(|stored| stored.target_pane == "w1:p1" && !stored.acked)
            .expect("collapsed notice");
        let brief = &notice.message.brief;
        assert!(
            brief.len() <= MAX_BODY_BYTES + 4096,
            "集約通知は単発肥大しない: {} bytes",
            brief.len()
        );
        // 予算内の1通目は全文、超過する2通目は ID を残して本文を省略する。
        assert!(brief.contains(&format!("## 元のメッセージ #{}", ids[0])));
        assert!(brief.contains(&big));
        assert!(brief.contains(&format!("## 元のメッセージ #{}", ids[1])));
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
        // pane id は opaque — route は形式で落とさず、登録の有無は handler が
        // registry で判定する (未登録なら 404)。
        assert_eq!(
            classify_http(&Method::GET, "/api/agents/%251/screen"),
            HttpRoute::Screen("%1".into())
        );
        assert_eq!(
            classify_http(&Method::GET, "/api/agents/w2%3Ap4/screen"),
            HttpRoute::Screen("w2:p4".into())
        );
        assert_eq!(
            classify_http(&Method::GET, "/api/agents/w2%3Ap4/skills"),
            HttpRoute::Skills("w2:p4".into())
        );
        assert_eq!(
            super::installed_skills_for_runtime(
                std::path::Path::new("/tmp/no-such-home"),
                "claude"
            ),
            Vec::<String>::new()
        );
        // `/` を含む opaque な id も percent encode で screen route に載る。
        assert_eq!(
            classify_http(&Method::GET, "/api/agents/pane%2F%CE%B1%3Anext%3F/screen"),
            HttpRoute::Screen("pane/α:next?".into())
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
        ] {
            assert_eq!(classify_http(&Method::GET, path), HttpRoute::BadRequest);
        }
        // decode に成功した opaque な pane 文字列は Screen へ通り、未登録なら
        // handler が 404 を返す。mailbox は従来どおり token 検査で NotFound
        // (decode 後の `/` も token 検査が拒否する)。
        assert_eq!(
            classify_http(&Method::GET, "/api/agents/%252F/screen"),
            HttpRoute::Screen("%2F".into())
        );
        for path in ["/api/mailbox/Bad", "/api/mailbox/bad%2Fname"] {
            assert_eq!(classify_http(&Method::GET, path), HttpRoute::NotFound);
        }
    }

    fn assert_who_http_headers(response: &hyper::Response<http_body_util::Full<bytes::Bytes>>) {
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .map(hyper::header::HeaderValue::as_bytes),
            Some(b"no-store".as_slice())
        );
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .expect("who responses set Content-Type")
            .to_str()
            .expect("Content-Type is visible ASCII");
        assert!(
            content_type.starts_with("application/json"),
            "{content_type}"
        );
    }

    #[tokio::test]
    async fn who_http_sets_no_store_json_on_200_and_broker_unavailable() {
        let (closed_tx, closed_rx) = tokio::sync::mpsc::channel(1);
        drop(closed_rx);
        let unavailable = request_web_agents(&closed_tx).await;
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_who_http_headers(&unavailable);

        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let respond = async {
            match rx.recv().await {
                Some(Event::Http(HttpEvent::Who { reply })) => {
                    let _ = reply.send(Ok(Vec::new()));
                }
                _ => panic!("expected HttpEvent::Who"),
            }
        };
        let (ok, ()) = tokio::join!(request_web_agents(&tx), respond);
        assert_eq!(ok.status(), StatusCode::OK);
        assert_who_http_headers(&ok);
    }

    #[test]
    fn strict_percent_decoder_rejects_malformed_and_unsafe_segments() {
        assert_eq!(decode_path_segment("%251"), Some("%1".into()));
        assert_eq!(decode_path_segment("mobile"), Some("mobile".into()));
        // decode 後の `/` は opaque な pane id の data として通す
        // (route 構造は raw path 側で確定済み)。raw の `/` と NUL は拒否。
        assert_eq!(decode_path_segment("%2F"), Some("/".into()));
        assert_eq!(decode_path_segment("%"), None);
        assert_eq!(decode_path_segment("%GG"), None);
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
            pane_id: "w1:p1".into(),
            session: "work".into(),
            location: "work:0.1".into(),
            cwd: "/tmp/project with \"quotes\"".into(),
            backend: "herdr",
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
