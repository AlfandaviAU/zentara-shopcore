use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::error::{ApiError, ApiResult};
use crate::models::Order;
use crate::util::to_int;
use crate::AppState;

type HmacSha256 = Hmac<Sha256>;

/// Constant-time check of the hex HMAC-SHA256 of the raw body.
pub fn signature_ok(secret: &str, body: &[u8], sig_hex: &str) -> bool {
    let Ok(sig) = hex::decode(sig_hex.trim()) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Bank settlement callback. The signature is now mandatory (legacy accepted
/// requests that simply left the `X-Signature` header out).
pub async fn bank(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let sig = headers
        .get("X-Signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !signature_ok(&st.cfg.bank_secret, &body, sig) {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "bad signature"));
    }
    let data = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(m)) => m,
        _ => return Err(ApiError::bad_request("invalid JSON body")),
    };
    if data.get("status").and_then(Value::as_str) != Some("settled") {
        return Ok(Json(json!({ "ok": true })));
    }
    let Ok(order_id) = i32::try_from(to_int(data.get("order_id"), -1)) else {
        return Ok(Json(json!({ "ok": true })));
    };
    let bank_ref = data
        .get("bank_ref")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut tx = st.pool.begin().await?;
    // lock the order first: if the pay request that created this charge has
    // not committed yet we wait for it, then the payment row is visible
    let order = sqlx::query_as::<_, Order>("SELECT * FROM orders WHERE id = $1 FOR UPDATE")
        .bind(order_id)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("UPDATE payments SET status = 'settled' WHERE order_id = $1 AND bank_ref = $2")
        .bind(order_id)
        .bind(&bank_ref)
        .execute(&mut *tx)
        .await?;
    let mut became_paid = None;
    if let Some(o) = order.filter(|o| o.status() == "pending") {
        sqlx::query("UPDATE orders SET status = 'paid', paid_amount = total WHERE id = $1")
            .bind(order_id)
            .execute(&mut *tx)
            .await?;
        became_paid = Some(o);
    }
    tx.commit().await?;

    if let Some(o) = became_paid {
        if let Some(url) = o.notify_url.clone().filter(|u| !u.is_empty()) {
            st.notifier.notify(
                o.id,
                url,
                json!({ "order_id": o.id, "order_no": o.order_no, "status": "paid" }),
            );
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures() {
        let body = br#"{"order_id": 1, "bank_ref": "BK-x", "status": "settled"}"#;
        let sig = sign("bank-shared-secret-2019", body);
        assert!(signature_ok("bank-shared-secret-2019", body, &sig));
        assert!(!signature_ok("bank-shared-secret-2019", body, "deadbeef"));
        assert!(!signature_ok("bank-shared-secret-2019", body, ""));
        assert!(!signature_ok("other", body, &sig));
    }
}
