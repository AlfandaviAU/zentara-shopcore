//! End-to-end tests against the real router and a real Postgres.
//!
//! They need a database: set `TEST_DATABASE_URL` (or `DATABASE_URL`), e.g.
//!   docker compose up -d db
//!   TEST_DATABASE_URL=postgresql://shop:shop@localhost:5432/shop cargo test
//! (or simply `docker compose --profile test run --rm test`).
//! Without a database URL these tests print a notice and pass trivially.
//! A mock bank is started in-process; it fails the first attempt of every
//! charge so the retry/idempotency path is always exercised.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use reqwest::Method;
use serde_json::{json, Value};
use shopcore::{db, manage, router, AppState, Config};
use tokio::sync::OnceCell;

#[derive(Default)]
struct MockBank {
    /// idempotency key -> (bank_ref, attempts)
    keys: HashMap<String, (String, u32)>,
    charges: Vec<Value>,
}

struct Env {
    base: String,
    bank: Arc<Mutex<MockBank>>,
}

static ENV: OnceCell<Option<Env>> = OnceCell::const_new();

async fn env() -> Option<&'static Env> {
    ENV.get_or_init(|| async {
        let url = std::env::var("TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()?;
        let bank = Arc::new(Mutex::new(MockBank::default()));
        let bank_url = start_mock_bank(bank.clone()).await;

        // the server runs on its own runtime thread so it outlives each #[tokio::test] runtime
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let mut cfg = Config::from_env();
                cfg.database_url = url;
                cfg.bank_url = bank_url;
                cfg.export_dir =
                    std::env::temp_dir().join(format!("shopcore-test-{}", std::process::id()));
                cfg.bank_timeout = std::time::Duration::from_secs(2);
                let pool = db::connect(&cfg).await.unwrap();
                db::init_db(&pool).await.unwrap();
                manage::seed(&pool, &cfg).await.unwrap();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(format!("http://{}", listener.local_addr().unwrap()))
                    .unwrap();
                axum::serve(listener, router(AppState::new(cfg, pool)))
                    .await
                    .unwrap();
            });
        });
        let base = rx.recv().unwrap();
        Some(Env { base, bank })
    })
    .await
    .as_ref()
}

