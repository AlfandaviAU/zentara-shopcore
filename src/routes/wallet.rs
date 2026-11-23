use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use super::{JsonBody, ADD_TO_WALLET};
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::util::{parse_money, round2, to_int};
use crate::AppState;

const MAX_AMOUNT: f64 = 1_000_000_000.0;

/// A positive amount, rounded to cents. Legacy accepted 0, negatives
/// (a negative transfer pulled money from the recipient) and garbage.
fn amount_field(b: &serde_json::Map<String, Value>) -> ApiResult<f64> {
    let amount = b
        .get("amount")
        .and_then(parse_money)
        .map(round2)
        .ok_or_else(|| ApiError::bad_request("amount must be a number"))?;
    if amount <= 0.0 {
        return Err(ApiError::bad_request("amount must be positive"));
    }
    if amount > MAX_AMOUNT {
        return Err(ApiError::bad_request("amount is too large"));
    }
    Ok(amount)
}

pub async fn get_wallet(cu: CurrentUser) -> Json<Value> {
    Json(json!({ "balance": cu.user.wallet_balance }))
}

/// Atomic increment: concurrent top-ups are never lost (legacy read-modify-write).
pub async fn topup(
    State(st): State<AppState>,
    cu: CurrentUser,
    JsonBody(b): JsonBody,
) -> ApiResult<Json<Value>> {
    let amount = amount_field(&b)?;
    let balance: Option<f64> = sqlx::query_scalar(ADD_TO_WALLET)
        .bind(amount)
        .bind(cu.user.id)
        .fetch_one(&st.pool)
        .await?;
    Ok(Json(json!({ "balance": balance })))
}

pub async fn transfer(
    State(st): State<AppState>,
    cu: CurrentUser,
    JsonBody(b): JsonBody,
) -> ApiResult<Json<Value>> {
    let from = cu.user.id;
    let to = to_int(b.get("to_user_id"), 0);
    let amount = amount_field(&b)?;
    let to = i32::try_from(to).map_err(|_| ApiError::not_found_msg("recipient not found"))?;
    if to == from {
        return Err(ApiError::bad_request("cannot transfer to yourself"));
    }

    let mut tx = st.pool.begin().await?;
    // lock both wallets in id order so opposite transfers cannot deadlock
    let rows: Vec<(i32, Option<f64>)> = sqlx::query_as(
        "SELECT id, wallet_balance FROM users WHERE id = ANY($1) ORDER BY id FOR UPDATE",
    )
    .bind(vec![from, to])
    .fetch_all(&mut *tx)
    .await?;
    if !rows.iter().any(|(id, _)| *id == to) {
        return Err(ApiError::not_found_msg("recipient not found"));
    }
    let src = rows
        .iter()
        .find(|(id, _)| *id == from)
        .and_then(|(_, b)| *b)
        .unwrap_or(0.0);
    if src < amount {
        return Err(ApiError::payment_required("insufficient balance"));
    }
    let new_src: Option<f64> = sqlx::query_scalar(ADD_TO_WALLET)
        .bind(-amount)
        .bind(from)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query(ADD_TO_WALLET)
        .bind(amount)
        .bind(to)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "balance": new_src })))
}
