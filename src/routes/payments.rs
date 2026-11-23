use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};

use super::{path_id, JsonBody};
use crate::auth::CurrentUser;
use crate::bank;
use crate::error::{ApiError, ApiResult};
use crate::models::Order;
use crate::AppState;

/// Pay a pending order from the wallet or through the bank.
///
/// The order row is locked (`FOR UPDATE`) for the whole operation, so two
/// concurrent pay requests can never both succeed: the second one waits, then
/// sees the order is paid and gets 409. The wallet debit is a conditional
/// UPDATE, so the balance can never go negative.
pub async fn pay(
    State(st): State<AppState>,
    Path(id): Path<String>,
    cu: CurrentUser,
    JsonBody(b): JsonBody,
) -> ApiResult<Json<Value>> {
    let id = path_id(&id)?;
    let user = cu.user;
    let method = match b.get("method") {
        None => "wallet".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => String::new(),
    };

    let mut tx = st.pool.begin().await?;
    let o = sqlx::query_as::<_, Order>(
        "SELECT * FROM orders WHERE id = $1 AND user_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(ApiError::not_found)?;
    if o.status() != "pending" {
        return Err(ApiError::conflict("order not payable"));
    }
    let total = o.total();

    let bank_ref = match method.as_str() {
        "wallet" => {
            let debited: Option<f64> = sqlx::query_scalar(
                "UPDATE users SET wallet_balance = round((COALESCE(wallet_balance, 0) - $1::float8)::numeric, 2)::float8 \
                 WHERE id = $2 AND COALESCE(wallet_balance, 0) >= $1::float8 RETURNING wallet_balance",
            )
            .bind(total)
            .bind(user.id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
            if debited.is_none() {
                return Err(ApiError::payment_required("insufficient wallet balance"));
            }
            None
        }
        "bank" => match bank::charge(&st.http, &st.cfg, o.id, total).await {
            Some(r) => Some(r),
            None => return Err(ApiError::bad_gateway("payment failed")),
        },
        _ => return Err(ApiError::bad_request("unknown method")),
    };

    sqlx::query("INSERT INTO payments (order_id, amount, method, bank_ref, status) VALUES ($1, $2, $3, $4, 'captured')")
        .bind(id)
        .bind(total)
        .bind(&method)
        .bind(&bank_ref)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE orders SET status = 'paid', paid_amount = $1 WHERE id = $2")
        .bind(total)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let order_no = o.order_no.unwrap_or_default();
    st.notifier.email(
        user.email.as_deref(),
        &format!("Payment received for order {order_no}"),
        &format!("Thanks, we received {total}."),
    );
    if let Some(url) = o.notify_url.clone().filter(|u| !u.is_empty()) {
        st.notifier.notify(
            id,
            url,
            json!({ "order_id": id, "order_no": o.order_no, "status": "paid" }),
        );
    }
    Ok(Json(
        json!({ "order_id": id, "status": "paid", "method": method, "bank_ref": bank_ref }),
    ))
}
