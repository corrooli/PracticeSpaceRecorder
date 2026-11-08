//! HTTP + WebSocket server. Serves the very same UI to the app window and to
//! any tablet / phone on the network.
//!
//! Trust model: only connections from this machine (loopback peer *and* a
//! loopback Host header, which defeats DNS rebinding) may edit presets or
//! settings. Remote devices can record, stop and switch presets. That's it.

use crate::engine::Engine;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

#[derive(RustEmbed)]
#[folder = "../dist"]
struct Assets;

/// Bind (falling back to the next free ports) and serve. Returns the port.
pub async fn serve(engine: Arc<Engine>, port: u16) -> anyhow::Result<u16> {
    let mut listener = None;
    for p in port..port.saturating_add(20) {
        if let Ok(l) = tokio::net::TcpListener::bind(("0.0.0.0", p)).await {
            listener = Some(l);
            break;
        }
    }
    let listener = match listener {
        Some(l) => l,
        None => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let port = listener.local_addr()?.port();
    let app = Router::new().route("/ws", get(ws_handler)).fallback(get(static_handler)).with_state(engine);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await {
            log::error!("server stopped: {e}");
        }
    });
    log::info!("UI server on port {port}");
    Ok(port)
}

fn is_local(addr: &SocketAddr, headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let host_name = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    addr.ip().is_loopback() && matches!(host_name, "127.0.0.1" | "localhost" | "[::1]")
}

fn origin_ok(headers: &HeaderMap) -> bool {
    // Browsers always send Origin on WebSocket upgrades. It has to match the
    // host we're serving, so random websites can't drive the recorder.
    let Some(origin) = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) else { return true };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let o = origin.trim_start_matches("http://").trim_start_matches("https://");
    o == host || origin.starts_with("tauri://") || origin == "null"
}

async fn ws_handler(ws: WebSocketUpgrade, State(engine): State<Arc<Engine>>, ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap) -> Response {
    let local = is_local(&addr, &headers);
    if !origin_ok(&headers) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    if !local && !engine.settings().remote_enabled {
        return (StatusCode::FORBIDDEN, "remote control disabled").into_response();
    }
    ws.on_upgrade(move |socket| client(socket, engine, local))
}

async fn client(socket: WebSocket, engine: Arc<Engine>, privileged: bool) {
    let (mut tx, mut rx) = socket.split();
    let mut sub = engine.subscribe();
    if !privileged {
        engine.clients.fetch_add(1, Ordering::Relaxed);
    }
    let hello = serde_json::json!({ "t": "hello", "privileged": privileged }).to_string();
    let init_ok = tx.send(Message::Text(hello)).await.is_ok()
        && tx.send(Message::Text(engine.snapshot().to_string())).await.is_ok()
        && tx.send(Message::Text(engine.history().to_string())).await.is_ok();
    if init_ok {
        let eng = engine.clone();
        let mut send_task = tokio::spawn(async move {
            loop {
                match sub.recv().await {
                    Ok(m) => {
                        if tx.send(Message::Text(m.to_string())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // slow client (sleepy iPad): resync instead of choking
                        let _ = tx.send(Message::Text(eng.history().to_string())).await;
                    }
                    Err(_) => break,
                }
            }
        });
        let eng = engine.clone();
        let mut recv_task = tokio::spawn(async move {
            while let Some(Ok(msg)) = rx.next().await {
                match msg {
                    Message::Text(t) => {
                        let e = eng.clone();
                        tokio::task::spawn_blocking(move || e.handle(&t, privileged));
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });
        tokio::select! {
            _ = &mut send_task => recv_task.abort(),
            _ = &mut recv_task => send_task.abort(),
        }
    }
    if !privileged {
        engine.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match Assets::get(path).or_else(|| if path.contains('.') { None } else { Assets::get("index.html") }) {
        Some(f) => {
            let mime = f.metadata.mimetype().to_string();
            let cache = if path == "index.html" { "no-cache" } else { "public, max-age=31536000, immutable" };
            ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache.to_string())], f.data.into_owned()).into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
