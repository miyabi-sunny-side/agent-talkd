//! Stateless HTTP console. Herdr owns destinations; each CLI owns its transcript.
use std::{collections::BTreeMap, convert::Infallible, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Method, Request, Response, StatusCode, body::Incoming, header, server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex};

use crate::{config::Config, herdr::Herdr, history};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}

const MAX_BODY: usize = 64 * 1024;
type HttpResponse = Response<Full<Bytes>>;

struct Console {
    herdr: Herdr,
    home: PathBuf,
    // Prevent two browser submissions from interleaving in a terminal input field.
    input: Mutex<()>,
}

pub async fn run(config: Config) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(config.log_level)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
    let listener = TcpListener::bind(config.http_addr)
        .await
        .context("cannot bind HTTP listener")?;
    let console = Arc::new(Console {
        herdr: Herdr::new(config.herdr_socket),
        home: config.home,
        input: Mutex::new(()),
    });
    tracing::info!(address = %listener.local_addr()?, "remote console listening");
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let (stream, _) = connection?;
                let console = console.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let console = console.clone();
                        async move { Ok::<_, Infallible>(handle(console, request).await) }
                    });
                    if let Err(error) = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service).await {
                        tracing::debug!(%error, "HTTP connection closed");
                    }
                });
            }
            result = tokio::signal::ctrl_c() => { result?; return Ok(()); }
        }
    }
}

async fn handle(console: Arc<Console>, request: Request<Incoming>) -> HttpResponse {
    let path = request.uri().path();
    match (request.method(), path) {
        (&Method::GET, "/api/hello") => json_response(
            StatusCode::OK,
            &json!({"name":"agent-talk","version":env!("CARGO_PKG_VERSION")}),
        ),
        (&Method::GET, "/api/agents") => match console.herdr.list().await {
            Ok(agents) => json_response(StatusCode::OK, &json!({"agents":agents})),
            Err(error) => adapter_error(&error),
        },
        (&Method::GET, "/api/conversation") => conversation(&console, request.uri().query()).await,
        (&Method::POST, "/api/messages") => submit(&console, request).await,
        (_, path) if path.starts_with("/api/") || path == "/api" => {
            error_response(StatusCode::NOT_FOUND, "not_found", "この API はありません")
        }
        (&Method::GET, path) => static_response(path),
        _ => error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "この操作は利用できません",
        ),
    }
}

async fn conversation(console: &Console, query: Option<&str>) -> HttpResponse {
    let Ok(query) = parse_query(query.unwrap_or_default()) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "宛先とセッションを指定してください",
        );
    };
    let (Some(pane), Some(session)) = (query.get("pane"), query.get("session")) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "宛先とセッションを指定してください",
        );
    };
    let agent = match console.herdr.get(pane).await {
        Ok(agent) => agent,
        Err(error) => return adapter_error(&error),
    };
    if agent.session_id.as_deref() != Some(session.as_str()) {
        return error_response(
            StatusCode::CONFLICT,
            "session_changed",
            "対象のセッションが変わりました。一覧から確認し直してください",
        );
    }
    let home = console.home.clone();
    match tokio::task::spawn_blocking(move || history::read(&agent, &home)).await {
        Ok(Ok(conversation)) => json_response(StatusCode::OK, &json!(conversation)),
        Ok(Err(error)) => adapter_error(&error),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "history_unavailable",
            "会話履歴を読み出せませんでした",
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    pane_id: String,
    session_id: String,
    body: String,
}

fn validate_message(message: &Message) -> bool {
    !message.pane_id.is_empty()
        && message.pane_id.len() <= 256
        && !message.session_id.is_empty()
        && message.session_id.len() <= 4096
        && crate::herdr::validate_message(&message.body).is_ok()
}

fn same_origin(headers: &hyper::HeaderMap) -> bool {
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| value == "cross-site")
    {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Ok(uri) = origin.parse::<hyper::Uri>() else {
        return false;
    };
    matches!(uri.scheme_str(), Some("http" | "https"))
        && uri.authority().is_some_and(|authority| {
            headers
                .get(header::HOST)
                .and_then(|host| host.to_str().ok())
                == Some(authority.as_str())
        })
}

