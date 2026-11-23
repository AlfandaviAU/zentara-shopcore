use chrono::NaiveDateTime;
use serde_json::{json, Value};

use crate::util::http_date;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i32,
    pub username: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub password_hash: Option<String>,
    pub role: Option<String>,
    pub wallet_balance: Option<f64>,
    pub created_at: Option<NaiveDateTime>,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.role.as_deref() == Some("admin")
    }

    pub fn public_json(&self) -> Value {
        json!({
            "id": self.id,
            "username": self.username,
            "email": self.email,
            "name": self.name,
            "role": self.role,
            "wallet_balance": self.wallet_balance,
        })
    }

    /// Admin listing. Unlike the legacy service it never includes `password_hash`.
    pub fn admin_json(&self) -> Value {
        let mut v = self.public_json();
        v["created_at"] = json!(self.created_at.as_ref().map(http_date));
        v
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Product {
    pub id: i32,
    pub sku: Option<String>,
    pub name: Option<String>,
    pub price: Option<f64>,
    pub stock: Option<i32>,
    pub created_at: Option<NaiveDateTime>,
}

impl Product {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "sku": self.sku,
            "name": self.name,
            "price": self.price,
            "stock": self.stock,
            "created_at": self.created_at.as_ref().map(http_date),
        })
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Order {
    pub id: i32,
    pub order_no: Option<i32>,
    pub user_id: Option<i32>,
    pub status: Option<String>,
    pub total: Option<f64>,
    pub paid_amount: Option<f64>,
    pub refunded: Option<f64>,
    pub coupon: Option<String>,
    pub notify_url: Option<String>,
    pub created_at: Option<NaiveDateTime>,
}

impl Order {
    pub fn status(&self) -> &str {
        self.status.as_deref().unwrap_or("")
    }
    pub fn total(&self) -> f64 {
        self.total.unwrap_or(0.0)
    }

    pub fn json(&self, items: Option<Vec<Value>>) -> Value {
        let mut v = json!({
            "id": self.id,
            "order_no": self.order_no,
            "user_id": self.user_id,
            "status": self.status,
            "total": self.total,
            "paid_amount": self.paid_amount,
            "refunded": self.refunded,
            "coupon": self.coupon,
            "created_at": self.created_at.as_ref().map(http_date),
        });
        if let Some(items) = items {
            v["items"] = Value::Array(items);
        }
        v
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Coupon {
    pub id: i32,
    pub code: Option<String>,
    pub percent: Option<i32>,
    pub expires: Option<String>,
    pub single_use: Option<bool>,
    pub used_count: Option<i32>,
}

/// One order line joined with its product, as the API reports it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ItemRow {
    pub order_id: Option<i32>,
    pub product_id: Option<i32>,
    pub quantity: Option<i32>,
    pub price: Option<f64>,
    pub sku: Option<String>,
    pub name: Option<String>,
}

impl ItemRow {
    pub fn json(&self) -> Value {
        json!({
            "product_id": self.product_id,
            "sku": self.sku,
            "name": self.name,
            "quantity": self.quantity,
            "price": self.price,
        })
    }
}
