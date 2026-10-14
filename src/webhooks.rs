// Webhooks: the outbox rows, signing, and the delivery worker.
// Signature header: t=<unix time>,v1=<hmac of "t.body">.
// Each delivery gets 6 tries, with waits of 1m, 5m, 30m, 2h, 6h.

use std::time::Duration;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::db;

pub const MAX_ATTEMPTS: i32 = 6;
pub const RETRY_DELAYS_SECS: [f64; 5] = [60.0, 300.0, 1800.0, 7200.0, 21600.0];
/// Hides a claimed row from other workers. If a worker crashes, the row comes back after this.
const LEASE_SECS: f64 = 60.0;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

type HmacSha256 = Hmac<Sha256>;

pub fn sign(secret: &str, timestamp: i64, body: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(format!("{timestamp}.{body}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Adds a pending delivery for each endpoint. Call it in the same transaction as the change.
pub async fn enqueue(
    conn: &mut PgConnection,
    business_id: Uuid,
    event_type: &str,
    data: Value,
) -> sqlx::Result<()> {
    let webhook_ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM webhooks WHERE business_id = $1")
            .bind(business_id)
            .fetch_all(&mut *conn)
            .await?;

    for webhook_id in webhook_ids {
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, webhook_id, event_type, payload, status, attempt_count, next_retry_at)
             VALUES ($1, $2, $3, $4, 'pending', 0, now())",
        )
        .bind(Uuid::new_v4())
        .bind(webhook_id)
        .bind(event_type)
        .bind(&data)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

struct Due {
    id: Uuid,
    event_type: String,
    payload: Value,
    attempt_count: i32,
    created_at: DateTime<Utc>,
    endpoint_url: String,
    secret: String,
}

/// Sends the due deliveries once each. Returns how many it tried.
pub async fn deliver_due(pool: &PgPool, http: &reqwest::Client) -> sqlx::Result<usize> {
    // Push the retry time forward so no other worker picks these rows up.
    let claimed: Vec<(Uuid,)> = sqlx::query_as(
        "UPDATE webhook_deliveries SET next_retry_at = now() + ($1 * interval '1 second')
         WHERE id IN (
             SELECT id FROM webhook_deliveries
             WHERE status = 'pending' AND next_retry_at <= now()
             ORDER BY next_retry_at LIMIT 50 FOR UPDATE SKIP LOCKED)
         RETURNING id",
    )
    .bind(LEASE_SECS)
    .fetch_all(pool)
    .await?;

    let mut attempted = 0;
    for (id,) in claimed {
        let due: Due = sqlx::query_as::<_, (Uuid, String, Value, i32, DateTime<Utc>, String, String)>(
            "SELECT d.id, d.event_type, d.payload, d.attempt_count, d.created_at, w.endpoint_url, w.secret
             FROM webhook_deliveries d JOIN webhooks w ON w.id = d.webhook_id WHERE d.id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .map(|(id, event_type, payload, attempt_count, created_at, endpoint_url, secret)| Due {
            id,
            event_type,
            payload,
            attempt_count,
            created_at,
            endpoint_url,
            secret,
        })?;

        deliver_one(pool, http, due).await?;
        attempted += 1;
    }
    Ok(attempted)
}

async fn deliver_one(pool: &PgPool, http: &reqwest::Client, due: Due) -> sqlx::Result<()> {
    let body = json!({
        "id": due.id,
        "type": due.event_type,
        "created_at": due.created_at,
        "data": due.payload,
    })
    .to_string();
    let ts = Utc::now().timestamp();
    let signature = sign(&due.secret, ts, &body);

    let result = http
        .post(&due.endpoint_url)
        .timeout(DELIVERY_TIMEOUT)
        .header("Content-Type", "application/json")
        .header("Dodo-Signature", format!("t={ts},v1={signature}"))
        .header("Dodo-Event", &due.event_type)
        .header("Dodo-Delivery-Id", due.id.to_string())
        .body(body)
        .send()
        .await;

    let attempts = due.attempt_count + 1;
    let code = match &result {
        Ok(r) => Some(r.status().as_u16() as i32),
        Err(_) => None,
    };
    let succeeded = matches!(&result, Ok(r) if r.status().is_success());

    if succeeded {
        sqlx::query(
            "UPDATE webhook_deliveries SET status = 'delivered', attempt_count = $1, last_attempt_at = now(),
             last_response_code = $2, next_retry_at = NULL, updated_at = now() WHERE id = $3",
        )
        .bind(attempts)
        .bind(code)
        .bind(due.id)
        .execute(pool)
        .await?;
        return Ok(());
    }

    tracing::warn!(delivery = %due.id, attempt = attempts, code = ?code, "webhook delivery failed");

    if attempts >= MAX_ATTEMPTS {
        sqlx::query(
            "UPDATE webhook_deliveries SET status = 'failed', attempt_count = $1, last_attempt_at = now(),
             last_response_code = $2, next_retry_at = NULL, updated_at = now() WHERE id = $3",
        )
        .bind(attempts)
        .bind(code)
        .bind(due.id)
        .execute(pool)
        .await?;
    } else {
        let delay = RETRY_DELAYS_SECS[(attempts - 1) as usize];
        sqlx::query(
            "UPDATE webhook_deliveries SET attempt_count = $1, last_attempt_at = now(), last_response_code = $2,
             next_retry_at = now() + ($3 * interval '1 second'), updated_at = now() WHERE id = $4",
        )
        .bind(attempts)
        .bind(code)
        .bind(delay)
        .bind(due.id)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Runs the worker forever, once a second.
pub async fn run_worker(pool: PgPool, http: reqwest::Client) {
    loop {
        match deliver_due(&pool, &http).await {
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "webhook worker tick failed"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Random secret made from two UUIDs.
pub fn new_secret() -> String {
    format!(
        "whsec_{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

pub async fn register(
    conn: &mut PgConnection,
    business_id: Uuid,
    url: &str,
) -> sqlx::Result<(crate::models::Webhook, String)> {
    let secret = new_secret();
    let webhook = db::insert_webhook(conn, business_id, url, &secret).await?;
    Ok((webhook, secret))
}
