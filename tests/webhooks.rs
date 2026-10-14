mod common;

use std::{
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{extract::State, http::HeaderMap, routing::post, Router};
use chrono::Utc;
use common::{idem, setup};
use dodo_payment::webhooks;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

// Receiver-side check: what a business does with our header.
type HmacSha256 = hmac::Hmac<sha2::Sha256>;
const TOLERANCE_SECS: i64 = 300;

fn verify(secret: &str, header: &str, body: &str, now: i64) -> bool {
    let mut ts = None;
    let mut sig = None;
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", v)) => ts = v.parse::<i64>().ok(),
            Some(("v1", v)) => sig = hex::decode(v).ok(),
            _ => {}
        }
    }
    let (Some(ts), Some(sig)) = (ts, sig) else {
        return false;
    };
    if (now - ts).abs() > TOLERANCE_SECS {
        return false;
    }
    use hmac::Mac;
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(format!("{ts}.{body}").as_bytes());
    mac.verify_slice(&sig).is_ok()
}


#[derive(Clone)]
struct Receiver {
    status: Arc<AtomicU16>,
    received: Arc<Mutex<Vec<(HeaderMap, String)>>>,
}

async fn receive(
    State(r): State<Receiver>,
    headers: HeaderMap,
    body: String,
) -> axum::http::StatusCode {
    r.received.lock().unwrap().push((headers, body));
    axum::http::StatusCode::from_u16(r.status.load(Ordering::SeqCst)).unwrap()
}

async fn start_receiver(status: u16) -> (String, Receiver) {
    let r = Receiver {
        status: Arc::new(AtomicU16::new(status)),
        received: Arc::new(Mutex::new(vec![])),
    };
    let router = Router::new()
        .route("/hook", post(receive))
        .with_state(r.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, r)
}

/// Worker tests take this lock. Otherwise they pick up each other's rows.
fn worker_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Runs the worker until this endpoint has nothing pending.
async fn deliver_for(pool: &PgPool, http: &reqwest::Client, webhook_id: Uuid) {
    for _ in 0..50 {
        webhooks::deliver_due(pool, http).await.unwrap();
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM webhook_deliveries WHERE webhook_id = $1 AND status = 'pending'",
        )
        .bind(webhook_id)
        .fetch_one(pool)
        .await
        .unwrap();
        if pending == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("deliveries still pending after polling");
}

