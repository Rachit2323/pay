// Client for the payment processor.
// We send the attempt id as the PSP idempotency key.

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PspOutcome {
    /// The charge went through.
    Succeeded { psp_ref: String },
    /// The PSP said no (declined, insufficient funds, ...).
    Declined { code: String },
    /// We don't know if the card was charged. Timeout, network error, 5xx or bad body.
    Unknown { reason: String },
}

#[derive(Deserialize)]
struct PspBody {
    status: String,
    psp_ref: Option<String>,
    code: Option<String>,
}

pub async fn charge(
    http: &reqwest::Client,
    base_url: &str,
    timeout: Duration,
    idempotency_key: &str,
    card_token: &str,
) -> PspOutcome {
    let sent = http
        .post(format!("{base_url}/payment"))
        .timeout(timeout)
        .header("Idempotency-Key", idempotency_key)
        .json(&json!({ "card_token": card_token }))
        .send()
        .await;

    let resp = match sent {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return PspOutcome::Unknown {
                reason: "psp timeout".into(),
            }
        }
        Err(e) => {
            return PspOutcome::Unknown {
                reason: format!("psp transport error: {e}"),
            }
        }
    };

    if !resp.status().is_success() {
        return PspOutcome::Unknown {
            reason: format!("psp http {}", resp.status().as_u16()),
        };
    }

    match resp.json::<PspBody>().await {
        Ok(body) => match body.status.as_str() {
            "succeeded" => match body.psp_ref {
                Some(psp_ref) => PspOutcome::Succeeded { psp_ref },
                None => PspOutcome::Unknown {
                    reason: "psp succeeded without psp_ref".into(),
                },
            },
            "failed" => PspOutcome::Declined {
                code: body.code.unwrap_or_else(|| "unknown".into()),
            },
            other => PspOutcome::Unknown {
                reason: format!("psp unexpected status {other}"),
            },
        },
        Err(e) => PspOutcome::Unknown {
            reason: format!("psp bad response: {e}"),
        },
    }
}
