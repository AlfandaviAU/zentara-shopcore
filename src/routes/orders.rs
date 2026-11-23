use std::collections::{BTreeMap, HashMap};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use sqlx::{PgExecutor, PgPool};

use super::{path_id, JsonBody, ADD_TO_WALLET};
use crate::auth::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::models::{Coupon, ItemRow, Order, User};
use crate::util::{calc_total, parse_money, query_int, round2, to_int};
use crate::AppState;

const ITEMS_SQL: &str = "SELECT i.order_id, i.product_id, i.quantity, i.price, p.sku, p.name \
     FROM order_items i LEFT JOIN products p ON p.id = i.product_id \
     WHERE i.order_id = ANY($1) ORDER BY i.id";

/// Load the items of many orders with one query (legacy did 1 + N queries per order).
pub async fn items_for<'e>(
    db: impl PgExecutor<'e>,
    ids: &[i32],
) -> sqlx::Result<HashMap<i32, Vec<ItemRow>>> {
    let rows: Vec<ItemRow> = sqlx::query_as(ITEMS_SQL).bind(ids).fetch_all(db).await?;
    let mut map: HashMap<i32, Vec<ItemRow>> = HashMap::new();
    for r in rows {
        map.entry(r.order_id.unwrap_or(0)).or_default().push(r);
    }
    Ok(map)
}

pub async fn order_json(pool: &PgPool, o: &Order) -> sqlx::Result<Value> {
    let mut items = items_for(pool, &[o.id]).await?;
    let items = items.remove(&o.id).unwrap_or_default();
    Ok(o.json(Some(items.iter().map(ItemRow::json).collect())))
}

/// Owners can see and act on their orders, admins on every order.
/// Anyone else gets 404 (legacy let any user read, cancel or refund any order).
fn can_access(user: &User, o: &Order) -> bool {
    user.is_admin() || o.user_id == Some(user.id)
}

