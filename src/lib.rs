// The app router. Tests build it too, so they run the real code.

pub mod auth;
pub mod db;
pub mod error;
pub mod handlers;
pub mod models;
pub mod psp;
pub mod webhooks;

use std::time::Duration;

use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use sqlx::PgPool;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub http: reqwest::Client,
    pub psp_url: String,
    /// How long to wait for the PSP before calling the result unknown.
    pub psp_timeout: Duration,
}

pub fn app(state: AppState) -> Router {
    let protected = Router::new()
        .route(
            "/customers",
            post(handlers::create_customer).get(handlers::list_customers),
        )
        .route("/customers/:id", get(handlers::get_customer))
        .route(
            "/invoices",
            post(handlers::create_invoice).get(handlers::list_invoices),
        )
        .route("/invoices/:id", get(handlers::get_invoice))
        .route("/invoices/:id/pay", post(handlers::pay_invoice))
        .route("/webhooks", post(handlers::register_webhook))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_api_key,
        ));

    Router::new()
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
