//! Client for the bank's `/charge` endpoint.

use std::time::Duration;

use serde_json::{json, Value};

use crate::auth::random_hex;
use crate::config::Config;

/// Charge `amount` for `order_id` and return the bank reference.
///
/// Retries on network errors and 5xx answers, always with the same
/// `Idempotency-Key`, so a retry can never create a second charge.
/// (The legacy client retried without a key and could double charge.)
pub async fn charge(
    http: &reqwest::Client,
    cfg: &Config,
    order_id: i32,
    amount: f64,
) -> Option<String> {
    let key = format!("shopcore-{order_id}-{}", random_hex(12));
    let url = format!("{}/charge", cfg.bank_url);
    for attempt in 0..cfg.bank_attempts {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
        }
        let res = http
            .post(&url)
            .header("Idempotency-Key", &key)
            .json(&json!({ "order_id": order_id, "amount": amount }))
            .timeout(cfg.bank_timeout)
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {
                let body: Value = match r.json().await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(order_id, attempt, error = %e, "bank sent an unreadable response");
                        continue;
                    }
                };
                if let Some(bank_ref) = body.get("bank_ref").and_then(Value::as_str) {
                    return Some(bank_ref.to_string());
                }
                tracing::warn!(order_id, attempt, "bank response had no bank_ref");
            }
            Ok(r) if r.status().is_client_error() => {
                tracing::warn!(order_id, status = %r.status(), "bank rejected the charge");
                return None;
            }
            Ok(r) => {
                tracing::warn!(order_id, attempt, status = %r.status(), "bank error, retrying")
            }
            Err(e) => tracing::warn!(order_id, attempt, error = %e, "bank unreachable, retrying"),
        }
    }
    None
}