async fn start_mock_bank(state: Arc<Mutex<MockBank>>) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let app = Router::new().route(
                "/charge",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let state = state.clone();
                    async move {
                        let key = headers
                            .get("Idempotency-Key")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let mut s = state.lock().unwrap();
                        if key.is_empty() {
                            s.charges.push(body);
                            return (
                                StatusCode::OK,
                                Json(json!({"bank_ref": "BK-nokey", "status": "ok"})),
                            );
                        }
                        let n = s.keys.len();
                        let first = !s.keys.contains_key(&key);
                        if first {
                            s.charges.push(body);
                        }
                        let entry = s.keys.entry(key).or_insert((format!("BK-mock{n:06}"), 0));
                        entry.1 += 1;
                        if entry.1 == 1 {
                            // charge recorded, but the answer is "lost": the client must retry
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error": "gateway timeout"})),
                            );
                        }
                        (
                            StatusCode::OK,
                            Json(json!({"bank_ref": entry.0.clone(), "status": "ok"})),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(format!("http://{}", listener.local_addr().unwrap()))
                .unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    rx.recv().unwrap()
}

macro_rules! env_or_skip {
    () => {
        match env().await {
            Some(e) => e,
            None => {
                eprintln!("skipped: set TEST_DATABASE_URL to run integration tests");
                return;
            }
        }
    };
}

impl Env {
    /// A client per call: a shared pooled client would keep connections bound to
    /// the runtime of whichever #[tokio::test] opened them.
    fn http(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut req = self
            .http()
            .request(method, format!("{}{}", self.base, path));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        let text = res.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
    async fn get(&self, path: &str, token: Option<&str>) -> (u16, Value) {
        self.call(Method::GET, path, token, None).await
    }
    async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        self.call(Method::POST, path, token, Some(body)).await
    }

    async fn user(&self) -> (i64, String) {
        let name = format!("t_{}", shopcore::auth::random_hex(5));
        let (s, b) = self
            .post(
                "/api/register",
                None,
                json!({"username": name, "email": format!("{name}@x.io"), "password": "pw12345"}),
            )
            .await;
        assert_eq!(s, 201, "{b}");
        let (s, t) = self
            .post(
                "/api/login",
                None,
                json!({"username": name, "password": "pw12345"}),
            )
            .await;
        assert_eq!(s, 200, "{t}");
        (
            b["id"].as_i64().unwrap(),
            t["token"].as_str().unwrap().to_string(),
        )
    }

    async fn admin(&self) -> String {
        let (s, t) = self
            .post(
                "/api/login",
                None,
                json!({"username": "admin", "password": "admin123"}),
            )
            .await;
        assert_eq!(s, 200, "{t}");
        t["token"].as_str().unwrap().to_string()
    }

    async fn product(&self, stock: i64, price: f64) -> i64 {
        let adm = self.admin().await;
        let sku = format!("T-{}", shopcore::auth::random_hex(4));
        let (s, p) = self
            .post(
                "/api/products",
                Some(&adm),
                json!({"sku": sku, "name": "Test Item", "price": price, "stock": stock}),
            )
            .await;
        assert_eq!(s, 201, "{p}");
        p["id"].as_i64().unwrap()
    }

    async fn order(&self, token: &str, pid: i64, qty: i64) -> Value {
        let (s, o) = self
            .post(
                "/api/orders",
                Some(token),
                json!({"items": [{"product_id": pid, "quantity": qty}]}),
            )
            .await;
        assert_eq!(s, 201, "{o}");
        o
    }
}

#[tokio::test]
async fn logout_invalidates_token_and_admin_routes_need_a_token() {
    let e = env_or_skip!();
    let (_, tok) = e.user().await;
    assert_eq!(e.get("/api/users/me", Some(&tok)).await.0, 200);
    assert_eq!(e.post("/api/logout", Some(&tok), json!({})).await.0, 200);
    assert_eq!(e.get("/api/users/me", Some(&tok)).await.0, 401);
    // legacy let unauthenticated requests into admin routes
    assert_eq!(e.get("/api/admin/users", None).await.0, 401);
    assert_eq!(
        e.post("/api/products", None, json!({"sku": "x"})).await.0,
        401
    );
    assert_eq!(e.get("/api/reports/sales", None).await.0, 401);
}

#[tokio::test]
async fn profile_update_cannot_escalate_role_or_touch_wallet() {
    let e = env_or_skip!();
    let (_, tok) = e.user().await;
    let (s, u) = e
        .call(
            Method::PATCH,
            "/api/users/me",
            Some(&tok),
            Some(json!({"name": "N", "role": "admin", "wallet_balance": 1e6})),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(u["name"], "N");
    assert_eq!(u["role"], "user");
    assert_eq!(u["wallet_balance"], 0.0);
    let (_, users) = e.get("/api/admin/users", Some(&e.admin().await)).await;
    assert!(users
        .as_array()
        .unwrap()
        .iter()
        .all(|u| u.get("password_hash").is_none()));
}

#[tokio::test]
async fn sql_injection_in_sort_and_group_by_is_rejected() {
    let e = env_or_skip!();
    assert_eq!(
        e.get("/api/products?sort=id;DROP%20TABLE%20users", None)
            .await
            .0,
        400
    );
    let adm = e.admin().await;
    assert_eq!(
        e.get(
            "/api/reports/sales?group_by=1;DELETE%20FROM%20orders",
            Some(&adm)
        )
        .await
        .0,
        400
    );
    assert_eq!(
        e.get("/api/reports/sales?from=nope", Some(&adm)).await.0,
        400
    );
    assert_eq!(e.get("/api/products?sort=price&limit=3", None).await.0, 200);
}

#[tokio::test]
async fn concurrent_orders_never_oversell() {
    let e = env_or_skip!();
    let pid = e.product(5, 10.0).await;
    let (_, tok) = e.user().await;
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let tok = tok.clone();
            tokio::spawn(async move {
                let e = env().await.unwrap();
                e.post(
                    "/api/orders",
                    Some(&tok),
                    json!({"items": [{"product_id": pid, "quantity": 1}]}),
                )
                .await
            })
        })
        .collect();
    let mut ok = 0;
    let mut order_nos = std::collections::HashSet::new();
    for t in tasks {
        let (s, o) = t.await.unwrap();
        if s == 201 {
            ok += 1;
            assert!(
                order_nos.insert(o["order_no"].as_i64().unwrap()),
                "duplicate order_no"
            );
        } else {
            assert_eq!(s, 409, "{o}");
        }
    }
    assert_eq!(ok, 5);
    assert_eq!(
        e.get(&format!("/api/products/{pid}"), None).await.1["stock"],
        0
    );
}

