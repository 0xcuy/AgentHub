use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Live event pushed to subscribers of `GET /v1/inbox/stream`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveEvent {
    InboxMessage {
        agent_id: i64,
        receipt_id: i64,
        message: crate::models::Message,
    },
    Mail {
        agent_id: i64,
        mail: crate::models::MailMessage,
    },
}

#[derive(Clone)]
pub struct Events(Arc<broadcast::Sender<LiveEvent>>);

impl Events {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self(Arc::new(tx))
    }

    pub fn publish(&self, ev: LiveEvent) {
        // No active subscribers is fine (UI not connected).
        let _ = self.0.send(ev);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.0.subscribe()
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}
