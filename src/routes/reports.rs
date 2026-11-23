use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path as FsPath;

use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::NaiveDateTime;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Value};

use super::JsonBody;
use crate::auth::AdminUser;
use crate::error::{ApiError, ApiResult};
use crate::util::{parse_bound, py_datetime, py_float, round2, valid_file_name};
use crate::AppState;

const DEFAULT_FROM: &str = "1970-01-01";
const DEFAULT_TO: &str = "2999-12-31";

fn bound(raw: Option<&str>, default: &str, name: &str) -> ApiResult<(String, NaiveDateTime)> {
    let s = raw.unwrap_or(default).to_string();
    let dt = parse_bound(&s)
        .ok_or_else(|| ApiError::bad_request(format!("{name} must be a date (YYYY-MM-DD)")))?;
    Ok((s, dt))
}

/// Sales report. Computed fresh on every call: the legacy result cache was
/// never invalidated, so reports went stale as soon as a new order came in.
pub async fn sales(
    State(st): State<AppState>,
    _a: AdminUser,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let (from_s, from) = bound(q.get("from").map(String::as_str), DEFAULT_FROM, "from")?;
    let (to_s, to) = bound(q.get("to").map(String::as_str), DEFAULT_TO, "to")?;
    let group_by = q
        .get("group_by")
        .map(String::as_str)
        .unwrap_or("day")
        .to_string();
    // whitelisted SQL per grouping (legacy pasted group_by into the query)
    let sql = match group_by.as_str() {
        "day" => "SELECT to_char(date(created_at), 'YYYY-MM-DD') AS bucket, COUNT(*) AS orders, SUM(total) AS revenue \
                  FROM orders WHERE created_at >= $1 AND created_at < $2 GROUP BY date(created_at) ORDER BY date(created_at)",
        "status" => "SELECT COALESCE(status, 'None') AS bucket, COUNT(*) AS orders, SUM(total) AS revenue \
                     FROM orders WHERE created_at >= $1 AND created_at < $2 GROUP BY status ORDER BY status",
        "user" => "SELECT COALESCE(user_id::text, 'None') AS bucket, COUNT(*) AS orders, SUM(total) AS revenue \
                   FROM orders WHERE created_at >= $1 AND created_at < $2 GROUP BY user_id ORDER BY user_id",
        _ => return Err(ApiError::bad_request("group_by must be one of day, status, user")),
    };
    let rows: Vec<(Option<String>, i64, Option<f64>)> = sqlx::query_as(sql)
        .bind(from)
        .bind(to)
        .fetch_all(&st.pool)
        .await?;
    let buckets: Vec<Value> = rows
        .into_iter()
        .map(|(bucket, orders, revenue)| json!({ "bucket": bucket, "orders": orders, "revenue": revenue.map(round2) }))
        .collect();

    // one aggregate query instead of the legacy N x M Python loops
    let top: Vec<(Option<String>, Option<f64>)> = sqlx::query_as(
        "SELECT p.name, SUM(i.price * i.quantity) AS revenue \
         FROM orders o JOIN order_items i ON i.order_id = o.id JOIN products p ON p.id = i.product_id \
         WHERE o.created_at >= $1 AND o.created_at < $2 AND o.status IN ('paid', 'shipped') \
         GROUP BY p.name ORDER BY revenue DESC NULLS LAST, p.name LIMIT 10",
    )
    .bind(from)
    .bind(to)
    .fetch_all(&st.pool)
    .await?;
    let top_products: Vec<Value> = top
        .into_iter()
        .map(|(name, revenue)| json!({ "name": name, "revenue": round2(revenue.unwrap_or(0.0)) }))
        .collect();

    Ok(Json(json!({
        "from": from_s,
        "to": to_s,
        "group_by": group_by,
        "buckets": buckets,
        "top_products": top_products,
    })))
}

/// Build the sales-lines CSV (header `order_no,date,status,user,product,qty,price`).
pub async fn sales_csv(
    pool: &sqlx::PgPool,
    from: NaiveDateTime,
    to: NaiveDateTime,
) -> anyhow::Result<Vec<u8>> {
    type Line = (
        Option<i32>,
        Option<NaiveDateTime>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<f64>,
    );
    let rows: Vec<Line> = sqlx::query_as(
        "SELECT o.order_no, o.created_at, o.status, u.username, p.name, i.quantity, i.price \
         FROM orders o JOIN users u ON u.id = o.user_id JOIN order_items i ON i.order_id = o.id \
         JOIN products p ON p.id = i.product_id \
         WHERE o.created_at >= $1 AND o.created_at < $2 ORDER BY o.id, i.id",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record([
        "order_no", "date", "status", "user", "product", "qty", "price",
    ])?;
    for (no, created, status, user, product, qty, price) in rows {
        w.write_record([
            no.map(|n| n.to_string()).unwrap_or_default(),
            created.as_ref().map(py_datetime).unwrap_or_default(),
            status.unwrap_or_default(),
            user.unwrap_or_default(),
            product.unwrap_or_default(),
            qty.map(|n| n.to_string()).unwrap_or_default(),
            price.map(py_float).unwrap_or_default(),
        ])?;
    }
    Ok(w.into_inner()?)
}

/// Write `bytes` to `path` atomically (temp file + rename), so a concurrent
/// download never sees a half-written file.
pub fn write_atomic(path: &FsPath, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", crate::auth::random_hex(6)));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

pub async fn export(
    State(st): State<AppState>,
    _a: AdminUser,
    JsonBody(b): JsonBody,
) -> ApiResult<Json<Value>> {
    let filename = match b.get("filename") {
        None | Some(Value::Null) => "sales.csv".to_string(),
        Some(Value::String(s)) if s.is_empty() => "sales.csv".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(ApiError::bad_request("invalid filename")),
    };
    // no path separators, no shell: legacy ran `gzip -kf <filename>` through a shell
    if !valid_file_name(&filename) {
        return Err(ApiError::bad_request(
            "invalid filename, use letters, digits, '.', '_' and '-'",
        ));
    }
    let (_, from) = bound(b.get("from").and_then(Value::as_str), DEFAULT_FROM, "from")?;
    let (_, to) = bound(b.get("to").and_then(Value::as_str), DEFAULT_TO, "to")?;
    let csv = sales_csv(&st.pool, from, to).await?;

    let dir = st.cfg.export_dir.clone();
    let gz_name = format!("{filename}.gz");
    let gz_name_c = gz_name.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&csv)?;
        let gz = enc.finish()?;
        write_atomic(&dir.join(&filename), &csv)?; // legacy kept the plain CSV too (gzip -k)
        write_atomic(&dir.join(&gz_name_c), &gz)?;
        Ok(())
    })
    .await??;
    Ok(Json(json!({ "file": gz_name })))
}

pub async fn download(
    State(st): State<AppState>,
    _a: AdminUser,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let name = q.get("file").map(String::as_str).unwrap_or("");
    // only plain file names inside the export dir (legacy allowed ../ traversal)
    if !valid_file_name(name) {
        return Err(ApiError::not_found());
    }
    let path = st.cfg.export_dir.join(name);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ApiError::not_found()),
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => return Err(ApiError::not_found()),
        Err(e) => return Err(e.into()),
    };
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}
