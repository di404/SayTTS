//! OBS 字幕页：http://127.0.0.1:端口/overlay ，页面经 /ws 收 show / hide / clear。
//! 只监听本机地址。

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::{Html, IntoResponse},
    routing::get,
};
use serde::Serialize;
use tokio::sync::{broadcast, watch};

const PAGE: &str = include_str!("overlay.html");

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OverlayEvent {
    Show { id: u64, text: String },
    Hide { id: u64 },
    Clear,
}

#[derive(Clone)]
pub struct Overlay {
    tx: broadcast::Sender<OverlayEvent>,
    current: Arc<Mutex<Option<(u64, String)>>>,
    stop: Arc<watch::Sender<bool>>,
    pub port: u16,
}

impl Overlay {
    /// 在 127.0.0.1:port 上启动。端口被占用时返回错误。
    pub async fn start(port: u16) -> Result<Self> {
        let (tx, _) = broadcast::channel(64);
        let me = Self { tx, current: Arc::new(Mutex::new(None)), stop: Arc::new(watch::Sender::new(false)), port };
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("字幕页端口 {port} 被占用"))?;
        let app = Router::new()
            .route("/overlay", get(|| async { Html(PAGE) }))
            .route("/ws", get(ws))
            .with_state(me.clone());
        let mut stop = me.stop.subscribe();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stop.wait_for(|s| *s).await;
                })
                .await;
        });
        Ok(me)
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/overlay", self.port)
    }

    pub fn show(&self, id: u64, text: &str) {
        *self.current.lock().unwrap() = Some((id, text.to_string()));
        let _ = self.tx.send(OverlayEvent::Show { id, text: text.to_string() });
    }

    pub fn hide(&self, id: u64) {
        let mut cur = self.current.lock().unwrap();
        if cur.as_ref().is_some_and(|(c, _)| *c == id) {
            *cur = None;
        }
        let _ = self.tx.send(OverlayEvent::Hide { id });
    }

    pub fn clear(&self) {
        *self.current.lock().unwrap() = None;
        let _ = self.tx.send(OverlayEvent::Clear);
    }

    /// 关掉服务和所有已连上的页面，放开端口。
    pub fn shutdown(&self) {
        self.stop.send_replace(true);
    }
}

async fn ws(upgrade: WebSocketUpgrade, State(o): State<Overlay>) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| client(socket, o))
}

async fn client(mut socket: WebSocket, o: Overlay) {
    let mut rx = o.tx.subscribe();
    let mut stop = o.stop.subscribe();
    // 新打开的页面先显示当前这句
    let now = o.current.lock().unwrap().clone();
    if let Some((id, text)) = now
        && send(&mut socket, &OverlayEvent::Show { id, text }).await.is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => if send(&mut socket, &ev).await.is_err() { return },
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                _ => {}
            },
            _ = async { let _ = stop.wait_for(|s| *s).await; } => return,
        }
    }
}

async fn send(socket: &mut WebSocket, ev: &OverlayEvent) -> Result<(), axum::Error> {
    socket.send(Message::Text(serde_json::to_string(ev).unwrap_or_default().into())).await
}