async fn find_order(pool: &PgPool, user: &User, id: i32) -> ApiResult<Order> {
    let o = sqlx::query_as::<_, Order>("SELECT * FROM orders WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .filter(|o| can_access(user, o))
        .ok_or_else(ApiError::not_found)?;
    Ok(o)
}

struct LineReq {
    product_id: i64,
    quantity: i64,
}

pub async fn create(
    State(st): State<AppState>,
    cu: CurrentUser,
    JsonBody(b): JsonBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let user = cu.user;
    let items = match b.get("items") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return Err(ApiError::bad_request("items required")),
    };
    let mut lines = Vec::with_capacity(items.len());
    for it in items {
        let Value::Object(it) = it else {
            return Err(ApiError::bad_request("each item must be an object"));
        };
        let quantity = to_int(it.get("quantity"), 1);
        if quantity < 1 {
            return Err(ApiError::bad_request("quantity must be at least 1"));
        }
        lines.push(LineReq {
            product_id: to_int(it.get("product_id"), 0),
            quantity,
        });
    }
    let coupon_code = match b.get("coupon") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(ApiError::bad_request("invalid coupon")),
    };
    let notify_url = match b.get("notify_url") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::String(s)) => {
            let ok = reqwest::Url::parse(s)
                .map(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
                .unwrap_or(false);
            if !ok {
                return Err(ApiError::bad_request("notify_url must be an http(s) URL"));
            }
            Some(s.clone())
        }
        Some(_) => return Err(ApiError::bad_request("notify_url must be an http(s) URL")),
    };

    // One transaction for the whole order: any failure leaves no half-created
    // order and no reserved stock behind (legacy left both).
    let mut tx = st.pool.begin().await?;

    let mut coupon: Option<Coupon> = None;
    if let Some(code) = &coupon_code {
        // plain read: a popular multi-use coupon must not serialise checkouts
        let c = sqlx::query_as::<_, Coupon>(
            "SELECT * FROM coupons WHERE code = $1 ORDER BY id LIMIT 1",
        )
        .bind(code)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::bad_request("invalid coupon"))?;
        let today = chrono::Utc::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        if c.expires.as_deref().is_some_and(|e| e < today.as_str()) {
            return Err(ApiError::bad_request("coupon expired"));
        }
        if c.single_use.unwrap_or(false) {
            // atomic claim: exactly one order can ever use a single-use coupon
            let claimed = sqlx::query(
                "UPDATE coupons SET used_count = COALESCE(used_count, 0) + 1 \
                 WHERE id = $1 AND COALESCE(used_count, 0) = 0",
            )
            .bind(c.id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if claimed == 0 {
                return Err(ApiError::conflict("coupon already used"));
            }
        }
        coupon = Some(c);
    }

    // order_no comes from a sequence (migration 002): unique without serialising orders
    let (oid, order_no): (i32, i32) = sqlx::query_as(
        "INSERT INTO orders (order_no, user_id, status, total, paid_amount, refunded, coupon, notify_url, created_at) \
         VALUES (nextval('orders_order_no_seq')::int, $1, 'pending', 0, 0, 0, $2, $3, (now() AT TIME ZONE 'UTC')) \
         RETURNING id, order_no",
    )
    .bind(user.id)
    .bind(&coupon_code)
    .bind(&notify_url)
    .fetch_one(&mut *tx)
    .await?;

    // Reserve stock per product in ascending id order, so concurrent orders
    // touching the same products always lock rows in the same order (no deadlocks).
    let mut wanted: BTreeMap<i32, i32> = BTreeMap::new();
    for line in &lines {
        let pid = i32::try_from(line.product_id).map_err(|_| {
            ApiError::not_found_msg(format!("product {} not found", line.product_id))
        })?;
        let qty = i32::try_from(line.quantity)
            .map_err(|_| ApiError::bad_request("quantity is too large"))?;
        let total_qty = wanted
            .get(&pid)
            .copied()
            .unwrap_or(0)
            .checked_add(qty)
            .ok_or_else(|| ApiError::bad_request("quantity is too large"))?;
        wanted.insert(pid, total_qty);
    }
    let mut prices: HashMap<i32, f64> = HashMap::with_capacity(wanted.len());
    for (&pid, &qty) in &wanted {
        // atomic reserve: never oversells, even under concurrent orders
        let reserved: Option<(Option<f64>,)> = sqlx::query_as(
            "UPDATE products SET stock = stock - $1 WHERE id = $2 AND stock >= $1 RETURNING price",
        )
        .bind(qty)
        .bind(pid)
        .fetch_optional(&mut *tx)
        .await?;
        match reserved {
            Some((price,)) => {
                prices.insert(pid, price.unwrap_or(0.0));
            }
            None => {
                let sku: Option<(Option<String>,)> =
                    sqlx::query_as("SELECT sku FROM products WHERE id = $1")
                        .bind(pid)
                        .fetch_optional(&mut *tx)
                        .await?;
                return Err(match sku {
                    None => ApiError::not_found_msg(format!("product {pid} not found")),
                    Some((sku,)) => ApiError::conflict(format!(
                        "insufficient stock for {}",
                        sku.unwrap_or_default()
                    )),
                });
            }
        }
    }

    // order lines keep the request order; one multi-row INSERT
    let mut subtotal = 0.0f64;
    let (mut item_pids, mut item_qtys, mut item_prices) = (vec![], vec![], vec![]);
    for line in &lines {
        // both conversions were validated above
        let pid = line.product_id as i32;
        let qty = line.quantity as i32;
        let price = prices.get(&pid).copied().unwrap_or(0.0);
        subtotal += price * qty as f64;
        item_pids.push(pid);
        item_qtys.push(qty);
        item_prices.push(price);
    }
    sqlx::query(
        "INSERT INTO order_items (order_id, product_id, quantity, price) \
         SELECT $1, * FROM UNNEST($2::int[], $3::int[], $4::float8[])",
    )
    .bind(oid)
    .bind(&item_pids)
    .bind(&item_qtys)
    .bind(&item_prices)
    .execute(&mut *tx)
    .await?;

    let pct = coupon.as_ref().and_then(|c| c.percent).unwrap_or(0);
    let total = calc_total(subtotal, pct);
    let order =
        sqlx::query_as::<_, Order>("UPDATE orders SET total = $1 WHERE id = $2 RETURNING *")
            .bind(total)
            .bind(oid)
            .fetch_one(&mut *tx)
            .await?;
    if let Some(c) = coupon.as_ref().filter(|c| !c.single_use.unwrap_or(false)) {
        // last statement before commit, so the row lock is held only briefly
        sqlx::query("UPDATE coupons SET used_count = COALESCE(used_count, 0) + 1 WHERE id = $1")
            .bind(c.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    st.notifier.email(
        user.email.as_deref(),
        &format!("Order {order_no} received"),
        &format!("We received your order totalling {total}."),
    );
    if let Some(url) = &notify_url {
        st.notifier.notify(
            oid,
            url.clone(),
            json!({ "order_id": oid, "order_no": order_no, "status": "pending" }),
        );
    }
    Ok((
        StatusCode::CREATED,
        Json(order_json(&st.pool, &order).await?),
    ))
}

pub async fn list(
    State(st): State<AppState>,
    cu: CurrentUser,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let page = query_int(q.get("page"), 1).max(1);
    let limit = query_int(q.get("limit"), 20);
    let orders: Vec<Order> = if limit >= 0 {
        sqlx::query_as(
            "SELECT * FROM orders WHERE user_id = $1 ORDER BY id DESC LIMIT $2 OFFSET $3",
        )
        .bind(cu.user.id)
        .bind(limit)
        .bind((page - 1).saturating_mul(limit))
        .fetch_all(&st.pool)
        .await?
    } else {
        sqlx::query_as("SELECT * FROM orders WHERE user_id = $1 ORDER BY id DESC")
            .bind(cu.user.id)
            .fetch_all(&st.pool)
            .await?
    };
    let ids: Vec<i32> = orders.iter().map(|o| o.id).collect();
    let mut items = items_for(&st.pool, &ids).await?;
    let out = orders
        .iter()
        .map(|o| {
            let its = items.remove(&o.id).unwrap_or_default();
            o.json(Some(its.iter().map(ItemRow::json).collect()))
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

pub async fn get_one(
    State(st): State<AppState>,
    Path(id): Path<String>,
    cu: CurrentUser,
) -> ApiResult<Json<Value>> {
    let o = find_order(&st.pool, &cu.user, path_id(&id)?).await?;
    Ok(Json(order_json(&st.pool, &o).await?))
}

pub async fn invoice(
    State(st): State<AppState>,
    Path(id): Path<String>,
    cu: CurrentUser,
) -> ApiResult<Json<Value>> {
    let o = find_order(&st.pool, &cu.user, path_id(&id)?).await?;
    let customer: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT name, email FROM users WHERE id = $1")
            .bind(o.user_id)
            .fetch_optional(&st.pool)
            .await?;
    let (name, email) = customer.unwrap_or((None, None));
    let mut items = items_for(&st.pool, &[o.id]).await?;
    let lines: Vec<Value> = items
        .remove(&o.id)
        .unwrap_or_default()
        .iter()
        .map(|it| {
            let line_total = round2(it.price.unwrap_or(0.0) * it.quantity.unwrap_or(0) as f64);
            json!({ "sku": it.sku, "name": it.name, "quantity": it.quantity, "price": it.price, "line_total": line_total })
        })
        .collect();
    Ok(Json(json!({
        "order_no": o.order_no,
        "customer": { "name": name, "email": email },
        "lines": lines,
        "total": o.total,
        "status": o.status,
    })))
}

async fn lock_order(tx: &mut sqlx::PgConnection, user: &User, id: i32) -> ApiResult<Order> {
    let o = sqlx::query_as::<_, Order>("SELECT * FROM orders WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .filter(|o| can_access(user, o))
        .ok_or_else(ApiError::not_found)?;
    Ok(o)
}

/// Money actually received for an order and not yet returned.
fn refundable(o: &Order) -> f64 {
    round2(o.paid_amount.unwrap_or(0.0) - o.refunded.unwrap_or(0.0)).max(0.0)
}

pub async fn cancel(
    State(st): State<AppState>,
    Path(id): Path<String>,
    cu: CurrentUser,
) -> ApiResult<Json<Value>> {
    let id = path_id(&id)?;
    let mut tx = st.pool.begin().await?;
    let o = lock_order(&mut tx, &cu.user, id).await?;
    if o.status() == "cancelled" {
        return Err(ApiError::conflict("already cancelled"));
    }
    // lock affected products in id order first (same order as checkout: no deadlocks)
    sqlx::query(
        "SELECT p.id FROM products p WHERE p.id IN (SELECT product_id FROM order_items WHERE order_id = $1) \
         ORDER BY p.id FOR UPDATE",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE products p SET stock = COALESCE(p.stock, 0) + s.qty \
         FROM (SELECT product_id, SUM(quantity)::int AS qty FROM order_items WHERE order_id = $1 GROUP BY product_id) s \
         WHERE p.id = s.product_id",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    // credit what was really paid and not refunded yet (legacy credited the
    // full total, even for orders that were never paid or already refunded)
    if matches!(o.status(), "paid" | "shipped" | "refunded") {
        let credit = refundable(&o);
        if credit > 0.0 {
            if let Some(uid) = o.user_id {
                sqlx::query(ADD_TO_WALLET)
                    .bind(credit)
                    .bind(uid)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    let updated = sqlx::query_as::<_, Order>(
        "UPDATE orders SET status = 'cancelled' WHERE id = $1 RETURNING *",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let owner_email: Option<String> = sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
        .bind(o.user_id)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
    tx.commit().await?;
    st.notifier.email(
        owner_email.as_deref(),
        &format!("Order {} cancelled", o.order_no.unwrap_or_default()),
        "Your order was cancelled.",
    );
    Ok(Json(order_json(&st.pool, &updated).await?))
}

pub async fn refund(
    State(st): State<AppState>,
    Path(id): Path<String>,
    cu: CurrentUser,
    JsonBody(b): JsonBody,
) -> ApiResult<Json<Value>> {
    let id = path_id(&id)?;
    let mut tx = st.pool.begin().await?;
    let o = lock_order(&mut tx, &cu.user, id).await?;
    match o.status() {
        "pending" => return Err(ApiError::conflict("order not paid")),
        "cancelled" => return Err(ApiError::conflict("order is cancelled")),
        _ => {}
    }
    let remaining = refundable(&o);
    if remaining <= 0.0 {
        return Err(ApiError::conflict("nothing left to refund"));
    }
    let amount = match b.get("amount") {
        None | Some(Value::Null) => remaining,
        Some(v) => {
            round2(parse_money(v).ok_or_else(|| ApiError::bad_request("amount must be a number"))?)
        }
    };
    if amount <= 0.0 {
        return Err(ApiError::bad_request("amount must be positive"));
    }
    if amount > remaining {
        return Err(ApiError::bad_request(format!(
            "amount exceeds the refundable {remaining}"
        )));
    }
    if let Some(uid) = o.user_id {
        sqlx::query(ADD_TO_WALLET)
            .bind(amount)
            .bind(uid)
            .execute(&mut *tx)
            .await?;
    }
    let updated = sqlx::query_as::<_, Order>(
        "UPDATE orders SET status = 'refunded', \
         refunded = round((COALESCE(refunded, 0) + $1::float8)::numeric, 2)::float8 WHERE id = $2 RETURNING *",
    )
    .bind(amount)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(order_json(&st.pool, &updated).await?))
}
