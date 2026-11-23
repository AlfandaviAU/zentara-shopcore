use std::time::Duration;

use anyhow::Context;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::config::Config;

/// The legacy schema, unchanged (`shopcore/db.py`).
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id SERIAL PRIMARY KEY,
    username TEXT,
    email TEXT,
    name TEXT,
    password_hash TEXT,
    role TEXT DEFAULT 'user',
    wallet_balance DOUBLE PRECISION DEFAULT 0,
    created_at TIMESTAMP DEFAULT now()
);
CREATE TABLE IF NOT EXISTS products (
    id SERIAL PRIMARY KEY,
    sku TEXT,
    name TEXT,
    price DOUBLE PRECISION,
    stock INTEGER,
    created_at TIMESTAMP DEFAULT now()
);
CREATE TABLE IF NOT EXISTS coupons (
    id SERIAL PRIMARY KEY,
    code TEXT,
    percent INTEGER,
    expires TEXT,
    single_use BOOLEAN DEFAULT FALSE,
    used_count INTEGER DEFAULT 0
);
CREATE TABLE IF NOT EXISTS orders (
    id SERIAL PRIMARY KEY,
    order_no INTEGER,
    user_id INTEGER,
    status TEXT,
    total DOUBLE PRECISION,
    paid_amount DOUBLE PRECISION DEFAULT 0,
    refunded DOUBLE PRECISION DEFAULT 0,
    coupon TEXT,
    notify_url TEXT,
    created_at TIMESTAMP
);
CREATE TABLE IF NOT EXISTS order_items (
    id SERIAL PRIMARY KEY,
    order_id INTEGER,
    product_id INTEGER,
    quantity INTEGER,
    price DOUBLE PRECISION
);
CREATE TABLE IF NOT EXISTS payments (
    id SERIAL PRIMARY KEY,
    order_id INTEGER,
    amount DOUBLE PRECISION,
    method TEXT,
    bank_ref TEXT,
    status TEXT,
    created_at TIMESTAMP DEFAULT now()
);
"#;

/// Additive migration 001 (see README): sessions move from process memory to
/// the database, plus lookup indexes. Nothing existing is altered or dropped.
pub const MIGRATION_001: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_user_id_idx ON sessions (user_id);
CREATE INDEX IF NOT EXISTS users_username_idx ON users (username);
CREATE INDEX IF NOT EXISTS users_email_idx ON users (email);
CREATE INDEX IF NOT EXISTS products_sku_idx ON products (sku);
CREATE INDEX IF NOT EXISTS coupons_code_idx ON coupons (code);
CREATE INDEX IF NOT EXISTS orders_user_id_idx ON orders (user_id);
CREATE INDEX IF NOT EXISTS orders_created_at_idx ON orders (created_at);
CREATE INDEX IF NOT EXISTS order_items_order_id_idx ON order_items (order_id);
CREATE INDEX IF NOT EXISTS payments_order_id_idx ON payments (order_id);
"#;

/// Additive migration 002: `order_no` from a sequence instead of a locked
/// `MAX(order_no) + 1`, so orders are created in parallel. The sequence is
/// (re)aligned to the current maximum on every `init-db`, so numbering
/// continues from existing data (first order on an empty table is 1001).
pub const MIGRATION_002: &str = r#"
CREATE SEQUENCE IF NOT EXISTS orders_order_no_seq;
SELECT setval(
    'orders_order_no_seq',
    GREATEST(
        COALESCE((SELECT MAX(order_no) FROM orders), 1000),
        (SELECT CASE WHEN is_called THEN last_value ELSE 1000 END FROM orders_order_no_seq)
    ),
    true
);
"#;

/// Advisory lock ids used to serialise the few operations that need it.
pub mod locks {
    pub const SCHEMA: i64 = 7_100_000;
    pub const USERS: i64 = 7_100_001;
}

/// Connect, retrying for up to ~60s so the service survives a slow database start.
pub async fn connect(cfg: &Config) -> anyhow::Result<PgPool> {
    let mut last_err = None;
    for attempt in 0..60 {
        match PgPoolOptions::new()
            .max_connections(cfg.db_pool_size)
            .acquire_timeout(Duration::from_secs(30))
            .connect(&cfg.database_url)
            .await
        {
            Ok(pool) => return Ok(pool),
            Err(e) => {
                if attempt % 10 == 0 {
                    tracing::warn!(error = %e, "database not ready, retrying");
                }
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Err(last_err
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("no attempts made")))
    .context("could not connect to the database")
}

/// Create the legacy tables (if missing) and apply the additive migration.
pub async fn init_db(pool: &PgPool) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(locks::SCHEMA)
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql(SCHEMA)
        .execute(&mut *tx)
        .await
        .context("legacy schema")?;
    sqlx::raw_sql(MIGRATION_001)
        .execute(&mut *tx)
        .await
        .context("migration 001")?;
    sqlx::raw_sql(MIGRATION_002)
        .execute(&mut *tx)
        .await
        .context("migration 002")?;
    tx.commit().await?;
    Ok(())
}
