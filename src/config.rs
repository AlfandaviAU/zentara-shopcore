use std::path::PathBuf;
use std::time::Duration;

/// Runtime configuration, read from the environment.
///
/// Every value has a default that matches the legacy service, so the
/// container starts with no extra configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bank_url: String,
    pub bank_secret: String,
    pub admin_user: String,
    pub admin_password: String,
    pub export_dir: PathBuf,
    pub bind_addr: String,
    pub session_ttl_hours: f64,
    pub bank_timeout: Duration,
    pub bank_attempts: u32,
    pub db_pool_size: u32,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        Config {
            database_url: env_or("DATABASE_URL", "postgresql://shop:shop@localhost:5432/shop"),
            bank_url: env_or("BANK_URL", "http://localhost:5001")
                .trim_end_matches('/')
                .to_string(),
            bank_secret: env_or("BANK_SECRET", "bank-shared-secret-2019"),
            admin_user: env_or("ADMIN_USER", "admin"),
            admin_password: env_or("ADMIN_PASSWORD", "admin123"),
            export_dir: PathBuf::from(env_or("EXPORT_DIR", "/tmp/exports")),
            bind_addr: env_or("BIND_ADDR", "0.0.0.0:8000"),
            session_ttl_hours: env_num("SESSION_TTL_HOURS", 24.0),
            bank_timeout: Duration::from_millis(env_num("BANK_TIMEOUT_MS", 15_000)),
            bank_attempts: env_num("BANK_ATTEMPTS", 3u32).max(1),
            db_pool_size: env_num("DB_POOL_SIZE", 64u32).max(2),
        }
    }
}
