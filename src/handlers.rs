use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    db,
    error::{is_unique_violation, ApiResult, AppError, AppJson},
    models::*,
    psp::{self, PspOutcome},
    webhooks, AppState,
};

const MAX_LINE_ITEMS: usize = 100;

fn parse_id(raw: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(raw).map_err(|_| AppError::bad_request("invalid_id", "Path id must be a UUID"))
}

/// Hash of the invoice and card token. Used to catch a reused key with a different request.
fn request_hash(invoice_id: Uuid, card_token: &str) -> String {
    hex::encode(Sha256::digest(
        format!("{invoice_id}:{card_token}").as_bytes(),
    ))
}

pub async fn create_customer(
    State(st): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    AppJson(req): AppJson<CreateCustomerRequest>,
) -> ApiResult<(StatusCode, Json<Customer>)> {
    let name = req.name.trim().to_string();
    let email = req.email.trim().to_lowercase();
    if name.is_empty() || name.len() > 255 {
        return Err(AppError::bad_request(
            "validation_error",
            "name must be 1-255 characters",
        ));
    }
    if !email.contains('@') || email.len() > 255 {
        return Err(AppError::bad_request(
            "validation_error",
            "email must be a valid address",
        ));
    }

    let mut conn = st.pool.acquire().await?;
    let customer = match db::insert_customer(&mut conn, business_id, &name, &email).await {
        Ok(c) => c,
        Err(e) if is_unique_violation(&e) => {
            return Err(AppError::conflict(
                "customer_email_exists",
                "A customer with this email already exists",
            ))
        }
        Err(e) => return Err(e.into()),
    };

    Ok((StatusCode::CREATED, Json(customer)))
}

pub async fn get_customer(
    State(st): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Path(id): Path<String>,
) -> ApiResult<Json<Customer>> {
    let id = parse_id(&id)?;
    let mut conn = st.pool.acquire().await?;
    db::get_customer(&mut conn, business_id, id)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::not_found("customer_not_found", "Customer not found"))
}

pub async fn list_customers(
    State(st): State<AppState>,
    Extension(business_id): Extension<Uuid>,
) -> ApiResult<Json<Vec<Customer>>> {
    let mut conn = st.pool.acquire().await?;
    Ok(Json(db::list_customers(&mut conn, business_id).await?))
}

pub async fn create_invoice(
    State(st): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    AppJson(req): AppJson<CreateInvoiceRequest>,
) -> ApiResult<(StatusCode, Json<Invoice>)> {
    if req.line_items.is_empty() || req.line_items.len() > MAX_LINE_ITEMS {
        return Err(AppError::bad_request(
            "validation_error",
            &format!("line_items must contain 1-{MAX_LINE_ITEMS} entries"),
        ));
    }

    // Money is in cents. The total comes from the line items, not the client.
    let mut total: i64 = 0;
    for item in &req.line_items {
        if item.description.trim().is_empty() || item.description.len() > 255 {
            return Err(AppError::bad_request(
                "validation_error",
                "description must be 1-255 characters",
            ));
        }
        if item.quantity < 1 {
            return Err(AppError::bad_request(
                "validation_error",
                "quantity must be at least 1",
            ));
        }
        if item.unit_amount_cents < 0 {
            return Err(AppError::bad_request(
                "validation_error",
                "unit_amount_cents must be >= 0",
            ));
        }
        let line = item
            .quantity
            .checked_mul(item.unit_amount_cents)
            .ok_or_else(|| AppError::bad_request("validation_error", "line amount overflows"))?;
        total = total
            .checked_add(line)
            .ok_or_else(|| AppError::bad_request("validation_error", "invoice total overflows"))?;
    }
    if total <= 0 {
        return Err(AppError::bad_request(
            "validation_error",
            "invoice total must be greater than 0",
        ));
    }

    let mut tx = st.pool.begin().await?;
    if db::get_customer(&mut tx, business_id, req.customer_id)
        .await?
        .is_none()
    {
        return Err(AppError::not_found(
            "customer_not_found",
            "Customer not found",
        ));
    }
    let invoice =
        db::insert_invoice(&mut tx, business_id, req.customer_id, req.due_date, total).await?;
    for item in &req.line_items {
        db::insert_line_item(
            &mut tx,
            invoice.id,
            item.description.trim(),
            item.quantity,
            item.unit_amount_cents,
        )
        .await?;
    }
    webhooks::enqueue(
        &mut tx,
        business_id,
        "invoice.created",
        json!({ "id": invoice.id, "customer_id": invoice.customer_id, "state": invoice.state, "total_amount_cents": total }),
    )
    .await?;
    tx.commit().await?;

    Ok((StatusCode::CREATED, Json(invoice)))
}

pub async fn get_invoice(
    State(st): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Path(id): Path<String>,
) -> ApiResult<Json<Invoice>> {
    let id = parse_id(&id)?;
    let mut conn = st.pool.acquire().await?;
    db::get_invoice(&mut conn, business_id, id)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::not_found("invoice_not_found", "Invoice not found"))
}

#[derive(Deserialize)]
pub struct ListInvoicesQuery {
    state: Option<String>,
}

pub async fn list_invoices(
    State(st): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Query(q): Query<ListInvoicesQuery>,
) -> ApiResult<Json<Vec<Invoice>>> {
    if let Some(s) = &q.state {
        if InvoiceState::parse(s).is_none() {
            return Err(AppError::bad_request(
                "invalid_state_filter",
                "state must be one of open, paid",
            ));
        }
    }
    let mut conn = st.pool.acquire().await?;
    Ok(Json(
        db::list_invoices(&mut conn, business_id, q.state.as_deref()).await?,
    ))
}