async fn submit(console: &Console, request: Request<Incoming>) -> HttpResponse {
    if !same_origin(request.headers()) {
        return error_response(
            StatusCode::FORBIDDEN,
            "cross_origin",
            "同じサイトから送信してください",
        );
    }
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| {
            !value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
    {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_content_type",
            "JSON で送信してください",
        );
    }
    let collected = tokio::time::timeout(
        Duration::from_secs(10),
        Limited::new(request.into_body(), MAX_BODY).collect(),
    )
    .await;
    let Ok(Ok(collected)) = collected else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "本文が大きすぎるか、受信が完了しませんでした",
        );
    };
    let Ok(message) = serde_json::from_slice::<Message>(&collected.to_bytes()) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "宛先・セッション・本文を確認してください",
        );
    };
    if !validate_message(&message) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "空の本文、32 KiB を超える本文、制御文字は送れません",
        );
    }
    let _guard = console.input.lock().await;
    match console
        .herdr
        .send(&message.pane_id, &message.session_id, &message.body)
        .await
    {
        Ok(()) => json_response(StatusCode::OK, &json!({"status":"submitted"})),
        Err(error) => adapter_error(&error),
    }
}

fn adapter_error(error: &anyhow::Error) -> HttpResponse {
    let detail = error.to_string();
    let (code, message) = error
        .downcast_ref::<crate::herdr::RemoteError>()
        .map_or(("unavailable", detail.as_str()), |error| {
            (error.code, error.message.as_str())
        });
    let status = match code {
        "session_changed" | "blocked" | "unknown" | "unregistered" | "unsupported" => {
            StatusCode::CONFLICT
        }
        "not_found" => StatusCode::NOT_FOUND,
        "invalid_input" => StatusCode::BAD_REQUEST,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    error_response(status, code, message.trim())
}

fn parse_query(query: &str) -> Result<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    for field in query.split('&').filter(|field| !field.is_empty()) {
        let (key, value) = field.split_once('=').context("query field has no value")?;
        let key = decode(key)?;
        anyhow::ensure!(
            matches!(key.as_str(), "pane" | "session"),
            "unknown query field"
        );
        anyhow::ensure!(
            fields.insert(key, decode(value)?).is_none(),
            "duplicate query field"
        );
    }
    Ok(fields)
}

fn decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let pair = bytes
                    .get(index + 1..index + 3)
                    .context("incomplete escape")?;
                let hex = std::str::from_utf8(pair)?;
                decoded.push(u8::from_str_radix(hex, 16)?);
                index += 3;
            }
            byte => {
                decoded.push(if byte == b'+' { b' ' } else { byte });
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).context("query is not UTF-8")
}

fn json_response(status: StatusCode, value: &Value) -> HttpResponse {
    response(
        status,
        "application/json; charset=utf-8",
        Bytes::from(value.to_string()),
    )
}

fn error_response(status: StatusCode, code: &str, message: &str) -> HttpResponse {
    json_response(status, &json!({"error":{"code":code,"message":message}}))
}

fn response(status: StatusCode, mime: &str, body: Bytes) -> HttpResponse {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .header("referrer-policy", "same-origin")
        .body(Full::new(body))
        .expect("static response headers are valid")
}

fn static_response(path: &str) -> HttpResponse {
    let asset = embedded::ASSETS
        .iter()
        .find(|(route, _)| *route == path)
        .or_else(|| {
            embedded::ASSETS
                .iter()
                .find(|(route, _)| *route == "/index.html")
        });
    let Some((route, bytes)) = asset else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "static_assets_unavailable",
            "ブラウザ画面を含むビルドが必要です",
        );
    };
    let mime = match route.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    };
    response(StatusCode::OK, mime, Bytes::from_static(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_preserves_opaque_identity_and_rejects_ambiguity() {
        let query = parse_query("pane=w1%3Ap2&session=a%2Fb%25%CE%B1").unwrap();
        assert_eq!(query["pane"], "w1:p2");
        assert_eq!(query["session"], "a/b%α");
        for invalid in ["pane=%", "pane=%ff", "pane=a&pane=b", "path=/tmp/file"] {
            assert!(parse_query(invalid).is_err());
        }
    }

    #[test]
    fn message_validation_preserves_text_but_rejects_terminal_controls() {
        let mut message = Message {
            pane_id: "w1:p2".into(),
            session_id: "session".into(),
            body: "  原文\n続き\t保持  ".into(),
        };
        assert!(validate_message(&message));
        for body in ["", " \n", "text\u{1b}[A", "text\rnext", "\0"] {
            message.body = body.into();
            assert!(!validate_message(&message));
        }
        message.body = "a".repeat(32 * 1024 + 1);
        assert!(!validate_message(&message));
    }

    #[test]
    fn writes_reject_cross_site_and_allow_the_proxy_public_host() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(header::HOST, "example.ts.net:8443".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "https://example.ts.net:8443".parse().unwrap(),
        );
        assert!(same_origin(&headers));
        headers.insert(header::ORIGIN, "https://unrelated.example".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.remove(header::ORIGIN);
        headers.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(!same_origin(&headers));
    }
}
