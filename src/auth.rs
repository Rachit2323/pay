use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{error::AppError, AppState};

/// Keys are random and long, so a plain SHA-256 hash is enough.
pub fn hash_key(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

/// Saves a new API key. Only the hash is stored.
pub async fn create_api_key(pool: &PgPool, business_id: Uuid, key: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO api_keys (id, business_id, key_hash) VALUES ($1, $2, $3)
         ON CONFLICT (key_hash) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(business_id)
    .bind(hash_key(key))
    .execute(pool)
    .await
    .map(|_| ())
}

/// Creates a demo business and key for local runs.
pub async fn seed_dev_key(pool: &PgPool, key: &str) -> sqlx::Result<()> {
    let business_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("valid uuid literal");
    sqlx::query(
        "INSERT INTO businesses (id, name) VALUES ($1, 'Dev Business') ON CONFLICT (id) DO NOTHING",
    )
    .bind(business_id)
    .execute(pool)
    .await?;
    create_api_key(pool, business_id, key).await
}

/// Finds the business for the bearer key and puts its id in the request.
pub async fn require_api_key(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let hash = match req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|k| !k.is_empty())
    {
        Some(key) => hash_key(key),
        None => return AppError::new(StatusCode::UNAUTHORIZED, "unauthorized", "Missing or invalid API key").into_response(),
    };

    let row: Result<Option<(Uuid,)>, sqlx::Error> = sqlx::query_as(
        "SELECT business_id FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL",
    )
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await;

    match row {
        Ok(Some((business_id,))) => {
            req.extensions_mut().insert(business_id);
            next.run(req).await
        }
        Ok(None) => AppError::new(StatusCode::UNAUTHORIZED, "unauthorized", "Missing or invalid API key").into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}
