use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub business_id: Uuid,
    pub name: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Invoice {
    pub id: Uuid,
    pub business_id: Uuid,
    pub customer_id: Uuid,
    pub state: String,
    pub total_amount_cents: i64,
    pub due_date: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Invoice states. Open goes to paid, and paid is final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceState {
    Open,
    Paid,
}

impl InvoiceState {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(InvoiceState::Open),
            "paid" => Some(InvoiceState::Paid),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            InvoiceState::Open => "open",
            InvoiceState::Paid => "paid",
        }
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PaymentAttempt {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub business_id: Uuid,
    pub idempotency_key: String,
    #[serde(skip)]
    pub request_hash: String,
    pub status: String,
    pub psp_reference: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub amount_cents: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Webhook {
    pub id: Uuid,
    pub business_id: Uuid,
    pub endpoint_url: String,
    pub created_at: DateTime<Utc>,
}

/// The secret is only returned when the webhook is created.
#[derive(Debug, Serialize)]
pub struct RegisteredWebhook {
    #[serde(flatten)]
    pub webhook: Webhook,
    pub secret: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateCustomerRequest {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    pub customer_id: Uuid,
    pub due_date: DateTime<Utc>,
    pub line_items: Vec<LineItemInput>,
}

#[derive(Debug, Deserialize)]
pub struct LineItemInput {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
}

#[derive(Debug, Deserialize)]
pub struct PayInvoiceRequest {
    pub card_token: String,
}

#[derive(Debug, Deserialize)]
pub struct RegisterWebhookRequest {
    pub endpoint_url: String,
}
