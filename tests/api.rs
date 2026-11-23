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
    db_url: String,
    export_dir: std::path::PathBuf,
    /// bodies received by the notify_url test receiver
    hooks: Arc<Mutex<Vec<Value>>>,
    hook_url: String,
}

static ENV: OnceCell<Option<Env>> = OnceCell::const_new();

async fn env() -> Option<&'static Env> {
    ENV.get_or_init(|| async {
        let url = std::env::var("TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()?;
        let bank = Arc::new(Mutex::new(MockBank::default()));
        let bank_url = start_mock_bank(bank.clone()).await;
        let hooks = Arc::new(Mutex::new(Vec::new()));
        let hook_url = start_hook_receiver(hooks.clone());
        let db_url = url.clone();
        let export_dir = std::env::temp_dir().join(format!("shopcore-test-{}", std::process::id()));
        let export_dir_srv = export_dir.clone();

        // the server runs on its own runtime thread so it outlives each #[tokio::test] runtime
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let mut cfg = Config::from_env();
                cfg.database_url = url;
                cfg.bank_url = bank_url;
                cfg.export_dir = export_dir_srv;
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
        Some(Env {
            base,
            bank,
            db_url,
            export_dir,
            hooks,
            hook_url,
        })
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
                        if body["amount"].as_f64() == Some(13.13) {
                            // a card the bank declines outright
                            return (StatusCode::BAD_REQUEST, Json(json!({"error": "declined"})));
                        }
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

fn start_hook_receiver(hooks: Arc<Mutex<Vec<Value>>>) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let app = Router::new().route(
                "/hook",
                post(move |Json(body): Json<Value>| {
                    let hooks = hooks.clone();
                    async move {
                        hooks.lock().unwrap().push(body);
                        StatusCode::OK
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(format!("http://{}/hook", listener.local_addr().unwrap()))
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
    /// A fresh small pool per test (pools are bound to the runtime that made them).
    async fn pool(&self) -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&self.db_url)
            .await
            .unwrap()
    }

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
    async fn post_to(&self, id: i64, action: &str, token: &str, body: Value) -> (u16, Value) {
        self.post(&format!("/api/orders/{id}/{action}"), Some(token), body)
            .await
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

// ---------------------------------------------------------------------------
// Coverage of the remaining paths: every route's error branches, background
// notifications, reports/exports, coupons, legacy password upgrade.
// ---------------------------------------------------------------------------

async fn wait_for_hooks(e: &Env, order_id: i64, n: usize) -> Vec<String> {
    for _ in 0..60 {
        let got: Vec<String> = e
            .hooks
            .lock()
            .unwrap()
            .iter()
            .filter(|h| h["order_id"].as_i64() == Some(order_id))
            .map(|h| h["status"].as_str().unwrap_or("").to_string())
            .collect();
        if got.len() >= n {
            return got;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    vec![]
}

#[tokio::test]
async fn admin_role_and_status_routes_validate_input() {
    let e = env_or_skip!();
    let adm = e.admin().await;
    let (uid, tok) = e.user().await;
    assert_eq!(e.get("/api/admin/users", Some(&tok)).await.0, 403);
    assert_eq!(
        e.post(
            &format!("/api/admin/users/{uid}/role"),
            Some(&tok),
            json!({"role": "admin"})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        e.post(
            &format!("/api/admin/users/{uid}/role"),
            Some(&adm),
            json!({"role": "god"})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        e.post(
            "/api/admin/users/999999999/role",
            Some(&adm),
            json!({"role": "user"})
        )
        .await
        .0,
        404
    );
    assert_eq!(
        e.post("/api/admin/users/abc/role", Some(&adm), json!({}))
            .await
            .0,
        404
    );
    let (s, r) = e
        .post(
            &format!("/api/admin/users/{uid}/role"),
            Some(&adm),
            json!({"role": "admin"}),
        )
        .await;
    assert_eq!((s, r["role"].as_str()), (200, Some("admin")));
    let (s, r) = e
        .post(
            &format!("/api/admin/users/{uid}/role"),
            Some(&adm),
            json!({}),
        )
        .await;
    assert_eq!(
        (s, r["role"].as_str()),
        (200, Some("user")),
        "role defaults to user"
    );

    let (_, users) = e.get("/api/admin/users", Some(&adm)).await;
    let me = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"].as_i64() == Some(uid))
        .unwrap()
        .clone();
    assert!(me["created_at"].as_str().unwrap().ends_with("GMT"));

    let pid = e.product(5, 2.0).await;
    let o = e.order(&tok, pid, 1).await;
    let path = format!("/api/admin/orders/{}/status", o["id"]);
    assert_eq!(
        e.post(&path, Some(&adm), json!({"status": "lost"})).await.0,
        400
    );
    assert_eq!(e.post(&path, Some(&adm), json!({})).await.0, 400);
    assert_eq!(
        e.post(
            "/api/admin/orders/999999999/status",
            Some(&adm),
            json!({"status": "paid"})
        )
        .await
        .0,
        404
    );
    let (s, r) = e
        .post(&path, Some(&adm), json!({"status": "shipped"}))
        .await;
    assert_eq!((s, r["status"].as_str()), (200, Some("shipped")));
    assert_eq!(r["order_no"], o["order_no"]);
    // a shipped-but-never-paid order refunds nothing (legacy credited the total)
    assert_eq!(
        e.post(
            &format!("/api/orders/{}/refund", o["id"]),
            Some(&tok),
            json!({})
        )
        .await
        .0,
        409
    );
}

#[tokio::test]
async fn notify_url_receives_pending_then_paid() {
    let e = env_or_skip!();
    let pid = e.product(5, 3.0).await;
    let (_, tok) = e.user().await;
    let (s, _) = e
        .post(
            "/api/orders",
            Some(&tok),
            json!({"items": [{"product_id": pid}], "notify_url": "ftp://x"}),
        )
        .await;
    assert_eq!(s, 400);
    let (s, _) = e
        .post(
            "/api/orders",
            Some(&tok),
            json!({"items": [{"product_id": pid}], "notify_url": 5}),
        )
        .await;
    assert_eq!(s, 400);

    let (s, o) = e
        .post(
            "/api/orders",
            Some(&tok),
            json!({"items": [{"product_id": pid}], "notify_url": e.hook_url}),
        )
        .await;
    assert_eq!(s, 201, "{o}");
    e.post("/api/wallet/topup", Some(&tok), json!({"amount": 10}))
        .await;
    assert_eq!(
        e.post(
            &format!("/api/orders/{}/pay", o["id"]),
            Some(&tok),
            json!({})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        wait_for_hooks(e, o["id"].as_i64().unwrap(), 2).await,
        vec!["pending", "paid"]
    );

    // the settlement webhook also notifies when it moves an order to paid
    let (_, o2) = e
        .post(
            "/api/orders",
            Some(&tok),
            json!({"items": [{"product_id": pid}], "notify_url": e.hook_url}),
        )
        .await;
    let body =
        serde_json::to_vec(&json!({"order_id": o2["id"], "bank_ref": "BK-n", "status": "settled"}))
            .unwrap();
    let sig = shopcore::routes::webhooks::sign("bank-shared-secret-2019", &body);
    let r = e
        .http()
        .post(format!("{}/api/webhooks/bank", e.base))
        .header("X-Signature", sig)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(
        wait_for_hooks(e, o2["id"].as_i64().unwrap(), 2).await,
        vec!["pending", "paid"]
    );
    let (_, got) = e
        .get(&format!("/api/orders/{}", o2["id"]), Some(&tok))
        .await;
    assert_eq!(
        got["paid_amount"], got["total"],
        "webhook records the paid amount"
    );
}

#[tokio::test]
async fn reports_group_by_and_export_contents() {
    let e = env_or_skip!();
    let adm = e.admin().await;
    let (_, tok) = e.user().await;
    let pid = e.product(5, 4.5).await;
    let o = e.order(&tok, pid, 2).await;
    e.post("/api/wallet/topup", Some(&tok), json!({"amount": 100}))
        .await;
    e.post(
        &format!("/api/orders/{}/pay", o["id"]),
        Some(&tok),
        json!({}),
    )
    .await;

    let (s, day) = e.get("/api/reports/sales?group_by=day", Some(&adm)).await;
    assert_eq!(s, 200);
    assert_eq!(day["from"], "1970-01-01");
    let b = day["buckets"][0]["bucket"].as_str().unwrap();
    assert!(b.len() == 10 && b.as_bytes()[4] == b'-', "{b}");
    let (s, by_user) = e
        .get(
            "/api/reports/sales?group_by=user&from=2000-01-01T00:00:00&to=2999-01-01",
            Some(&adm),
        )
        .await;
    assert_eq!(s, 200);
    let uid = o["user_id"].as_i64().unwrap().to_string();
    assert!(by_user["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["bucket"] == uid.as_str()));
    assert!(by_user["top_products"].as_array().unwrap().len() <= 10);
    assert_eq!(
        e.get("/api/reports/sales?to=not-a-date", Some(&adm))
            .await
            .0,
        400
    );

    assert_eq!(
        e.post("/api/reports/export", Some(&tok), json!({})).await.0,
        403
    );
    assert_eq!(
        e.post("/api/reports/export", Some(&adm), json!({"filename": 7}))
            .await
            .0,
        400
    );
    assert_eq!(
        e.post("/api/reports/export", Some(&adm), json!({"from": "bad"}))
            .await
            .0,
        400
    );
    let (s, r) = e
        .post("/api/reports/export", Some(&adm), json!({"filename": ""}))
        .await;
    assert_eq!(
        (s, r["file"].as_str()),
        (200, Some("sales.csv.gz")),
        "default name"
    );
    let name = format!("cov_{}.csv", shopcore::auth::random_hex(4));
    let (s, r) = e
        .post(
            "/api/reports/export",
            Some(&adm),
            json!({"filename": name, "from": "2000-01-01"}),
        )
        .await;
    assert_eq!(s, 200, "{r}");
    assert!(e.export_dir.join(&name).exists(), "plain CSV is kept too");

    let res = e
        .http()
        .get(format!("{}/api/reports/download?file={name}.gz", e.base))
        .bearer_auth(&adm)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.headers()["content-type"], "application/gzip");
    let gz = res.bytes().await.unwrap();
    let mut csv = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&gz[..]), &mut csv).unwrap();
    assert!(csv.starts_with("order_no,date,status,user,product,qty,price\n"));
    assert!(
        csv.contains(&format!("{},", o["order_no"])),
        "order line present"
    );
    assert!(
        csv.contains(",4.5\n"),
        "price written like Python's str(float)"
    );
    assert_eq!(e.get("/api/reports/download", Some(&adm)).await.0, 404);
    assert_eq!(
        e.get("/api/reports/download?file=nope.gz", Some(&adm))
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn wallet_transfer_paths() {
    let e = env_or_skip!();
    let (_, a) = e.user().await;
    let (b_id, b) = e.user().await;
    assert_eq!(e.get("/api/wallet", Some(&a)).await.1["balance"], 0.0);
    assert_eq!(
        e.post("/api/wallet/topup", Some(&a), json!({"amount": "100"}))
            .await
            .1["balance"],
        100.0
    );
    assert_eq!(
        e.post("/api/wallet/topup", Some(&a), json!({"amount": 2e9}))
            .await
            .0,
        400
    );
    assert_eq!(
        e.post("/api/wallet/topup", Some(&a), json!({})).await.0,
        400
    );
    let (s, r) = e
        .post(
            "/api/wallet/transfer",
            Some(&a),
            json!({"to_user_id": b_id, "amount": 40}),
        )
        .await;
    assert_eq!((s, r["balance"].as_f64()), (200, Some(60.0)));
    assert_eq!(e.get("/api/wallet", Some(&b)).await.1["balance"], 40.0);
    assert_eq!(
        e.post(
            "/api/wallet/transfer",
            Some(&a),
            json!({"to_user_id": b_id, "amount": 61})
        )
        .await
        .0,
        402
    );
    assert_eq!(
        e.post(
            "/api/wallet/transfer",
            Some(&a),
            json!({"to_user_id": "abc", "amount": 1})
        )
        .await
        .0,
        404
    );
    assert_eq!(
        e.post(
            "/api/wallet/transfer",
            Some(&a),
            json!({"to_user_id": 99999999999i64, "amount": 1})
        )
        .await
        .0,
        404
    );
    assert_eq!(
        e.post(
            "/api/wallet/transfer",
            None,
            json!({"to_user_id": b_id, "amount": 1})
        )
        .await
        .0,
        401
    );
}

#[tokio::test]
async fn product_admin_validation_and_listing() {
    let e = env_or_skip!();
    let adm = e.admin().await;
    for bad in [
        json!({"price": -1}),
        json!({"price": "abc"}),
        json!({"stock": "x"}),
        json!({"stock": -1}),
        json!({"stock": 3e10}),
    ] {
        assert_eq!(
            e.post("/api/products", Some(&adm), bad.clone()).await.0,
            400,
            "{bad}"
        );
    }
    let (s, p) = e
        .post(
            "/api/products",
            Some(&adm),
            json!({"sku": 123, "name": null}),
        )
        .await;
    assert_eq!(s, 201);
    assert_eq!(
        (p["sku"].as_str(), p["price"].as_f64(), p["stock"].as_i64()),
        (Some("123"), Some(0.0), Some(0))
    );
    let id = p["id"].as_i64().unwrap();
    let path = format!("/api/products/{id}");
    let (s, u) = e
        .call(
            Method::PUT,
            &path,
            Some(&adm),
            Some(json!({"name": "Renamed", "stock": "7"})),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        (u["name"].as_str(), u["stock"].as_i64(), u["sku"].as_str()),
        (Some("Renamed"), Some(7), Some("123"))
    );
    assert_eq!(
        e.call(Method::PUT, &path, Some(&adm), Some(json!({"price": "x"})))
            .await
            .0,
        400
    );
    assert_eq!(
        e.call(
            Method::PUT,
            "/api/products/999999999",
            Some(&adm),
            Some(json!({}))
        )
        .await
        .0,
        404
    );
    assert_eq!(
        e.call(Method::PUT, &path, None, Some(json!({}))).await.0,
        401
    );
    assert_eq!(e.get("/api/products/abc", None).await.0, 404);

    let (_, all) = e.get("/api/products?limit=-1", None).await;
    let (_, page) = e.get("/api/products?limit=2&page=0", None).await;
    assert!(all.as_array().unwrap().len() > 2);
    assert_eq!(
        page.as_array().unwrap().len(),
        2,
        "page < 1 is treated as page 1"
    );
    let (_, wild) = e.get("/api/products?q=%25", None).await;
    assert!(
        wild.as_array().unwrap().is_empty(),
        "% is matched literally"
    );
    let (_, by_name) = e.get("/api/products?sort=name&limit=-1", None).await;
    let names: Vec<String> = by_name
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(!names.is_empty());
}

#[tokio::test]
async fn order_edge_paths_and_coupons() {
    let e = env_or_skip!();
    let pool = e.pool().await;
    let (_, tok) = e.user().await;
    let pid = e.product(10, 10.0).await;
    let order = |body: Value| e.post("/api/orders", Some(&tok), body);

    assert_eq!(order(json!({"items": {"product_id": pid}})).await.0, 400);
    assert_eq!(order(json!({"items": [5]})).await.0, 400);
    assert_eq!(
        order(json!({"items": [{"product_id": pid, "quantity": 1e12}]}))
            .await
            .0,
        400
    );
    assert_eq!(
        order(json!({"items": [{"product_id": pid}], "coupon": 5}))
            .await
            .0,
        400
    );
    assert_eq!(
        order(json!({"items": [{"product_id": pid}], "coupon": "NOPE"}))
            .await
            .0,
        400
    );
    let (s, r) = order(json!({"items": [{"product_id": pid, "quantity": 11}]})).await;
    assert_eq!(s, 409);
    assert!(r["error"]
        .as_str()
        .unwrap()
        .starts_with("insufficient stock"));

    let (s, o) =
        order(json!({"items": [{"product_id": pid, "quantity": 2}], "coupon": "SAVE10"})).await;
    assert_eq!((s, o["total"].as_f64()), (201, Some(18.0)));
    let (s, o) = order(json!({"items": [{"product_id": pid, "quantity": 1}, {"product_id": pid, "quantity": "2"}], "coupon": ""})).await;
    assert_eq!(s, 201);
    assert_eq!(
        (
            o["items"].as_array().unwrap().len(),
            o["total"].as_f64(),
            o["coupon"].clone()
        ),
        (2, Some(30.0), Value::Null)
    );
    assert_eq!(
        e.get(&format!("/api/products/{pid}"), None).await.1["stock"],
        5
    );

    // expired and single-use coupons (inserted directly; there is no coupon API)
    let tag = shopcore::auth::random_hex(4).to_uppercase();
    sqlx::query("INSERT INTO coupons (code, percent, expires, single_use) VALUES ($1, 10, '2000-01-01', FALSE), ($2, 50, '2099-12-31', TRUE)")
        .bind(format!("OLD{tag}"))
        .bind(format!("ONE{tag}"))
        .execute(&pool)
        .await
        .unwrap();
    let (s, r) =
        order(json!({"items": [{"product_id": pid}], "coupon": format!("OLD{tag}")})).await;
    assert_eq!((s, r["error"].as_str()), (400, Some("coupon expired")));
    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let (tok, code) = (tok.clone(), format!("ONE{tag}"));
            tokio::spawn(async move {
                env()
                    .await
                    .unwrap()
                    .post(
                        "/api/orders",
                        Some(&tok),
                        json!({"items": [{"product_id": pid}], "coupon": code}),
                    )
                    .await
                    .0
            })
        })
        .collect();
    let mut codes = vec![];
    for t in tasks {
        codes.push(t.await.unwrap());
    }
    codes.sort();
    assert_eq!(
        codes,
        vec![201, 409, 409, 409, 409],
        "a single-use coupon works exactly once"
    );

    // list pagination, newest first
    let (_, all) = e.get("/api/orders?limit=-1", Some(&tok)).await;
    let n = all.as_array().unwrap().len();
    assert_eq!(n, 3);
    assert!(all[0]["id"].as_i64() > all[1]["id"].as_i64());
    assert_eq!(
        e.get("/api/orders?limit=2&page=2", Some(&tok))
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // invoice: owner and admin yes, others no
    let oid = all[0]["id"].as_i64().unwrap();
    let (s, inv) = e
        .get(&format!("/api/orders/{oid}/invoice"), Some(&tok))
        .await;
    assert_eq!(s, 200);
    assert_eq!(
        inv["lines"][0]["line_total"],
        50.0 * 0.0
            + inv["lines"][0]["price"].as_f64().unwrap()
                * inv["lines"][0]["quantity"].as_f64().unwrap()
    );
    assert!(inv["customer"]["email"].as_str().unwrap().contains('@'));
    let (_, other) = e.user().await;
    assert_eq!(
        e.get(&format!("/api/orders/{oid}/invoice"), Some(&other))
            .await
            .0,
        404
    );
    assert_eq!(
        e.get(&format!("/api/orders/{oid}"), Some(&e.admin().await))
            .await
            .0,
        200
    );
    assert_eq!(
        e.get("/api/orders/999999999/invoice", Some(&tok)).await.0,
        404
    );

    // pay errors
    assert_eq!(
        e.post_to(oid, "pay", &tok, json!({"method": "card"}))
            .await
            .0,
        400
    );
    assert_eq!(
        e.post_to(oid, "pay", &tok, json!({"method": 1})).await.0,
        400
    );
    assert_eq!(
        e.post(&format!("/api/orders/{oid}/pay"), Some(&other), json!({}))
            .await
            .0,
        404
    );
    let declined = e.product(5, 13.13).await;
    let d = e.order(&tok, declined, 1).await;
    let did = d["id"].as_i64().unwrap();
    assert_eq!(
        e.post_to(did, "pay", &tok, json!({"method": "bank"}))
            .await
            .0,
        502,
        "bank declines"
    );
    assert_eq!(
        e.get(&format!("/api/orders/{did}"), Some(&tok)).await.1["status"],
        "pending"
    );

    // refund / cancel errors
    assert_eq!(
        e.post_to(oid, "refund", &tok, json!({})).await.0,
        409,
        "pending"
    );
    e.post("/api/wallet/topup", Some(&tok), json!({"amount": 100}))
        .await;
    assert_eq!(
        e.post_to(oid, "pay", &tok, json!({"method": "wallet"}))
            .await
            .0,
        200
    );
    assert_eq!(
        e.post_to(oid, "pay", &tok, json!({"method": "wallet"}))
            .await
            .0,
        409
    );
    assert_eq!(
        e.post_to(oid, "refund", &tok, json!({"amount": "abc"}))
            .await
            .0,
        400
    );
    assert_eq!(
        e.post_to(oid, "refund", &tok, json!({"amount": -5}))
            .await
            .0,
        400
    );
    let before = e.get("/api/wallet", Some(&tok)).await.1["balance"]
        .as_f64()
        .unwrap();
    assert_eq!(
        e.post(&format!("/api/orders/{did}/cancel"), Some(&tok), json!({}))
            .await
            .0,
        200
    );
    assert_eq!(
        e.get("/api/wallet", Some(&tok)).await.1["balance"]
            .as_f64()
            .unwrap(),
        before,
        "unpaid cancel credits nothing"
    );
    assert_eq!(
        e.post_to(did, "refund", &tok, json!({})).await.0,
        409,
        "cancelled"
    );
    assert_eq!(
        e.post("/api/orders/999999999/cancel", Some(&tok), json!({}))
            .await
            .0,
        404
    );
    assert_eq!(
        e.post("/api/orders/999999999/refund", Some(&tok), json!({}))
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn webhook_edge_cases() {
    let e = env_or_skip!();
    let url = format!("{}/api/webhooks/bank", e.base);
    let send = |body: Vec<u8>| {
        let sig = shopcore::routes::webhooks::sign("bank-shared-secret-2019", &body);
        e.http()
            .post(&url)
            .header("X-Signature", sig)
            .body(body)
            .send()
    };
    assert_eq!(
        send(b"not json".to_vec()).await.unwrap().status().as_u16(),
        400
    );
    assert_eq!(
        send(br#"{"status":"failed","order_id":1}"#.to_vec())
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    assert_eq!(
        send(br#"{"status":"settled","order_id":99999999999}"#.to_vec())
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    assert_eq!(
        send(br#"{"status":"settled","order_id":999999999}"#.to_vec())
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    let bad = e
        .http()
        .post(&url)
        .header("X-Signature", "zz-not-hex")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status().as_u16(), 401);
}

#[tokio::test]
async fn users_edge_cases_and_legacy_password_upgrade() {
    let e = env_or_skip!();
    let pool = e.pool().await;
    assert_eq!(
        e.post(
            "/api/register",
            None,
            json!({"username": "x", "email": "y"})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        e.post(
            "/api/register",
            None,
            json!({"username": "x", "email": "y", "password": 5})
        )
        .await
        .0,
        400
    );
    let name = format!("e_{}", shopcore::auth::random_hex(5));
    let (s, r) = e
        .post(
            "/api/register",
            None,
            json!({"username": name, "email": format!("{name}@x.io"), "password": "pw", "name": 5}),
        )
        .await;
    assert_eq!(s, 201, "{r}");
    let (s, _) = e.post("/api/register", None, json!({"username": format!("{name}2"), "email": format!("{name}@x.io"), "password": "pw"})).await;
    assert_eq!(s, 409, "email taken");
    assert_eq!(
        e.post("/api/login", None, json!({"username": 5, "password": "pw"}))
            .await
            .0,
        401
    );
    assert_eq!(e.post("/api/login", None, json!({})).await.0, 401);
    let (_, t) = e
        .post(
            "/api/login",
            None,
            json!({"username": name, "password": "pw"}),
        )
        .await;
    let tok = t["token"].as_str().unwrap().to_string();
    let (_, me) = e.get("/api/users/me", Some(&tok)).await;
    assert_eq!(
        me["name"].as_str(),
        Some(name.as_str()),
        "non-string name defaults to the username"
    );

    let patch = |body: Value| e.call(Method::PATCH, "/api/users/me", Some(&tok), Some(body));
    assert_eq!(patch(json!({"name": ""})).await.0, 400);
    assert_eq!(patch(json!({"email": 5})).await.0, 400);
    assert_eq!(patch(json!({"email": "admin@shopcore.local"})).await.0, 409);
    let (s, u) = patch(json!({"email": format!("new_{name}@x.io")})).await;
    assert_eq!(
        (s, u["email"].as_str()),
        (200, Some(format!("new_{name}@x.io").as_str()))
    );

    // a user created by the legacy service (unsalted MD5) can still log in,
    // and the stored hash is upgraded to Argon2id on that login
    let legacy = format!("legacy_{}", shopcore::auth::random_hex(4));
    sqlx::query("INSERT INTO users (username, email, name, password_hash, role, wallet_balance) VALUES ($1, $2, $1, $3, 'user', 0)")
        .bind(&legacy)
        .bind(format!("{legacy}@x.io"))
        .bind(shopcore::auth::legacy_md5("oldpass"))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        e.post(
            "/api/login",
            None,
            json!({"username": legacy, "password": "wrong"})
        )
        .await
        .0,
        401
    );
    assert_eq!(
        e.post(
            "/api/login",
            None,
            json!({"username": legacy, "password": "oldpass"})
        )
        .await
        .0,
        200
    );
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE username = $1")
        .bind(&legacy)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2id$"));
    assert_eq!(
        e.post(
            "/api/login",
            None,
            json!({"username": legacy, "password": "oldpass"})
        )
        .await
        .0,
        200
    );

    // auth header shapes, unknown routes and methods: always JSON errors
    let raw = |auth: &'static str| {
        e.http()
            .get(format!("{}/api/users/me", e.base))
            .header("Authorization", auth)
            .send()
    };
    assert_eq!(raw("Bearer").await.unwrap().status().as_u16(), 401);
    assert_eq!(raw("Bearer nope").await.unwrap().status().as_u16(), 401);
    let (s, b) = e.get("/api/does-not-exist", None).await;
    assert_eq!((s, b["error"].as_str()), (404, Some("not found")));
    let (s, b) = e.call(Method::DELETE, "/api/products", None, None).await;
    assert_eq!((s, b["error"].as_str()), (405, Some("method not allowed")));
    assert_eq!(e.get("/health", None).await.1["status"], "ok");
}
