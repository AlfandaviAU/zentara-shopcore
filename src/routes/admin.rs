use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};

use super::{path_id, JsonBody};
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult};
use crate::models::User;
use crate::AppState;

pub const ORDER_STATUSES: [&str; 5] = ["pending", "paid", "shipped", "cancelled", "refunded"];

/// Admin user list. `password_hash` is no longer exposed.
pub async fn list_users(State(st): State<AppState>, _a: AdminUser) -> ApiResult<Json<Value>> {
    let users: Vec<User> = sqlx::query_as("SELECT * FROM users ORDER BY id")
        .fetch_all(&st.pool)
        .await?;
    Ok(Json(Value::Array(
        users.iter().map(User::admin_json).collect(),
    )))
}

pub async fn set_role(
    State(st): State<AppState>,
    Path(id): Path<String>,
    _a: AdminUser,
    JsonBody(b): JsonBody,
) -> ApiResult<Json<Value>> {
    let id = path_id(&id)?;
    let role = match b.get("role") {
        None => "user",
        Some(Value::String(s)) if s == "user" || s == "admin" => s.as_str(),
        Some(_) => return Err(ApiError::bad_request("role must be user or admin")),
    };
    let row: Option<(i32, Option<String>, Option<String>)> =
        sqlx::query_as("UPDATE users SET role = $1 WHERE id = $2 RETURNING id, username, role")
            .bind(role)
            .bind(id)
            .fetch_optional(&st.pool)
            .await?;
    let (id, username, role) = row.ok_or_else(ApiError::not_found)?;
    Ok(Json(
        json!({ "id": id, "username": username, "role": role }),
    ))
}

pub async fn set_order_status(
    State(st): State<AppState>,
    Path(id): Path<String>,
    _a: AdminUser,
    JsonBody(b): JsonBody,
) -> ApiResult<Json<Value>> {
    let id = path_id(&id)?;
    let status = b
        .get("status")
        .and_then(Value::as_str)
        .filter(|s| ORDER_STATUSES.contains(s))
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "status must be one of {}",
                ORDER_STATUSES.join(", ")
            ))
        })?;
    let row: Option<(i32, Option<i32>, Option<String>)> = sqlx::query_as(
        "UPDATE orders SET status = $1 WHERE id = $2 RETURNING id, order_no, status",
    )
    .bind(status)
    .bind(id)
    .fetch_optional(&st.pool)
    .await?;
    let (id, order_no, status) = row.ok_or_else(ApiError::not_found)?;
    Ok(Json(
        json!({ "id": id, "order_no": order_no, "status": status }),
    ))
}
