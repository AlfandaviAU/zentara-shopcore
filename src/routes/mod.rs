use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use tower_http::catch_panic::CatchPanicLayer;

use crate::error::ApiError;
use crate::AppState;

pub mod admin;
pub mod orders;
pub mod payments;
pub mod products;
pub mod reports;
pub mod users;
pub mod wallet;
pub mod webhooks;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/register", post(users::register))
        .route("/api/login", post(users::login))
        .route("/api/logout", post(users::logout))
        .route("/api/users/me", get(users::me).patch(users::update_me))
        .route("/api/products", get(products::list).post(products::create))
        .route(
            "/api/products/{id}",
            get(products::get_one).put(products::update),
        )
        .route("/api/orders", get(orders::list).post(orders::create))
        .route("/api/orders/{id}", get(orders::get_one))
        .route("/api/orders/{id}/invoice", get(orders::invoice))
        .route("/api/orders/{id}/pay", post(payments::pay))
        .route("/api/orders/{id}/cancel", post(orders::cancel))
        .route("/api/orders/{id}/refund", post(orders::refund))
        .route("/api/wallet", get(wallet::get_wallet))
        .route("/api/wallet/topup", post(wallet::topup))
        .route("/api/wallet/transfer", post(wallet::transfer))
        .route("/api/reports/sales", get(reports::sales))
        .route("/api/reports/export", post(reports::export))
        .route("/api/reports/download", get(reports::download))
        .route("/api/admin/users", get(admin::list_users))
        .route("/api/admin/users/{id}/role", post(admin::set_role))
        .route(
            "/api/admin/orders/{id}/status",
            post(admin::set_order_status),
        )
        .route("/api/webhooks/bank", post(webhooks::bank))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn(access_log))
        .layer(CatchPanicLayer::custom(|_| {
            ApiError::internal().into_response()
        }))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn not_found() -> ApiError {
    ApiError::not_found()
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

/// Request log without bodies (the legacy audit log kept every body,
/// passwords included, in an ever-growing in-memory list).
async fn access_log(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let res = next.run(req).await;
    tracing::info!(target: "shopcore::http", %method, path, status = res.status().as_u16(), ms = start.elapsed().as_millis() as u64);
    res
}

/// Lenient JSON body, like Flask's `get_json(silent=True) or {}`:
/// a missing or malformed body, or a non-object, becomes an empty object.
pub struct JsonBody(pub Map<String, Value>);

impl<S: Send + Sync> FromRequest<S> for JsonBody {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|_| ApiError::bad_request("could not read request body"))?;
        let map = match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        Ok(JsonBody(map))
    }
}

pub fn path_id(raw: &str) -> Result<i32, ApiError> {
    crate::util::parse_path_id(raw).ok_or_else(ApiError::not_found)
}

/// Shared money SQL: round to cents in numeric to avoid float drift.
pub const ADD_TO_WALLET: &str = "UPDATE users SET wallet_balance = round((COALESCE(wallet_balance, 0) + $1::float8)::numeric, 2)::float8 \
     WHERE id = $2 RETURNING wallet_balance";
