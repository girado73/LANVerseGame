//! In-process routes between sockets and the game loop.
//!
//! The WebSocket task reads frames into [`Inbox`] and writes frames from its
//! [`Outbox`]; the loop is the only writer of outboxes and the only reader of
//! inboxes, so neither side blocks on the other.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex};

use tokio::sync::mpsc;

use crate::net::connection::Transport;

/// A frame plus whether the socket should close after it.
///
/// Snapshots share one allocation across every connection of a match
/// (`Arc<str>`), so a 100-player frame is not copied 100 times per tick.
#[derive(Debug, Clone)]
pub struct OutFrame {
    pub json: OutPayload,
    pub close: bool,
}

/// One queue payload.
#[derive(Debug, Clone)]
pub enum OutPayload {
    /// A frame owned by this connection.
    Owned(String),
    /// A frame shared by several connections.
    Shared(std::sync::Arc<str>),
    /// Close marker without content.
    Close,
}

impl OutFrame {
    pub fn text(json: impl Into<String>) -> Self {
        Self {
            json: OutPayload::Owned(json.into()),
            close: false,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.json {
            OutPayload::Owned(text) => Some(text),
            OutPayload::Shared(text) => Some(text),
            OutPayload::Close => None,
        }
    }
}

impl PartialEq for OutFrame {
    fn eq(&self, other: &Self) -> bool {
        self.close == other.close && self.as_str() == other.as_str()
    }
}

/// What one socket sends to the loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    Open,
    Frame(String),
    Close,
}

/// Frames buffered for the loop before new ones are dropped.
const MAX_LOOP_BACKLOG: usize = 16_384;

/// A loop-bound event with its connection.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopEvent {
    pub connection: String,
    pub inbound: Inbound,
}

/// The loop's handle on one socket.
pub struct Outbox {
    sender: mpsc::Sender<OutFrame>,
}

impl Outbox {
    pub fn try_send(&self, frame: OutFrame) {
        // The socket task may be gone; a dropped frame is fine, the loop will
        // reap the connection by its id.
        let _ = self.sender.try_send(frame);
    }
}

/// Everything one server process routes.
#[derive(Default)]
pub struct InProcessRouter {
    outboxes: Mutex<HashMap<String, Outbox>>,
    /// Socket tasks push here; the game loop drains it on its own thread.
    /// A condvar wakes the loop without depending on its runtime.
    loop_events: Mutex<Vec<LoopEvent>>,
    loop_wakeup: Condvar,
}

impl InProcessRouter {
    /// The loop side of the event queue (shared with the loop thread).
    pub fn event_slot(&self) -> (&Mutex<Vec<LoopEvent>>, &Condvar) {
        (&self.loop_events, &self.loop_wakeup)
    }

    /// Registers a socket; returns the frame stream the socket task owns.
    pub fn register(
        &self,
        connection_id: &str,
        channel_capacity: usize,
    ) -> mpsc::Receiver<OutFrame> {
        let (out_sender, out_receiver) = mpsc::channel(channel_capacity);

        self.outboxes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(connection_id.to_owned(), Outbox { sender: out_sender });

        out_receiver
    }

    /// Forwards an inbound event to the game loop and wakes it.
    pub fn send_inbound(&self, connection_id: &str, event: Inbound) {
        let mut queue = self
            .loop_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // A dead loop would otherwise grow this queue forever.
        if queue.len() >= MAX_LOOP_BACKLOG {
            tracing::warn!(%connection_id, "loop backlog full, dropping frame");
            return;
        }

        queue.push(LoopEvent {
            connection: connection_id.to_owned(),
            inbound: event,
        });
        drop(queue);
        self.loop_wakeup.notify_one();
    }

    pub fn unregister(&self, connection_id: &str) {
        self.outboxes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(connection_id);
    }

    pub fn socket_count(&self) -> usize {
        self.outboxes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl Transport for InProcessRouter {
    fn send_json(&self, connection: &str, json: &str) {
        let outbox = self
            .outboxes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(connection)
            .map(|outbox| outbox.sender.clone());
        if let Some(sender) = outbox {
            let _ = sender.try_send(OutFrame {
                json: OutPayload::Shared(std::sync::Arc::from(json)),
                close: false,
            });
        }
    }

    fn send(&self, connection: &str, frame: &serde_json::Value) {
        let outbox = self
            .outboxes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(connection)
            .map(|outbox| outbox.sender.clone());

        if let Some(sender) = outbox {
            let _ = sender.try_send(OutFrame::text(frame.to_string()));
        }
    }

    fn close(&self, connection: &str) {
        let outbox = self
            .outboxes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(connection)
            .map(|outbox| outbox.sender.clone());

        if let Some(sender) = outbox {
            let _ = sender.try_send(OutFrame {
                json: OutPayload::Close,
                close: true,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_travel_to_the_socket_and_events_to_the_loop() {
        let router = InProcessRouter::default();
        let (events, _wakeup) = router.event_slot();
        let mut out = router.register("c-1", 8);

        router.send("c-1", &serde_json::json!({"type": "pong"}));
        router.close("c-1");
        router.send_inbound("c-1", Inbound::Frame("{\"type\":\"ping\"}".into()));

        let first = out.recv().await.unwrap();
        assert_eq!(first.as_str(), Some(r#"{"type":"pong"}"#));
        assert!(!first.close);

        let second = out.recv().await.unwrap();
        assert!(second.close);

        let event = events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop()
            .expect("loop event");
        assert_eq!(event.connection, "c-1");
        match event.inbound {
            Inbound::Frame(frame) => assert!(frame.contains("ping")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn sending_to_an_unknown_socket_is_ignored() {
        let router = InProcessRouter::default();
        router.send("nobody", &serde_json::json!({"type": "pong"}));
        router.close("nobody");
        assert_eq!(router.socket_count(), 0);
    }
}
