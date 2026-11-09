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

/// Host header without the port. Handles `[::1]:8080` and bare `[::1]`.
fn host_name(host: &str) -> &str {
    if host.starts_with('[') {
        return host.find(']').map(|i| &host[..=i]).unwrap_or(host);
    }
    host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
}

fn is_local(addr: &SocketAddr, headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    addr.ip().is_loopback() && matches!(host_name(host), "127.0.0.1" | "localhost" | "[::1]")
}

fn origin_ok(headers: &HeaderMap) -> bool {
    // Browsers always send Origin on WebSocket upgrades. It has to match the
    // host we're serving, so random websites can't drive the recorder.
    let Some(origin) = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) else { return true };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    // No "null" here: sandboxed iframes on *any* website send Origin: null, and
    // from this machine that would count as privileged (presets run mount_cmd).
    // The app window loads http://127.0.0.1:<port>, so it matches like any other.
    let Some(o) = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://")) else { return false };
    !host.is_empty() && o == host
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(host: Option<&str>, origin: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = host {
            h.insert(header::HOST, HeaderValue::from_str(v).unwrap());
        }
        if let Some(v) = origin {
            h.insert(header::ORIGIN, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn host_name_strips_port() {
        assert_eq!(host_name("127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_name("localhost"), "localhost");
        assert_eq!(host_name("[::1]:8080"), "[::1]");
        assert_eq!(host_name("[::1]"), "[::1]");
        assert_eq!(host_name(""), "");
    }

    #[test]
    fn local_needs_loopback_peer_and_loopback_host() {
        let v4 = addr("127.0.0.1:5000");
        let v6 = addr("[::1]:5000");
        assert!(is_local(&v4, &headers(Some("127.0.0.1:8080"), None)));
        assert!(is_local(&v4, &headers(Some("localhost:8080"), None)));
        assert!(is_local(&v6, &headers(Some("[::1]:8080"), None)));
        assert!(is_local(&v6, &headers(Some("[::1]"), None)));
    }

    #[test]
    fn remote_peer_is_never_local() {
        let lan = addr("192.168.1.20:5000");
        assert!(!is_local(&lan, &headers(Some("127.0.0.1:8080"), None)));
        assert!(!is_local(&lan, &headers(Some("localhost"), None)));
    }

    #[test]
    fn dns_rebinding_host_is_not_local() {
        // Browser on this machine, evil.com rebound to 127.0.0.1.
        let v4 = addr("127.0.0.1:5000");
        assert!(!is_local(&v4, &headers(Some("evil.com:8080"), None)));
        assert!(!is_local(&v4, &headers(Some("localhost.evil.com:8080"), None)));
        assert!(!is_local(&v4, &headers(Some("127.0.0.1.evil.com"), None)));
        assert!(!is_local(&v4, &headers(None, None)));
    }

    #[test]
    fn origin_must_match_host() {
        assert!(origin_ok(&headers(Some("127.0.0.1:8080"), Some("http://127.0.0.1:8080"))));
        assert!(origin_ok(&headers(Some("192.168.1.5:8080"), Some("http://192.168.1.5:8080"))));
        assert!(origin_ok(&headers(Some("rec.local:8080"), Some("https://rec.local:8080"))));
        assert!(!origin_ok(&headers(Some("127.0.0.1:8080"), Some("http://evil.com"))));
        assert!(!origin_ok(&headers(Some("127.0.0.1:8080"), Some("http://127.0.0.1:9999"))));
        assert!(!origin_ok(&headers(Some("127.0.0.1:8080"), Some("http://127.0.0.1:8080.evil.com"))));
    }

    #[test]
    fn origin_rejects_null_and_odd_schemes() {
        // Sandboxed iframes on any website send Origin: null.
        assert!(!origin_ok(&headers(Some("127.0.0.1:8080"), Some("null"))));
        assert!(!origin_ok(&headers(Some("127.0.0.1:8080"), Some("tauri://localhost"))));
        assert!(!origin_ok(&headers(Some("127.0.0.1:8080"), Some("127.0.0.1:8080"))));
        assert!(!origin_ok(&headers(None, Some("http://"))));
    }

    #[test]
    fn missing_origin_is_allowed() {
        // Non-browser clients (curl, scripts) don't send Origin and can't be CSRF'd.
        assert!(origin_ok(&headers(Some("127.0.0.1:8080"), None)));
    }
}

#[cfg(test)]
mod live_tests {
    //! Real server, real sockets.
    use super::*;
    use crate::config::Store;
    use crate::dest::DestMonitor;
    use crate::jobs::Jobs;
    use crate::testutil::{wait_for, TempDir};
    use std::sync::atomic::AtomicBool;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as WsMsg};

    fn no_awake() -> Option<std::process::Child> {
        None
    }

    fn engine(dir: &TempDir, remote: bool) -> Arc<Engine> {
        let mut store = Store::load(&dir.join("config.json"));
        store.settings.remote_enabled = remote;
        let jobs = Jobs::load(dir.join("data"), Box::new(|| {}));
        let (tx, rx) = crossbeam_channel::unbounded();
        std::mem::forget(rx); // keep the audio channel open
        Engine::new(store, tx, jobs, DestMonitor::new(Box::new(|| {})), Arc::new(AtomicBool::new(false)), no_awake)
    }

    type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

    async fn connect(port: u16, host: &str, origin: Option<&str>) -> Result<Ws, u16> {
        let mut req = format!("ws://127.0.0.1:{port}/ws").into_client_request().unwrap();
        req.headers_mut().insert(header::HOST, host.parse().unwrap());
        if let Some(o) = origin {
            req.headers_mut().insert(header::ORIGIN, o.parse().unwrap());
        }
        match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(r)) => Err(r.status().as_u16()),
            Err(e) => panic!("{e}"),
        }
    }

    async fn next_json(ws: &mut Ws) -> serde_json::Value {
        loop {
            match ws.next().await.unwrap().unwrap() {
                WsMsg::Text(t) => return serde_json::from_str(&t).unwrap(),
                _ => continue,
            }
        }
    }

    async fn get(port: u16, path: &str) -> (u16, String, String) {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut b = Vec::new();
        s.read_to_end(&mut b).await.unwrap();
        let text = String::from_utf8_lossy(&b).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, head.to_ascii_lowercase(), body.to_string())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_ui_is_privileged_and_gets_initial_state() {
        let dir = TempDir::new("srv");
        let e = engine(&dir, true);
        let port = serve(e.clone(), 0).await.unwrap();
        let host = format!("127.0.0.1:{port}");
        let mut ws = connect(port, &host, Some(&format!("http://{host}"))).await.unwrap();
        assert_eq!(next_json(&mut ws).await, serde_json::json!({"t":"hello","privileged":true}));
        assert_eq!(next_json(&mut ws).await["t"], "state");
        assert_eq!(next_json(&mut ws).await["t"], "hist");
        // broadcasts reach the client
        e.picked("r", None);
        assert_eq!(next_json(&mut ws).await["t"], "picked");
        // privileged commands work
        ws.send(WsMsg::Text(r#"{"cmd":"save_settings","settings":{"fullscreen":false}}"#.into())).await.unwrap();
        assert!(wait_for(5, || !e.settings().fullscreen));
        assert_eq!(e.clients.load(Ordering::Relaxed), 0, "local window is not counted as remote");
        ws.send(WsMsg::Close(None)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn localhost_name_and_no_origin_are_local() {
        let dir = TempDir::new("srv");
        let port = serve(engine(&dir, false), 0).await.unwrap();
        let mut ws = connect(port, &format!("localhost:{port}"), None).await.unwrap();
        assert_eq!(next_json(&mut ws).await["privileged"], true);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_client_is_restricted_and_counted() {
        let dir = TempDir::new("srv");
        let e = engine(&dir, true);
        let port = serve(e.clone(), 0).await.unwrap();
        let host = format!("192.168.1.50:{port}");
        let mut ws = connect(port, &host, Some(&format!("http://{host}"))).await.unwrap();
        assert_eq!(next_json(&mut ws).await["privileged"], false);
        assert!(wait_for(5, || e.clients.load(Ordering::Relaxed) == 1));
        ws.send(WsMsg::Text(r#"{"cmd":"save_settings","settings":{"remote_enabled":false}}"#.into())).await.unwrap();
        assert!(wait_for(5, || e.snapshot()["toast"]["msg"] == "Settings can only be changed on the recording computer"));
        assert!(e.settings().remote_enabled);
        ws.send(WsMsg::Binary(vec![1, 2, 3])).await.unwrap();
        ws.send(WsMsg::Close(None)).await.unwrap();
        drop(ws);
        assert!(wait_for(5, || e.clients.load(Ordering::Relaxed) == 0), "disconnect is counted");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remote_disabled_and_foreign_origins_are_refused() {
        let dir = TempDir::new("srv");
        let port = serve(engine(&dir, false), 0).await.unwrap();
        let lan = format!("192.168.1.50:{port}");
        assert_eq!(connect(port, &lan, Some(&format!("http://{lan}"))).await.err(), Some(403));
        let local = format!("127.0.0.1:{port}");
        assert_eq!(connect(port, &local, Some("http://evil.example")).await.err(), Some(403));
        assert_eq!(connect(port, &local, Some("null")).await.err(), Some(403));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_client_is_resynced_instead_of_dropped() {
        let dir = TempDir::new("srv");
        let e = engine(&dir, true);
        let port = serve(e.clone(), 0).await.unwrap();
        let host = format!("127.0.0.1:{port}");
        let mut ws = connect(port, &host, None).await.unwrap();
        for _ in 0..3 {
            next_json(&mut ws).await;
        }
        // a sleepy iPad: not reading while lots of big messages go out
        let big = "x".repeat(16 * 1024);
        for i in 0..3000 {
            e.picked(&i.to_string(), Some(big.clone()));
        }
        let mut resynced = false;
        for _ in 0..3000 {
            let m = tokio::time::timeout(std::time::Duration::from_secs(5), next_json(&mut ws)).await.unwrap();
            if m["t"] == "hist" {
                resynced = true;
                break;
            }
        }
        assert!(resynced, "lagging client gets a history resync");
        // and keeps working afterwards
        e.picked("after", None);
        let mut seen = false;
        for _ in 0..3000 {
            let m = tokio::time::timeout(std::time::Duration::from_secs(5), next_json(&mut ws)).await.unwrap();
            if m["req"] == "after" {
                seen = true;
                break;
            }
        }
        assert!(seen);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_ports_busy_still_starts_on_a_random_one() {
        let dir = TempDir::new("srv");
        // find 20 free consecutive ports and hold them all
        let mut held = Vec::new();
        'search: for base in (20000u16..60000).step_by(97) {
            held.clear();
            for p in base..base + 20 {
                match std::net::TcpListener::bind(("0.0.0.0", p)) {
                    Ok(l) => held.push(l),
                    Err(_) => continue 'search,
                }
            }
            break;
        }
        assert_eq!(held.len(), 20);
        let base = held[0].local_addr().unwrap().port();
        let port = serve(engine(&dir, true), base).await.unwrap();
        assert!(!(base..base + 20).contains(&port));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serves_the_ui() {
        let dir = TempDir::new("srv");
        let port = serve(engine(&dir, true), 0).await.unwrap();
        let (st, head, body) = get(port, "/").await;
        assert_eq!(st, 200);
        assert!(head.contains("content-type: text/html"));
        assert!(head.contains("cache-control: no-cache"));
        assert!(body.contains("<html") || body.contains("<!doctype") || body.contains("<!DOCTYPE"));
        // client-side routes fall back to the app
        let (st, _, deep) = get(port, "/presets/edit").await;
        assert_eq!((st, deep.len()), (200, body.len()));
        assert_eq!(get(port, "/missing.png").await.0, 404);
        let (st, head, _) = get(port, "/icon.png").await;
        assert_eq!(st, 200);
        assert!(head.contains("immutable") && head.contains("image/png"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_port_falls_back_to_the_next_one() {
        let dir = TempDir::new("srv");
        let taken = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let p = taken.local_addr().unwrap().port();
        let port = serve(engine(&dir, true), p).await.unwrap();
        assert_ne!(port, p);
        assert!(port > p && port < p.saturating_add(20) || port != 0);
    }
}
