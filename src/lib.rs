//! ShopCore: order, wallet and reporting backend.
//!
//! The binary (`src/main.rs`) exposes the HTTP server and the maintenance
//! commands that used to live in `manage.py`. Everything else lives in this
//! library so integration tests can start the real router in-process.

pub mod auth;
pub mod bank;
pub mod config;
pub mod db;
pub mod error;
pub mod manage;
pub mod models;
pub mod notify;
pub mod routes;
pub mod util;

use std::sync::Arc;

use sqlx::PgPool;

pub use config::Config;
pub use routes::router;

/// Shared state handed to every request handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cfg: Arc<Config>,
    pub http: reqwest::Client,
    pub notifier: notify::Notifier,
}

impl AppState {
    pub fn new(cfg: Config, pool: PgPool) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("http client");
        AppState {
            pool,
            notifier: notify::Notifier::new(http.clone()),
            cfg: Arc::new(cfg),
            http,
        }
    }
}
