use std::time::Duration;

use dodo_payment::{app, auth, webhooks, AppState};
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let port = std::env::var("SERVER_PORT").unwrap_or_else(|_| "3000".into());
    let psp_url = std::env::var("PSP_BASE_URL").unwrap_or_else(|_| "http://localhost:3001".into());
    let psp_timeout = Duration::from_secs(35);

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .expect("failed to connect to database");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations failed");
    tracing::info!("migrations applied");

    if let Ok(key) = std::env::var("DEV_API_KEY") {
        auth::seed_dev_key(&pool, &key)
            .await
            .expect("failed to seed dev API key");
        tracing::warn!(
            "DEV_API_KEY is set: a demo business and key were seeded (development only)"
        );
    }

    let http = reqwest::Client::builder().build().expect("http client");
    tokio::spawn(webhooks::run_worker(pool.clone(), http.clone()));

    let state = AppState {
        pool,
        http,
        psp_url,
        psp_timeout,
    };
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("failed to bind");
    tracing::info!("listening on 0.0.0.0:{port}");
    axum::serve(listener, app(state))
        .await
        .expect("server error");
}