#[tokio::test]
async fn failed_order_rolls_back_reserved_stock() {
    let e = env_or_skip!();
    let pid = e.product(10, 3.0).await;
    let (_, tok) = e.user().await;
    let (s, _) = e
        .post(
            "/api/orders",
            Some(&tok),
            json!({"items": [{"product_id": pid, "quantity": 2}, {"product_id": 99999999}]}),
        )
        .await;
    assert_eq!(s, 404);
    assert_eq!(
        e.get(&format!("/api/products/{pid}"), None).await.1["stock"],
        10
    );
    assert_eq!(
        e.get("/api/orders", Some(&tok))
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let (s, _) = e
        .post(
            "/api/orders",
            Some(&tok),
            json!({"items": [{"product_id": pid, "quantity": -3}]}),
        )
        .await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn concurrent_wallet_payments_charge_once() {
    let e = env_or_skip!();
    let pid = e.product(10, 40.0).await;
    let (_, tok) = e.user().await;
    let o = e.order(&tok, pid, 1).await;
    e.post("/api/wallet/topup", Some(&tok), json!({"amount": 100}))
        .await;
    let path = format!("/api/orders/{}/pay", o["id"]);
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let (tok, path) = (tok.clone(), path.clone());
            tokio::spawn(async move {
                env()
                    .await
                    .unwrap()
                    .post(&path, Some(&tok), json!({"method": "wallet"}))
                    .await
                    .0
            })
        })
        .collect();
    let mut codes = vec![];
    for t in tasks {
        codes.push(t.await.unwrap());
    }
    assert_eq!(codes.iter().filter(|c| **c == 200).count(), 1, "{codes:?}");
    assert_eq!(e.get("/api/wallet", Some(&tok)).await.1["balance"], 60.0);
}

#[tokio::test]
async fn bank_retry_uses_idempotency_key_single_charge() {
    let e = env_or_skip!();
    let pid = e.product(10, 25.0).await;
    let (_, tok) = e.user().await;
    let o = e.order(&tok, pid, 1).await;
    let oid = o["id"].as_i64().unwrap();
    let (s, r) = e
        .post(
            &format!("/api/orders/{oid}/pay"),
            Some(&tok),
            json!({"method": "bank"}),
        )
        .await;
    assert_eq!(s, 200, "{r}");
    assert!(r["bank_ref"].as_str().unwrap().starts_with("BK-"));
    let bank = e.bank.lock().unwrap();
    let charges = bank
        .charges
        .iter()
        .filter(|c| c["order_id"].as_i64() == Some(oid))
        .count();
    assert_eq!(charges, 1, "the retry must not create a second charge");
}

