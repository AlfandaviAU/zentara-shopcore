use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{Map, Value};
use sqlx::{Postgres, QueryBuilder};

use super::{path_id, JsonBody};
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult};
use crate::models::Product;
use crate::util::{escape_like, opt_text, parse_money, query_int, to_int};
use crate::AppState;

/// Sort columns allowed by the spec (legacy concatenated `sort` into SQL).
fn sort_column(s: &str) -> Option<&'static str> {
    Some(match s {
        "" | "id" => "id",
        "name" => "name",
        "price" => "price",
        "stock" => "stock",
        "sku" => "sku",
        _ => return None,
    })
}

pub async fn list(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let col = sort_column(q.get("sort").map(String::as_str).unwrap_or("id")).ok_or_else(|| {
        ApiError::bad_request("invalid sort, use one of id, name, price, stock, sku")
    })?;
    let page = query_int(q.get("page"), 1).max(1);
    let limit = query_int(q.get("limit"), 20);

    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("SELECT * FROM products");
    if let Some(term) = q.get("q").filter(|s| !s.is_empty()) {
        qb.push(" WHERE name ILIKE ")
            .push_bind(format!("%{}%", escape_like(term)));
    }
    qb.push(" ORDER BY ").push(col).push(", id");
    if limit >= 0 {
        qb.push(" LIMIT ").push_bind(limit);
        qb.push(" OFFSET ")
            .push_bind((page - 1).saturating_mul(limit));
    }
    let rows: Vec<Product> = qb.build_query_as().fetch_all(&st.pool).await?;
    Ok(Json(Value::Array(rows.iter().map(Product::json).collect())))
}

pub async fn get_one(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let id = path_id(&id)?;
    let p = sqlx::query_as::<_, Product>("SELECT * FROM products WHERE id = $1")
        .bind(id)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(p.json()))
}

fn price_field(b: &Map<String, Value>, default: f64) -> ApiResult<f64> {
    let price = match b.get("price") {
        None | Some(Value::Null) => default,
        Some(v) => parse_money(v).ok_or_else(|| ApiError::bad_request("price must be a number"))?,
    };
    if price < 0.0 {
        return Err(ApiError::bad_request("price must not be negative"));
    }
    Ok(price)
}

fn stock_field(b: &Map<String, Value>, default: i32) -> ApiResult<i32> {
    let stock = match b.get("stock") {
        None | Some(Value::Null) => default as i64,
        Some(v) => to_int(Some(v), i64::MIN),
    };
    if stock == i64::MIN {
        return Err(ApiError::bad_request("stock must be an integer"));
    }
    if stock < 0 {
        return Err(ApiError::bad_request("stock must not be negative"));
    }
    i32::try_from(stock).map_err(|_| ApiError::bad_request("stock is too large"))
}

pub async fn create(
    State(st): State<AppState>,
    _admin: AdminUser,
    JsonBody(b): JsonBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let price = price_field(&b, 0.0)?;
    let stock = stock_field(&b, 0)?;
    let p = sqlx::query_as::<_, Product>(
        "INSERT INTO products (sku, name, price, stock) VALUES ($1, $2, $3, $4) RETURNING *",
    )
    .bind(opt_text(b.get("sku")))
    .bind(opt_text(b.get("name")))
    .bind(price)
    .bind(stock)
    .fetch_one(&st.pool)
    .await?;
    Ok((StatusCode::CREATED, Json(p.json())))
}

/// Omitted fields keep their value.
pub async fn update(
    State(st): State<AppState>,
    Path(id): Path<String>,
    _admin: AdminUser,
    JsonBody(b): JsonBody,
) -> ApiResult<Json<Value>> {
    let id = path_id(&id)?;
    let mut tx = st.pool.begin().await?;
    let cur = sqlx::query_as::<_, Product>("SELECT * FROM products WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let sku = if b.contains_key("sku") {
        opt_text(b.get("sku"))
    } else {
        cur.sku.clone()
    };
    let name = if b.contains_key("name") {
        opt_text(b.get("name"))
    } else {
        cur.name.clone()
    };
    let price = price_field(&b, cur.price.unwrap_or(0.0))?;
    let stock = stock_field(&b, cur.stock.unwrap_or(0))?;
    let p = sqlx::query_as::<_, Product>(
        "UPDATE products SET sku = $1, name = $2, price = $3, stock = $4 WHERE id = $5 RETURNING *",
    )
    .bind(sku)
    .bind(name)
    .bind(price)
    .bind(stock)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(p.json()))
}
