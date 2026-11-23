//! Maintenance commands (the legacy `manage.py`).

use std::collections::HashSet;

use anyhow::{bail, Context};
use sqlx::PgPool;

use crate::auth;
use crate::config::Config;
use crate::util::{py_datetime, py_float};

pub const SEED_PRODUCTS: &str = include_str!("../seed/products.csv");

/// Create the admin account, two coupons and the sample products, each only if missing.
pub async fn seed(pool: &PgPool, cfg: &Config) -> anyhow::Result<()> {
    let admin: Option<i32> = sqlx::query_scalar("SELECT id FROM users WHERE username = $1 LIMIT 1")
        .bind(&cfg.admin_user)
        .fetch_optional(pool)
        .await?;
    if admin.is_none() {
        let hash = auth::hash_password(cfg.admin_password.clone()).await?;
        sqlx::query(
            "INSERT INTO users (username, email, name, password_hash, role, wallet_balance) \
             VALUES ($1, 'admin@shopcore.local', 'Administrator', $2, 'admin', 0)",
        )
        .bind(&cfg.admin_user)
        .bind(hash)
        .execute(pool)
        .await?;
    }
    let coupon: Option<i32> = sqlx::query_scalar("SELECT id FROM coupons LIMIT 1")
        .fetch_optional(pool)
        .await?;
    if coupon.is_none() {
        sqlx::query(
            "INSERT INTO coupons (code, percent, expires, single_use) VALUES \
             ('SAVE10', 10, '2099-12-31', FALSE), ('ONCE50', 50, '2099-12-31', TRUE)",
        )
        .execute(pool)
        .await?;
    }
    let product: Option<i32> = sqlx::query_scalar("SELECT id FROM products LIMIT 1")
        .fetch_optional(pool)
        .await?;
    if product.is_none() {
        let n = import_products_csv(pool, SEED_PRODUCTS).await?;
        println!("imported {n} products");
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
pub struct ProductRow {
    pub sku: String,
    pub name: String,
    pub price: f64,
    pub stock: i32,
}

/// Parse a products CSV (`sku,name,price,stock`, header line first).
/// Duplicate SKUs keep the first occurrence, as before. Any invalid line
/// fails the whole import, so a bad file never half-imports.
pub fn parse_products_csv(text: &str) -> anyhow::Result<Vec<ProductRow>> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(text.as_bytes());
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for (i, rec) in rdr.records().enumerate() {
        let line = i + 2;
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                errors.push(format!("line {line}: {e}"));
                continue;
            }
        };
        if rec.iter().all(|f| f.is_empty()) {
            continue;
        }
        let field = |n: usize| rec.get(n).unwrap_or("").to_string();
        let sku = field(0);
        if sku.is_empty() {
            errors.push(format!("line {line}: missing sku"));
            continue;
        }
        let price = match field(2).parse::<f64>() {
            Ok(p) if p.is_finite() && p >= 0.0 => p,
            _ => {
                errors.push(format!("line {line}: invalid price {:?}", field(2)));
                continue;
            }
        };
        let stock = match field(3).parse::<i32>() {
            Ok(s) if s >= 0 => s,
            _ => {
                errors.push(format!("line {line}: invalid stock {:?}", field(3)));
                continue;
            }
        };
        if !seen.insert(sku.clone()) {
            continue;
        }
        out.push(ProductRow {
            sku,
            name: field(1),
            price,
            stock,
        });
    }
    if !errors.is_empty() {
        bail!("invalid products file:\n  {}", errors.join("\n  "));
    }
    Ok(out)
}

/// Insert new SKUs and update existing ones, in one transaction.
/// (Legacy built the INSERT by string formatting and re-inserted existing SKUs.)
pub async fn import_products_csv(pool: &PgPool, text: &str) -> anyhow::Result<usize> {
    let rows = parse_products_csv(text)?;
    let mut tx = pool.begin().await?;
    for r in &rows {
        let updated =
            sqlx::query("UPDATE products SET name = $2, price = $3, stock = $4 WHERE sku = $1")
                .bind(&r.sku)
                .bind(&r.name)
                .bind(r.price)
                .bind(r.stock)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        if updated == 0 {
            sqlx::query("INSERT INTO products (sku, name, price, stock) VALUES ($1, $2, $3, $4)")
                .bind(&r.sku)
                .bind(&r.name)
                .bind(r.price)
                .bind(r.stock)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(rows.len())
}

pub async fn import_products(pool: &PgPool, path: &str) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    let n = import_products_csv(pool, &text).await?;
    println!("imported {n} products");
    Ok(())
}

/// `export-sales <file>`: one line per order (`order_no,status,total,created_at`).
pub async fn export_sales(pool: &PgPool, out: &str) -> anyhow::Result<()> {
    type SaleRow = (
        Option<i32>,
        Option<String>,
        Option<f64>,
        Option<chrono::NaiveDateTime>,
    );
    let rows: Vec<SaleRow> =
        sqlx::query_as("SELECT order_no, status, total, created_at FROM orders ORDER BY id")
            .fetch_all(pool)
            .await?;
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record(["order_no", "status", "total", "created_at"])?;
    for (no, status, total, created) in &rows {
        w.write_record([
            no.map(|n| n.to_string()).unwrap_or_default(),
            status.clone().unwrap_or_default(),
            total.map(py_float).unwrap_or_default(),
            created.as_ref().map(py_datetime).unwrap_or_default(),
        ])?;
    }
    std::fs::write(out, w.into_inner()?).with_context(|| format!("cannot write {out}"))?;
    println!("wrote {out} ({} rows)", rows.len());
    Ok(())
}

/// Log everybody out. Works now that sessions live in the database.
pub async fn purge_sessions(pool: &PgPool) -> anyhow::Result<u64> {
    Ok(sqlx::query("DELETE FROM sessions")
        .execute(pool)
        .await?
        .rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_file_parses() {
        let rows = parse_products_csv(SEED_PRODUCTS).unwrap();
        assert_eq!(rows.len(), 10);
        assert_eq!(rows[0].sku, "SKU-1001");
        assert_eq!(rows[0].price, 89.99);
    }

    #[test]
    fn duplicates_keep_first_and_quotes_work() {
        let rows = parse_products_csv(
            "sku,name,price,stock\nA,\"Desk, large\",10,5\nA,Other,1,1\nB,Lamp,2.5,3\n",
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "Desk, large");
        assert_eq!(rows[1].sku, "B");
    }

    #[test]
    fn invalid_rows_fail_the_import() {
        assert!(parse_products_csv("sku,name,price,stock\nA,x,abc,5\n").is_err());
        assert!(parse_products_csv("sku,name,price,stock\nA,x,1,-5\n").is_err());
        assert!(parse_products_csv("sku,name,price,stock\n,x,1,5\n").is_err());
    }
}
