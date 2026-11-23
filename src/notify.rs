//! Background side effects: order webhooks (`notify_url`) and e-mails.
//!
//! The legacy service kept an in-memory job queue and an e-mail "outbox" list
//! that nothing ever read. E-mails are now written to the log; webhooks are
//! delivered by background tasks, in order per order (pending before paid).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct Notifier {
    http: reqwest::Client,
    chains: Arc<Mutex<HashMap<i32, JoinHandle<()>>>>,
}

impl Notifier {
    pub fn new(http: reqwest::Client) -> Self {
        Notifier {
            http,
            chains: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// POST `body` to `url` in the background. Deliveries for the same order
    /// run one after another, in the order they were queued.
    pub fn notify(&self, order_id: i32, url: String, body: Value) {
        // a poisoned lock only means another task panicked mid-insert; the map is still usable
        let mut chains = self
            .chains
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if chains.len() > 1024 {
            chains.retain(|_, h| !h.is_finished());
        }
        let previous = chains.remove(&order_id);
        let http = self.http.clone();
        let handle = tokio::spawn(async move {
            if let Some(prev) = previous {
                let _ = prev.await;
            }
            deliver(&http, &url, &body).await;
        });
        chains.insert(order_id, handle);
    }

    pub fn email(&self, to: Option<&str>, subject: &str, message: &str) {
        tracing::info!(target: "shopcore::mail", to = to.unwrap_or(""), subject, message, "email queued");
    }
}

async fn deliver(http: &reqwest::Client, url: &str, body: &Value) {
    for attempt in 0..3u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500 * attempt)).await;
        }
        match http
            .post(url)
            .json(body)
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return,
            Ok(r) if r.status().is_client_error() => {
                tracing::warn!(url, status = %r.status(), "notify rejected");
                return;
            }
            Ok(r) => tracing::warn!(url, status = %r.status(), attempt, "notify failed"),
            Err(e) => tracing::warn!(url, error = %e, attempt, "notify failed"),
        }
    }
}
