// Test helpers. Runs the real app against Postgres and a fake PSP.
// Needs TEST_DATABASE_URL. The database must exist.

// Each test file uses only some helpers, so unused ones are fine here.
#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use dodo_payment::{app, auth, AppState};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use uuid::Uuid;

pub struct FakePspInner {
    pub hang: Duration,
    /// How many more 500s to send for tok_network_error.
    pub network_failures_left: AtomicU32,
    /// All calls, including repeats.
    pub calls: AtomicUsize,
    /// Successful charges, counted once per key.
    pub charges: AtomicUsize,
    cache: Mutex<HashMap<String, Value>>,
}

pub struct FakePsp {
    pub url: String,
    pub inner: Arc<FakePspInner>,
}

impl FakePsp {
    pub async fn start(hang: Duration, network_failures: u32) -> Self {
        let inner = Arc::new(FakePspInner {
            hang,
            network_failures_left: AtomicU32::new(network_failures),
            calls: AtomicUsize::new(0),
            charges: AtomicUsize::new(0),
            cache: Mutex::new(HashMap::new()),
        });
        let router = Router::new()
            .route("/payment", post(fake_handle))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, inner }
    }

    pub fn calls(&self) -> usize {
        self.inner.calls.load(Ordering::SeqCst)
    }

    pub fn charges(&self) -> usize {
        self.inner.charges.load(Ordering::SeqCst)
    }
}

async fn fake_handle(
    State(s): State<Arc<FakePspInner>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // Run in a separate task. A real PSP keeps going after the client leaves,
    // and axum would cancel this handler otherwise.
    let (status, body) = tokio::spawn(process(s, key, body)).await.unwrap();
    (status, Json(body))
}

async fn process(s: Arc<FakePspInner>, key: Option<String>, body: Value) -> (StatusCode, Value) {
    s.calls.fetch_add(1, Ordering::SeqCst);
    if let Some(k) = &key {
        if let Some(v) = s.cache.lock().unwrap().get(k) {
            return (StatusCode::OK, v.clone());
        }
    }

    let token = body["card_token"].as_str().unwrap_or_default();
    let result = match token {
        "tok_success" => {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Some(json!({ "status": "succeeded", "psp_ref": Uuid::new_v4().to_string(), "code": "success" }))
        }
        "tok_card_declined" => {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Some(json!({ "status": "failed", "code": "card_declined" }))
        }
        "tok_timeout" => {
            tokio::time::sleep(s.hang).await;
            Some(json!({ "status": "succeeded", "psp_ref": Uuid::new_v4().to_string(), "code": "success" }))
        }
        "tok_network_error" => {
            let left = s.network_failures_left.load(Ordering::SeqCst);
            if left > 0 {
                s.network_failures_left.store(left - 1, Ordering::SeqCst);
                return (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": "unavailable" }));
            }
            Some(json!({ "status": "succeeded", "psp_ref": Uuid::new_v4().to_string(), "code": "success" }))
        }
        _ => Some(json!({ "status": "failed", "code": "unknown_token" })),
    }
    .unwrap();

    // Like a real PSP, return the saved answer if the key was already used.
    if let Some(k) = key {
        let mut cache = s.cache.lock().unwrap();
        if let Some(v) = cache.get(&k) {
            return (StatusCode::OK, v.clone());
        }
        if result["status"] == "succeeded" {
            s.charges.fetch_add(1, Ordering::SeqCst);
        }
        cache.insert(k, result.clone());
    } else if result["status"] == "succeeded" {
        s.charges.fetch_add(1, Ordering::SeqCst);
    }
    (StatusCode::OK, result)
}

pub struct TestEnv {
    pub base: String,
    pub key: String,
    pub business: Uuid,
    pub pool: PgPool,
    pub psp: FakePsp,
    pub http: reqwest::Client,
}

pub async fn pool() -> PgPool {
    let url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5432/dodo_test".into());
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .connect(&url)
        .await
        .expect("connect TEST_DATABASE_URL");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations");
    pool
}

/// Each test gets its own business and key, so they can share one database.
pub async fn setup(psp_hang: Duration, psp_timeout: Duration, network_failures: u32) -> TestEnv {
    let pool = pool().await;
    let psp = FakePsp::start(psp_hang, network_failures).await;
    let state = AppState {
        pool: pool.clone(),
        http: reqwest::Client::new(),
        psp_url: psp.url.clone(),
        psp_timeout,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app(state)).await.unwrap() });

    let business = Uuid::new_v4();
    let key = format!("test_{}", Uuid::new_v4().simple());
    sqlx::query("INSERT INTO businesses (id, name) VALUES ($1, 'test')")
        .bind(business)
        .execute(&pool)
        .await
        .unwrap();
    auth::create_api_key(&pool, business, &key).await.unwrap();

    TestEnv {
        base,
        key,
        business,
        pool,
        psp,
        http: reqwest::Client::new(),
    }
}

pub struct Resp {
    pub status: u16,
    pub body: Value,
}

impl TestEnv {
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, String)],
    ) -> Resp {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = self
            .http
            .request(m, format!("{}{}", self.base, path))
            .bearer_auth(&self.key);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.expect("request");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Resp { status, body }
    }

    pub async fn post(&self, path: &str, body: Value) -> Resp {
        self.send("POST", path, Some(body), &[]).await
    }

    pub async fn get(&self, path: &str) -> Resp {
        self.send("GET", path, None, &[]).await
    }

    pub async fn pay(&self, invoice: &str, idem_key: &str, token: &str) -> Resp {
        self.send(
            "POST",
            &format!("/invoices/{invoice}/pay"),
            Some(json!({ "card_token": token })),
            &[("Idempotency-Key", idem_key.to_string())],
        )
        .await
    }

    pub async fn customer(&self) -> String {
        let r = self
            .post(
                "/customers",
                json!({ "name": "Test", "email": format!("{}@example.com", Uuid::new_v4())}),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body["id"].as_str().unwrap().to_string()
    }

    /// Makes an open invoice with qty * unit cents.
    pub async fn invoice(&self, qty: i64, unit: i64) -> String {
        let cust = self.customer().await;
        let r = self
            .post(
                "/invoices",
                json!({
                    "customer_id": cust,
                    "due_date": "2030-01-01T00:00:00Z",
                    "line_items": [{ "description": "Plan", "quantity": qty, "unit_amount_cents": unit }]
                }),
            )
            .await;
        assert_eq!(r.status, 201, "{}", r.body);
        r.body["id"].as_str().unwrap().to_string()
    }

    /// Reads a value straight from the database.
    pub async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .bind(self.business)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    pub async fn invoice_state(&self, invoice: &str) -> String {
        let id = Uuid::parse_str(invoice).unwrap();
        sqlx::query_scalar("SELECT state FROM invoices WHERE id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

pub fn idem() -> String {
    Uuid::new_v4().to_string()
}
