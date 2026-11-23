use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use super::JsonBody;
use crate::auth::{self, CurrentUser, Verified};
use crate::db::locks;
use crate::error::{ApiError, ApiResult};
use crate::models::User;
use crate::util::clean;
use crate::AppState;

pub async fn register(
    State(st): State<AppState>,
    JsonBody(b): JsonBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let username = clean(b.get("username"));
    let email = clean(b.get("email"));
    let password = b
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if username.is_empty() || email.is_empty() || password.is_empty() {
        return Err(ApiError::bad_request(
            "username, email and password are required",
        ));
    }
    let name = b
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| username.clone());

    // hash outside the lock: it is the slow part
    let hash = auth::hash_password(password).await?;

    let mut tx = st.pool.begin().await?;
    // serialise registrations so two concurrent requests cannot both take a name
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(locks::USERS)
        .execute(&mut *tx)
        .await?;
    let taken: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE username = $1 LIMIT 1")
        .bind(&username)
        .fetch_optional(&mut *tx)
        .await?;
    if taken.is_some() {
        return Err(ApiError::conflict("username taken"));
    }
    let taken: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE email = $1 LIMIT 1")
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
    if taken.is_some() {
        return Err(ApiError::conflict("email taken"));
    }
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO users (username, email, name, password_hash, role, wallet_balance) \
         VALUES ($1, $2, $3, $4, 'user', 0) RETURNING id",
    )
    .bind(&username)
    .bind(&email)
    .bind(&name)
    .bind(&hash)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "username": username })),
    ))
}

pub async fn login(State(st): State<AppState>, JsonBody(b): JsonBody) -> ApiResult<Json<Value>> {
    let invalid = || ApiError::new(StatusCode::UNAUTHORIZED, "invalid credentials");
    let username = b
        .get("username")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let password = b
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let rows: Vec<(i32, Option<String>)> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE username = $1 ORDER BY id")
            .bind(username)
            .fetch_all(&st.pool)
            .await?;
    for (id, stored) in rows {
        let Some(stored) = stored else { continue };
        match auth::verify_password(password.clone(), stored).await? {
            Verified::No => continue,
            Verified::YesNeedsRehash => {
                // transparently upgrade legacy MD5 hashes
                let new_hash = auth::hash_password(password.clone()).await?;
                sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
                    .bind(new_hash)
                    .bind(id)
                    .execute(&st.pool)
                    .await?;
            }
            Verified::Yes => {}
        }
        let token = auth::create_session(&st.pool, id, st.cfg.session_ttl_hours).await?;
        return Ok(Json(json!({ "token": token })));
    }
    Err(invalid())
}

/// Ends the session: the token stops working (legacy kept it valid).
pub async fn logout(State(st): State<AppState>, cu: CurrentUser) -> ApiResult<Json<Value>> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(&cu.token_hash)
        .execute(&st.pool)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn me(cu: CurrentUser) -> Json<Value> {
    Json(cu.user.public_json())
}

/// Only `name` and `email` can change. Other keys are ignored (legacy wrote any
/// key straight into SQL, so `{"role": "admin"}` made anyone an admin).
pub async fn update_me(
    State(st): State<AppState>,
    cu: CurrentUser,
    JsonBody(b): JsonBody,
) -> ApiResult<Json<Value>> {
    let field = |key: &str| -> ApiResult<Option<String>> {
        match b.get(key) {
            None => Ok(None),
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
            Some(_) => Err(ApiError::bad_request(format!(
                "{key} must be a non-empty string"
            ))),
        }
    };
    let name = field("name")?;
    let email = field("email")?;

    let mut tx = st.pool.begin().await?;
    if let Some(email) = &email {
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(locks::USERS)
            .execute(&mut *tx)
            .await?;
        let taken: Option<i32> =
            sqlx::query_scalar("SELECT id FROM users WHERE email = $1 AND id <> $2 LIMIT 1")
                .bind(email)
                .bind(cu.user.id)
                .fetch_optional(&mut *tx)
                .await?;
        if taken.is_some() {
            return Err(ApiError::conflict("email taken"));
        }
    }
    let user = sqlx::query_as::<_, User>(
        "UPDATE users SET name = COALESCE($1, name), email = COALESCE($2, email) WHERE id = $3 RETURNING *",
    )
    .bind(name)
    .bind(email)
    .bind(cu.user.id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(user.public_json()))
}