#[tokio::test]
async fn webhook_requires_signature() {
    let e = env_or_skip!();
    let pid = e.product(10, 5.0).await;
    let (_, tok) = e.user().await;
    let o = e.order(&tok, pid, 1).await;
    let body =
        serde_json::to_vec(&json!({"order_id": o["id"], "bank_ref": "BK-x", "status": "settled"}))
            .unwrap();
    let url = format!("{}/api/webhooks/bank", e.base);
    let unsigned = e
        .http()
        .post(&url)
        .body(body.clone())
        .header("Content-Type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status().as_u16(), 401);
    assert_eq!(
        e.get(&format!("/api/orders/{}", o["id"]), Some(&tok))
            .await
            .1["status"],
        "pending"
    );
    let sig = shopcore::routes::webhooks::sign("bank-shared-secret-2019", &body);
    let signed = e
        .http()
        .post(&url)
        .body(body)
        .header("X-Signature", sig)
        .send()
        .await
        .unwrap();
    assert_eq!(signed.status().as_u16(), 200);
    assert_eq!(
        e.get(&format!("/api/orders/{}", o["id"]), Some(&tok))
            .await
            .1["status"],
        "paid"
    );
}

#[tokio::test]
async fn refunds_are_bounded_and_orders_are_private() {
    let e = env_or_skip!();
    let pid = e.product(10, 30.0).await;
    let (_, tok) = e.user().await;
    let (_, other) = e.user().await;
    let o = e.order(&tok, pid, 1).await;
    let oid = o["id"].as_i64().unwrap();
    assert_eq!(
        e.get(&format!("/api/orders/{oid}"), Some(&other)).await.0,
        404
    );
    assert_eq!(
        e.post(
            &format!("/api/orders/{oid}/cancel"),
            Some(&other),
            json!({})
        )
        .await
        .0,
        404
    );
    e.post("/api/wallet/topup", Some(&tok), json!({"amount": 30}))
        .await;
    assert_eq!(
        e.post(&format!("/api/orders/{oid}/pay"), Some(&tok), json!({}))
            .await
            .0,
        200
    );
    assert_eq!(
        e.post(
            &format!("/api/orders/{oid}/refund"),
            Some(&tok),
            json!({"amount": 31})
        )
        .await
        .0,
        400
    );
    let (s, r) = e
        .post(
            &format!("/api/orders/{oid}/refund"),
            Some(&tok),
            json!({"amount": 10}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(r["refunded"], 10.0);
    assert_eq!(
        e.post(&format!("/api/orders/{oid}/refund"), Some(&tok), json!({}))
            .await
            .0,
        200
    );
    assert_eq!(
        e.post(&format!("/api/orders/{oid}/refund"), Some(&tok), json!({}))
            .await
            .0,
        409
    );
    // cancelling after a full refund returns stock but no extra money
    assert_eq!(
        e.post(&format!("/api/orders/{oid}/cancel"), Some(&tok), json!({}))
            .await
            .0,
        200
    );
    assert_eq!(e.get("/api/wallet", Some(&tok)).await.1["balance"], 30.0);
}

#[tokio::test]
async fn wallet_rejects_bad_amounts_and_self_transfer() {
    let e = env_or_skip!();
    let (a_id, a) = e.user().await;
    let (b_id, b) = e.user().await;
    assert_eq!(
        e.post("/api/wallet/topup", Some(&a), json!({"amount": -5}))
            .await
            .0,
        400
    );
    assert_eq!(
        e.post("/api/wallet/topup", Some(&a), json!({"amount": "abc"}))
            .await
            .0,
        400
    );
    e.post("/api/wallet/topup", Some(&a), json!({"amount": 0.1}))
        .await;
    let (_, w) = e
        .post("/api/wallet/topup", Some(&a), json!({"amount": 0.2}))
        .await;
    assert_eq!(w["balance"], 0.3);
    assert_eq!(
        e.post(
            "/api/wallet/transfer",
            Some(&a),
            json!({"to_user_id": a_id, "amount": 0.1})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        e.post(
            "/api/wallet/transfer",
            Some(&a),
            json!({"to_user_id": b_id, "amount": -1})
        )
        .await
        .0,
        400
    );
    assert_eq!(e.get("/api/wallet", Some(&b)).await.1["balance"], 0.0);
}

#[tokio::test]
async fn export_blocks_path_traversal_and_reports_are_fresh() {
    let e = env_or_skip!();
    let adm = e.admin().await;
    assert_eq!(
        e.post(
            "/api/reports/export",
            Some(&adm),
            json!({"filename": "../../etc/x"})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        e.post(
            "/api/reports/export",
            Some(&adm),
            json!({"filename": "a;touch /tmp/pwn"})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        e.get("/api/reports/download?file=../../etc/passwd", Some(&adm))
            .await
            .0,
        404
    );

    let q = "/api/reports/sales?from=2000-01-01&to=2999-12-31&group_by=status";
    let before = e.get(q, Some(&adm)).await.1;
    let count = |r: &Value| -> i64 {
        r["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["bucket"] == "pending")
            .map(|b| b["orders"].as_i64().unwrap())
            .sum()
    };
    let pid = e.product(10, 1.0).await;
    let (_, tok) = e.user().await;
    e.order(&tok, pid, 1).await;
    let after = e.get(q, Some(&adm)).await.1;
    assert!(count(&after) > count(&before), "report must not be cached");
}
