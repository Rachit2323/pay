// Mock payment processor. The card token decides the result.
// The same Idempotency-Key always gets the same answer back.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Default)]
struct Psp {
    /// Saved answers by idempotency key.
    cache: Mutex<HashMap<String, Value>>,
    /// How many more 500s to send for tok_network_error.
    network_failures_left: Mutex<u32>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = Arc::new(Psp {
        network_failures_left: Mutex::new(1),
        ..Default::default()
    });
    let app = Router::new()
        .route("/payment", post(handle_payment))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3001")
        .await
        .expect("failed to bind mock PSP");
    tracing::info!("mock PSP listening on 0.0.0.0:3001");
    axum::serve(listener, app).await.expect("mock PSP error");
}

async fn handle_payment(
    State(psp): State<Arc<Psp>>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(k) = &key {
        if let Some(cached) = psp.cache.lock().unwrap().get(k) {
            return (StatusCode::OK, Json(cached.clone()));
        }
    }

    let token = req["card_token"].as_str().unwrap_or_default();
    let body = match token {
        "tok_success" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            succeeded()
        }
        "tok_insufficient_funds" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            failed("insufficient_funds")
        }
        "tok_card_declined" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            failed("card_declined")
        }
        "tok_timeout" => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            succeeded()
        }
        "tok_network_error" => {
            let mut left = psp.network_failures_left.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "upstream unavailable" })),
                );
            }
            succeeded()
        }
        _ => failed("unknown_token"),
    };

    if let Some(k) = key {
        psp.cache.lock().unwrap().insert(k, body.clone());
    }
    (StatusCode::OK, Json(body))
}

fn succeeded() -> Value {
    json!({ "status": "succeeded", "psp_ref": Uuid::new_v4().to_string(), "code": "success" })
}

fn failed(code: &str) -> Value {
    json!({ "status": "failed", "code": code })
}
