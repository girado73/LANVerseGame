//! WebSocket upgrade on `/` (and `/app/<key>`, kept as a no-op alias).
//!
//! The socket task only moves bytes: it forwards inbound text frames to the
//! game loop and writes whatever the loop queued. No simulation state lives in
//! the task, so a slow client can never stall a tick.

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::net::router::{InProcessRouter, Inbound, OutFrame};
use crate::net::state::AppState;

/// Bounded socket queue: a client that cannot keep up is dropped instead of
/// growing the server's memory.
const SOCKET_QUEUE: usize = 256;

pub async fn upgrade(
    websocket: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // The upgrade closure runs on the multi-threaded runtime, so it may only
    // capture `Send` data: the router, no game state.
    let router = state.router.clone();
    let connection_id = format!(
        "c-{}-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        crate::sim::random::alphanumeric(8)
    );

    websocket
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| serve(socket, router, connection_id))
}

async fn serve(socket: WebSocket, router: Arc<InProcessRouter>, connection_id: String) {
    let out_receiver = router.register(&connection_id, SOCKET_QUEUE);
    router.send_inbound(&connection_id, Inbound::Open);

    let (mut sink, mut stream) = socket.split();
    let writer = tokio::spawn(async move { write_loop(&mut sink, out_receiver).await });

    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Text(text)) => {
                router.send_inbound(&connection_id, Inbound::Frame(text.to_string()));
            }
            Ok(Message::Binary(bytes)) => {
                if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                    router.send_inbound(&connection_id, Inbound::Frame(text));
                }
            }
            Ok(Message::Ping(payload)) => {
                // Autobahn pings are answered by axum's tungstenite layer.
                let _ = payload;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }

    router.send_inbound(&connection_id, Inbound::Close);
    router.unregister(&connection_id);
    let _ = writer.await;
    // Dropping the receiver closes the writer if it is still running.
    tracing::debug!(%connection_id, "websocket closed");
}

async fn write_loop(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    mut frames: mpsc::Receiver<OutFrame>,
) {
    while let Some(frame) = frames.recv().await {
        if frame.close {
            let _ = sink
                .send(Message::Close(Some(CloseFrame {
                    code: 1000,
                    reason: "server".into(),
                })))
                .await;
            return;
        }

        let Some(json) = frame.as_str() else {
            continue;
        };

        let message = match &frame.json {
            crate::net::router::OutPayload::Owned(text) => Message::Text(text.clone().into()),
            crate::net::router::OutPayload::Shared(text) => Message::Text(text.to_string().into()),
            crate::net::router::OutPayload::Close => continue,
        };
        let _ = json;

        if sink.send(message).await.is_err() {
            return;
        }
    }

    let _ = sink.send(Message::Close(None)).await;
}

/// Frame type the loop and the socket share, re-exported for tests.
pub use crate::net::router::OutFrame as Frame;