/// Pays an invoice.
///
/// 1. Lock the invoice, and either return the saved result for this key or
///    save a new pending attempt.
/// 2. Call the PSP. No transaction is open during this call.
/// 3. Save the PSP result. On success, mark the invoice paid.
///
/// Returns 202 when the PSP result is unknown, otherwise 200.
pub async fn pay_invoice(
    State(st): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    Path(id): Path<String>,
    headers: HeaderMap,
    AppJson(req): AppJson<PayInvoiceRequest>,
) -> ApiResult<(StatusCode, Json<PaymentAttempt>)> {
    let invoice_id = parse_id(&id)?;
    let key = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty() && k.len() <= 255)
        .ok_or_else(|| {
            AppError::bad_request(
                "missing_idempotency_key",
                "Idempotency-Key header is required (1-255 chars)",
            )
        })?
        .to_string();
    
    if req.card_token.trim().is_empty() {
        return Err(AppError::bad_request(
            "validation_error",
            "card_token is required",
        ));
    }
    let hash = request_hash(invoice_id, &req.card_token);

    let mut tx = st.pool.begin().await?;
    let invoice = db::lock_invoice(&mut tx, business_id, invoice_id)
        .await?
        .ok_or_else(|| AppError::not_found("invoice_not_found", "Invoice not found"))?;

    let attempt = match db::find_attempt_by_key(&mut tx, business_id, &key).await? {
        Some(existing) => {
            tx.commit().await?;
            if existing.request_hash != hash {
                return Err(AppError::new(StatusCode::UNPROCESSABLE_ENTITY, 
                    "idempotency_key_reused",
                    "Idempotency-Key was already used with a different request",
                ));
            }
            if existing.status != "pending" {
                return Ok((status_for(&existing), Json(existing)));
            }
            // Still pending, so ask the PSP again with the same attempt id.
            // The PSP dedupes it, so there is no second charge.
            existing
        }
        None => {
            let state = InvoiceState::parse(&invoice.state).expect("db CHECK constrains state");
            if state != InvoiceState::Open {
                return Err(AppError::bad_request(
                    "invalid_state_for_payment",
                    &format!("Cannot pay invoice in {} state", state.as_str()),
                ));
            }
            if db::live_attempt_exists(&mut tx, invoice_id).await? {
                return Err(AppError::conflict(
                    "payment_in_progress",
                    "Another payment is in progress for this invoice",
                ));
            }
            let attempt = db::insert_attempt(
                &mut tx,
                business_id,
                invoice_id,
                &key,
                &hash,
                invoice.total_amount_cents,
            )
            .await?;
            tx.commit().await?;
            attempt
        }
    };

    let outcome = psp::charge(
        &st.http,
        &st.psp_url,
        st.psp_timeout,
        &attempt.id.to_string(),
        &req.card_token,
    )
    .await;

    let mut tx = st.pool.begin().await?;
    match &outcome {
        PspOutcome::Succeeded { psp_ref } => {
            if db::resolve_attempt(&mut tx, attempt.id, "succeeded", Some(psp_ref), None, None)
                .await?
                == 1
            {
                if db::set_invoice_state(&mut tx, invoice_id, "open", "paid").await? != 1 {
                    tracing::error!(%invoice_id, "charge succeeded but invoice was not open");
                }
                webhooks::enqueue(
                    &mut tx,
                    business_id,
                    "invoice.paid",
                    json!({ "id": invoice_id, "payment_attempt_id": attempt.id, "psp_reference": psp_ref }),
                )
                .await?;
            }
        }
        PspOutcome::Declined { code } => {
            if db::resolve_attempt(&mut tx, attempt.id, "failed", None, Some(code), None).await?
                == 1
            {
                webhooks::enqueue(
                    &mut tx,
                    business_id,
                    "invoice.payment_failed",
                    json!({ "id": invoice_id, "payment_attempt_id": attempt.id, "error_code": code }),
                )
                .await?;
            }
        }
        PspOutcome::Unknown { reason } => {
            // Keep it pending. A retry with the same key will ask again.
            tracing::warn!(attempt = %attempt.id, %reason, "psp outcome unknown; attempt left pending");
            db::note_unknown_outcome(&mut tx, attempt.id, reason).await?;
        }
    }
    tx.commit().await?;

    let mut conn = st.pool.acquire().await?;
    let final_attempt = db::find_attempt(&mut conn, attempt.id).await?;
    Ok((status_for(&final_attempt), Json(final_attempt)))
}

fn status_for(attempt: &PaymentAttempt) -> StatusCode {
    if attempt.status == "pending" {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    }
}

pub async fn register_webhook(
    State(st): State<AppState>,
    Extension(business_id): Extension<Uuid>,
    AppJson(req): AppJson<RegisterWebhookRequest>,
) -> ApiResult<(StatusCode, Json<RegisteredWebhook>)> {
    let url = req.endpoint_url.trim();
    if url.len() > 2048 || !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(AppError::bad_request(
            "validation_error",
            "endpoint_url must be an http(s) URL",
        ));
    }

    let mut conn = st.pool.acquire().await?;
    match webhooks::register(&mut conn, business_id, url).await {
        Ok((webhook, secret)) => Ok((
            StatusCode::CREATED,
            Json(RegisteredWebhook { webhook, secret }),
        )),
        Err(e) if is_unique_violation(&e) => Err(AppError::conflict(
            "webhook_exists",
            "This endpoint is already registered",
        )),
        Err(e) => Err(e.into()),
    }
}