async fn register(env: &common::TestEnv, url: &str) -> (Uuid, String) {
    let r = env.post("/webhooks", json!({ "endpoint_url": url })).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let id = Uuid::parse_str(r.body["id"].as_str().unwrap()).unwrap();
    (id, r.body["secret"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn paid_event_is_delivered_with_a_verifiable_signature() {
    let _guard = worker_lock().lock().await;
    let env = setup(Duration::ZERO, Duration::from_secs(5), 0).await;
    let (url, receiver) = start_receiver(200).await;
    let (wh, secret) = register(&env, &url).await;

    let inv = env.invoice(2, 500).await;
    assert_eq!(env.pay(&inv, &idem(), "tok_success").await.status, 200);
    deliver_for(&env.pool, &env.http, wh).await;

    let got = receiver.received.lock().unwrap().clone();
    let paid = got
        .iter()
        .find(|(h, _)| h.get("dodo-event").and_then(|v| v.to_str().ok()) == Some("invoice.paid"))
        .expect("invoice.paid delivered");
    let sig = paid.0.get("dodo-signature").unwrap().to_str().unwrap();
    let now = Utc::now().timestamp();
    assert!(
        verify(&secret, sig, &paid.1, now),
        "valid signature must verify"
    );
    assert!(
        !verify("whsec_wrong", sig, &paid.1, now),
        "wrong secret must fail"
    );
    assert!(
        !verify(&secret, sig, &paid.1, now + 3600),
        "stale timestamp must fail (replay protection)"
    );
    assert!(
        !verify(&secret, sig, &(paid.1.clone() + " "), now),
        "tampered body must fail"
    );

    let body: Value = serde_json::from_str(&paid.1).unwrap();
    assert_eq!(body["type"], "invoice.paid");
    assert_eq!(body["data"]["id"], inv);

    let status: String = sqlx::query_scalar(
        "SELECT status FROM webhook_deliveries WHERE webhook_id = $1 AND event_type = 'invoice.paid'",
    )
    .bind(wh)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(status, "delivered");
}

#[tokio::test]
async fn failing_endpoint_is_retried_on_schedule_then_marked_failed() {
    let _guard = worker_lock().lock().await;
    let env = setup(Duration::ZERO, Duration::from_secs(5), 0).await;
    let (url, receiver) = start_receiver(500).await;
    // Make the invoice first, so only invoice.paid goes to this endpoint.
    let inv = env.invoice(1, 100).await;
    let (wh, _) = register(&env, &url).await;
    assert_eq!(env.pay(&inv, &idem(), "tok_success").await.status, 200);

    // The first try fails, so the retry is about a minute away.
    webhooks::deliver_due(&env.pool, &env.http).await.unwrap();
    let (attempts, next_in_secs): (i32, f64) = sqlx::query_as(
        "SELECT attempt_count, EXTRACT(EPOCH FROM (next_retry_at - now()))::float8
         FROM webhook_deliveries WHERE webhook_id = $1 AND event_type = 'invoice.paid'",
    )
    .bind(wh)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(attempts, 1);
    assert!(
        next_in_secs > 50.0 && next_in_secs <= 61.0,
        "first retry in ~60 s, got {next_in_secs}"
    );

    // Make the retries due now and run them. Six tries, then failed.
    for _ in 0..6 {
        sqlx::query("UPDATE webhook_deliveries SET next_retry_at = now() WHERE webhook_id = $1 AND status = 'pending'")
            .bind(wh)
            .execute(&env.pool)
            .await
            .unwrap();
        webhooks::deliver_due(&env.pool, &env.http).await.unwrap();
    }
    let (status, attempts): (String, i32) = sqlx::query_as(
        "SELECT status, attempt_count FROM webhook_deliveries WHERE webhook_id = $1 AND event_type = 'invoice.paid'",
    )
    .bind(wh)
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(attempts, webhooks::MAX_ATTEMPTS);
    assert_eq!(
        receiver.received.lock().unwrap().len(),
        webhooks::MAX_ATTEMPTS as usize
    );
}

#[tokio::test]
async fn webhook_endpoint_must_be_http_and_unique() {
    let env = setup(Duration::ZERO, Duration::from_secs(5), 0).await;
    let bad = env
        .post(
            "/webhooks",
            json!({ "endpoint_url": "ftp://example.com/x" }),
        )
        .await;
    assert_eq!(bad.status, 400);
    assert_eq!(bad.body["code"], "validation_error");

    let url = "https://example.com/hooks/unique";
    assert_eq!(
        env.post("/webhooks", json!({ "endpoint_url": url }))
            .await
            .status,
        201
    );
    let dup = env.post("/webhooks", json!({ "endpoint_url": url })).await;
    assert_eq!(dup.status, 409);
    assert_eq!(dup.body["code"], "webhook_exists");
}

#[test]
fn signature_round_trip_and_tamper_detection() {
    let body = r#"{"type":"invoice.paid"}"#;
    let sig = webhooks::sign("whsec_x", 1_700_000_000, body);
    let header = format!("t=1700000000,v1={sig}");
    assert!(verify("whsec_x", &header, body, 1_700_000_100));
    assert!(
        !verify("whsec_x", &header, body, 1_700_001_000),
        "older than the 5 min tolerance"
    );
    assert!(
        !verify("whsec_x", "t=1700000000,v1=zz", body, 1_700_000_100),
        "non-hex signature"
    );
    assert!(!verify("whsec_x", "garbage", body, 1_700_000_100));
}
