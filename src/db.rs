// Database queries. Each one takes a connection, so it can run inside a transaction.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::models::{Customer, Invoice, PaymentAttempt, Webhook};

pub async fn insert_customer(conn: &mut PgConnection, business_id: Uuid, name: &str, email: &str) -> sqlx::Result<Customer> {
    sqlx::query_as::<_, Customer>("INSERT INTO customers (id, business_id, name, email) VALUES ($1, $2, $3, $4) RETURNING id, business_id, name, email, created_at")
        .bind(Uuid::new_v4())
        .bind(business_id)
        .bind(name)
        .bind(email)
        .fetch_one(conn)
        .await
}

pub async fn get_customer(conn: &mut PgConnection, business_id: Uuid, id: Uuid) -> sqlx::Result<Option<Customer>> {
    sqlx::query_as::<_, Customer>("SELECT id, business_id, name, email, created_at FROM customers WHERE id = $1 AND business_id = $2")
        .bind(id)
        .bind(business_id)
        .fetch_optional(conn)
        .await
}

pub async fn list_customers(conn: &mut PgConnection, business_id: Uuid) -> sqlx::Result<Vec<Customer>> {
    sqlx::query_as::<_, Customer>("SELECT id, business_id, name, email, created_at FROM customers WHERE business_id = $1 ORDER BY created_at DESC")
        .bind(business_id)
        .fetch_all(conn)
        .await
}

pub async fn insert_invoice(conn: &mut PgConnection, business_id: Uuid, customer_id: Uuid, due_date: DateTime<Utc>, total_cents: i64) -> sqlx::Result<Invoice> {
    sqlx::query_as::<_, Invoice>("INSERT INTO invoices (id, business_id, customer_id, state, total_amount_cents, due_date) VALUES ($1, $2, $3, 'open', $4, $5) RETURNING id, business_id, customer_id, state, total_amount_cents, due_date, created_at, updated_at")
        .bind(Uuid::new_v4())
        .bind(business_id)
        .bind(customer_id)
        .bind(total_cents)
        .bind(due_date)
        .fetch_one(conn)
        .await
}

pub async fn insert_line_item(conn: &mut PgConnection, invoice_id: Uuid, description: &str, quantity: i64, unit_amount_cents: i64) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO invoice_line_items (id, invoice_id, description, quantity, unit_amount_cents) VALUES ($1, $2, $3, $4, $5)")
        .bind(Uuid::new_v4())
        .bind(invoice_id)
        .bind(description)
        .bind(quantity)
        .bind(unit_amount_cents)
        .execute(conn)
        .await
        .map(|_| ())
}

pub async fn get_invoice(conn: &mut PgConnection, business_id: Uuid, id: Uuid) -> sqlx::Result<Option<Invoice>> {
    sqlx::query_as::<_, Invoice>("SELECT id, business_id, customer_id, state, total_amount_cents, due_date, created_at, updated_at FROM invoices WHERE id = $1 AND business_id = $2")
        .bind(id)
        .bind(business_id)
        .fetch_optional(conn)
        .await
}

// Locks the invoice row until the transaction ends.
pub async fn lock_invoice(conn: &mut PgConnection, business_id: Uuid, id: Uuid) -> sqlx::Result<Option<Invoice>> {
    sqlx::query_as::<_, Invoice>("SELECT id, business_id, customer_id, state, total_amount_cents, due_date, created_at, updated_at FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE")
        .bind(id)
        .bind(business_id)
        .fetch_optional(conn)
        .await
}

pub async fn list_invoices(conn: &mut PgConnection, business_id: Uuid, state: Option<&str>) -> sqlx::Result<Vec<Invoice>> {
    sqlx::query_as::<_, Invoice>("SELECT id, business_id, customer_id, state, total_amount_cents, due_date, created_at, updated_at FROM invoices WHERE business_id = $1 AND ($2::text IS NULL OR state = $2) ORDER BY created_at DESC LIMIT 200")
        .bind(business_id)
        .bind(state)
        .fetch_all(conn)
        .await
}

