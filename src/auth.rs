//! Passwords, sessions and the request extractors for authenticated routes.

use std::sync::LazyLock;

use anyhow::anyhow;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use md5::{Digest as _, Md5};
use rand::RngCore;
use sha2::Sha256;
use sqlx::PgPool;
use tokio::sync::Semaphore;

use crate::error::ApiError;
use crate::models::User;
use crate::AppState;

/// Bulkhead for password hashing. Argon2id is deliberately expensive (19 MiB of
/// memory and ~10-30 ms of CPU per call). Without a bound, a login burst spawns
/// hundreds of blocking hashes that exhaust memory and starve the async
/// runtime and Postgres. Capping concurrency at half the CPU count keeps throughput at
/// the hardware maximum, keeps memory flat, and lets every other endpoint stay fast.
static HASH_PERMITS: LazyLock<Semaphore> = LazyLock::new(|| {
    // half the cores: the other half keeps serving requests (and Postgres) during a login storm
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
    Semaphore::new((cpus / 2).max(2))
});

async fn run_hash<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> anyhow::Result<T> {
    let _permit = HASH_PERMITS.acquire().await?;
    Ok(tokio::task::spawn_blocking(f).await?)
}

/// Hash a password with Argon2id (the legacy service used unsalted MD5).
pub async fn hash_password(password: String) -> anyhow::Result<String> {
    run_hash(move || {
        let mut salt = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        let salt = SaltString::encode_b64(&salt).map_err(|e| anyhow!("salt: {e}"))?;
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| anyhow!("hash: {e}"))
    })
    .await?
}

/// Outcome of checking a password against a stored hash.
#[derive(Debug, PartialEq, Eq)]
pub enum Verified {
    No,
    Yes,
    /// Matched a legacy MD5 hash; the caller should store a new Argon2 hash.
    YesNeedsRehash,
}

pub fn legacy_md5(password: &str) -> String {
    hex::encode(Md5::digest(password.as_bytes()))
}

pub fn verify_password_sync(password: &str, stored: &str) -> Verified {
    if stored.starts_with("$argon2") {
        let ok = PasswordHash::new(stored)
            .map(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            })
            .unwrap_or(false);
        return if ok { Verified::Yes } else { Verified::No };
    }
    if stored.len() == 32
        && ct_eq(
            legacy_md5(password).as_bytes(),
            stored.to_ascii_lowercase().as_bytes(),
        )
    {
        return Verified::YesNeedsRehash;
    }
    Verified::No
}

pub async fn verify_password(password: String, stored: String) -> anyhow::Result<Verified> {
    run_hash(move || verify_password_sync(&password, &stored)).await
}

/// Constant-time byte comparison.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Create a session and return the bearer token. Only the token's SHA-256 is stored.
pub async fn create_session(pool: &PgPool, user_id: i32, ttl_hours: f64) -> anyhow::Result<String> {
    let token = random_hex(32);
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, expires_at) \
         VALUES ($1, $2, now() + ($3::float8 * interval '1 hour'))",
    )
    .bind(sha256_hex(&token))
    .bind(user_id)
    .bind(ttl_hours)
    .execute(pool)
    .await?;
    Ok(token)
}

/// The authenticated caller. Rejects with 401 when the token is missing or invalid.
pub struct CurrentUser {
    pub user: User,
    pub token_hash: String,
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // Legacy accepted "<anything> <token>"; we keep that.
        let token = header
            .split_once(' ')
            .map(|(_, t)| t.trim())
            .filter(|t| !t.is_empty())
            .ok_or_else(ApiError::unauthorized)?;
        let token_hash = sha256_hex(token);
        let user = sqlx::query_as::<_, User>(
            "SELECT u.* FROM sessions s JOIN users u ON u.id = s.user_id \
             WHERE s.token_hash = $1 AND s.expires_at > now()",
        )
        .bind(&token_hash)
        .fetch_optional(&st.pool)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
        Ok(CurrentUser { user, token_hash })
    }
}

/// An authenticated admin: 401 without a valid token, 403 for non-admins.
/// (The legacy decorator let unauthenticated requests through; fixed.)
pub struct AdminUser(pub User);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, Self::Rejection> {
        let cu = CurrentUser::from_request_parts(parts, st).await?;
        if !cu.user.is_admin() {
            return Err(ApiError::forbidden());
        }
        Ok(AdminUser(cu.user))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_md5_hashes_still_verify() {
        // md5("admin123") as stored by the legacy seed
        let stored = "0192023a7bbd73250516f069df18b500";
        assert_eq!(legacy_md5("admin123"), stored);
        assert_eq!(
            verify_password_sync("admin123", stored),
            Verified::YesNeedsRehash
        );
        assert_eq!(verify_password_sync("wrong", stored), Verified::No);
    }

    #[tokio::test]
    async fn argon2_round_trip() {
        let h = hash_password("pw12345".into()).await.unwrap();
        assert!(h.starts_with("$argon2"));
        assert_eq!(verify_password_sync("pw12345", &h), Verified::Yes);
        assert_eq!(verify_password_sync("pw1234", &h), Verified::No);
    }

    #[test]
    fn tokens_are_random() {
        let a = random_hex(32);
        assert_eq!(a.len(), 64);
        assert_ne!(a, random_hex(32));
    }
}