// Changes the state only if it is still `from`. Returns rows changed.
pub async fn set_invoice_state(conn: &mut PgConnection, id: Uuid, from: &str, to: &str) -> sqlx::Result<u64> {
    sqlx::query("UPDATE invoices SET state = $1, updated_at = now() WHERE id = $2 AND state = $3")
        .bind(to)
        .bind(id)
        .bind(from)
        .execute(conn)
        .await
        .map(|r| r.rows_affected())
}

pub async fn find_attempt_by_key(conn: &mut PgConnection, business_id: Uuid, key: &str) -> sqlx::Result<Option<PaymentAttempt>> {
    sqlx::query_as::<_, PaymentAttempt>("SELECT id, invoice_id, business_id, idempotency_key, request_hash, status, psp_reference, error_code, error_message, amount_cents, created_at, updated_at FROM payment_attempts WHERE business_id = $1 AND idempotency_key = $2")
        .bind(business_id)
        .bind(key)
        .fetch_optional(conn)
        .await
}

pub async fn find_attempt(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<PaymentAttempt> {
    sqlx::query_as::<_, PaymentAttempt>("SELECT id, invoice_id, business_id, idempotency_key, request_hash, status, psp_reference, error_code, error_message, amount_cents, created_at, updated_at FROM payment_attempts WHERE id = $1")
        .bind(id)
        .fetch_one(conn)
        .await
}

// True if the invoice has a pending or succeeded attempt.
pub async fn live_attempt_exists(conn: &mut PgConnection, invoice_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM payment_attempts WHERE invoice_id = $1 AND status IN ('pending', 'succeeded'))")
        .bind(invoice_id)
        .fetch_one(conn)
        .await
}

pub async fn insert_attempt(conn: &mut PgConnection, business_id: Uuid, invoice_id: Uuid, key: &str, request_hash: &str, amount_cents: i64) -> sqlx::Result<PaymentAttempt> {
    sqlx::query_as::<_, PaymentAttempt>("INSERT INTO payment_attempts (id, invoice_id, business_id, idempotency_key, request_hash, status, amount_cents) VALUES ($1, $2, $3, $4, $5, 'pending', $6) RETURNING id, invoice_id, business_id, idempotency_key, request_hash, status, psp_reference, error_code, error_message, amount_cents, created_at, updated_at")
        .bind(Uuid::new_v4())
        .bind(invoice_id)
        .bind(business_id)
        .bind(key)
        .bind(request_hash)
        .bind(amount_cents)
        .fetch_one(conn)
        .await
}

// Sets the final status. Only works while the attempt is pending.
// Returns rows changed, so 1 means this call did the update.
pub async fn resolve_attempt(conn: &mut PgConnection, id: Uuid, status: &str, psp_reference: Option<&str>, error_code: Option<&str>, error_message: Option<&str>) -> sqlx::Result<u64> {
    sqlx::query("UPDATE payment_attempts SET status = $1, psp_reference = $2, error_code = $3, error_message = $4, updated_at = now() WHERE id = $5 AND status = 'pending'")
        .bind(status)
        .bind(psp_reference)
        .bind(error_code)
        .bind(error_message)
        .bind(id)
        .execute(conn)
        .await
        .map(|r| r.rows_affected())
}

// Saves the reason when the PSP result is unknown.
pub async fn note_unknown_outcome(conn: &mut PgConnection, id: Uuid, reason: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE payment_attempts SET error_code = 'psp_unknown', error_message = $1, updated_at = now() WHERE id = $2 AND status = 'pending'")
        .bind(reason)
        .bind(id)
        .execute(conn)
        .await
        .map(|_| ())
}

pub async fn insert_webhook(conn: &mut PgConnection, business_id: Uuid, url: &str, secret: &str) -> sqlx::Result<Webhook> {
    sqlx::query_as::<_, Webhook>("INSERT INTO webhooks (id, business_id, endpoint_url, secret) VALUES ($1, $2, $3, $4) RETURNING id, business_id, endpoint_url, created_at")
        .bind(Uuid::new_v4())
        .bind(business_id)
        .bind(url)
        .bind(secret)
        .fetch_one(conn)
        .await
}
