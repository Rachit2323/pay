mod common;

use std::{sync::Arc, time::Duration};

use common::{idem, setup};
use serde_json::json;

const LONG: Duration = Duration::from_secs(5);

// Ten pays at once on one invoice, each with its own key.
#[tokio::test]
async fn concurrent_pays_with_distinct_keys_charge_at_most_once() {
    let env = Arc::new(setup(Duration::ZERO, LONG, 0).await);
    let inv = env.invoice(1, 1000).await;

    let mut tasks = Vec::new();
    for _ in 0..10 {
        let (env, inv) = (env.clone(), inv.clone());
        tasks.push(tokio::spawn(async move {
            env.pay(&inv, &idem(), "tok_success").await
        }));
    }
    let mut ok = 0;
    for t in tasks {
        let r = t.await.unwrap();
        match r.status {
            200 => {
                assert_eq!(r.body["status"], "succeeded");
                ok += 1;
            }
            // The losers get 409, or 400 if the winner already finished.
            409 => assert_eq!(r.body["code"], "payment_in_progress"),
            400 => assert_eq!(r.body["code"], "invalid_state_for_payment"),
            other => panic!("unexpected status {other}: {}", r.body),
        }
    }

    assert_eq!(ok, 1, "exactly one payment may succeed");
    assert_eq!(env.psp.charges(), 1, "PSP charged exactly once");
    assert_eq!(env.invoice_state(&inv).await, "paid");
    assert_eq!(
        env.count(
            "SELECT count(*) FROM payment_attempts WHERE business_id = $1 AND status = 'succeeded'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn concurrent_pays_with_same_key_return_one_result_and_charge_once() {
    let env = Arc::new(setup(Duration::ZERO, LONG, 0).await);
    let inv = env.invoice(1, 1000).await;
    let key = idem();

    let mut tasks = Vec::new();
    for _ in 0..10 {
        let (env, inv, key) = (env.clone(), inv.clone(), key.clone());
        tasks.push(tokio::spawn(async move {
            env.pay(&inv, &key, "tok_success").await
        }));
    }
    let mut ids = std::collections::HashSet::new();
    for t in tasks {
        let r = t.await.unwrap();
        assert_eq!(r.status, 200, "{}", r.body);
        ids.insert(r.body["id"].as_str().unwrap().to_string());
    }

    assert_eq!(
        ids.len(),
        1,
        "every request must see the same payment attempt"
    );
    assert_eq!(env.psp.charges(), 1);
    assert_eq!(
        env.count("SELECT count(*) FROM payment_attempts WHERE business_id = $1")
            .await,
        1
    );
}

// Same key again returns the same answer and does not call the PSP.
#[tokio::test]
async fn idempotent_retry_returns_same_response_without_second_psp_call() {
    let env = setup(Duration::ZERO, LONG, 0).await;
    let inv = env.invoice(1, 1000).await;
    let key = idem();

    let first = env.pay(&inv, &key, "tok_success").await;
    let calls_after_first = env.psp.calls();
    let second = env.pay(&inv, &key, "tok_success").await;

    assert_eq!(first.status, 200);
    assert_eq!(second.status, first.status);
    assert_eq!(second.body, first.body);
    assert_eq!(
        env.psp.calls(),
        calls_after_first,
        "retry must not reach the PSP"
    );
    assert_eq!(env.psp.charges(), 1);
}

#[tokio::test]
async fn reusing_key_with_different_body_is_rejected() {
    let env = setup(Duration::ZERO, LONG, 0).await;
    let inv = env.invoice(1, 1000).await;
    let key = idem();

    let first = env.pay(&inv, &key, "tok_card_declined").await;
    assert_eq!(first.status, 200);
    assert_eq!(first.body["status"], "failed");

    let reused = env.pay(&inv, &key, "tok_success").await;
    assert_eq!(reused.status, 422);
    assert_eq!(reused.body["code"], "idempotency_key_reused");
    assert_eq!(env.psp.charges(), 0, "the rejected reuse must not charge");
    assert_eq!(env.invoice_state(&inv).await, "open");
}

// A PSP timeout must not leave the invoice stuck.
#[tokio::test]
async fn psp_timeout_leaves_invoice_payable_and_retry_resolves_it() {
    // The PSP answers after 1 s. We give up after 200 ms.
    let env = setup(Duration::from_secs(1), Duration::from_millis(200), 0).await;
    let inv = env.invoice(1, 1000).await;
    let key = idem();

    let timed_out = env.pay(&inv, &key, "tok_timeout").await;
    assert_eq!(timed_out.status, 202, "{}", timed_out.body);
    assert_eq!(timed_out.body["status"], "pending");
    assert_eq!(timed_out.body["error_code"], "psp_unknown");
    assert_eq!(
        env.invoice_state(&inv).await,
        "open",
        "unknown outcome must not mark the invoice paid"
    );

    // A new key must not start a second charge while this one is unknown.
    let blocked = env.pay(&inv, &idem(), "tok_success").await;
    assert_eq!(blocked.status, 409);
    assert_eq!(blocked.body["code"], "payment_in_progress");

    // Retry with the same key to get the result.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let resolved = env.pay(&inv, &key, "tok_timeout").await;
    assert_eq!(resolved.status, 200, "{}", resolved.body);
    assert_eq!(resolved.body["status"], "succeeded");
    assert_eq!(env.invoice_state(&inv).await, "paid");
    assert_eq!(env.psp.charges(), 1, "one charge across timeout and retry");
}

#[tokio::test]
async fn psp_network_error_leaves_invoice_open_and_retry_recovers() {
    let env = setup(Duration::ZERO, LONG, 1).await;
    let inv = env.invoice(1, 1000).await;
    let key = idem();

    let failed = env.pay(&inv, &key, "tok_network_error").await;
    assert_eq!(failed.status, 202, "{}", failed.body);
    assert_eq!(
        failed.body["status"], "pending",
        "an unknown outcome is not a declined payment"
    );
    assert_eq!(env.invoice_state(&inv).await, "open");

    let recovered = env.pay(&inv, &key, "tok_network_error").await;
    assert_eq!(recovered.status, 200, "{}", recovered.body);
    assert_eq!(recovered.body["status"], "succeeded");
    assert_eq!(env.invoice_state(&inv).await, "paid");
    assert_eq!(env.psp.charges(), 1);
}

#[tokio::test]
async fn declined_payment_keeps_invoice_open_and_a_new_attempt_can_succeed() {
    let env = setup(Duration::ZERO, LONG, 0).await;
    let inv = env.invoice(1, 1000).await;

    let declined = env.pay(&inv, &idem(), "tok_card_declined").await;
    assert_eq!(declined.status, 200);
    assert_eq!(declined.body["status"], "failed");
    assert_eq!(declined.body["error_code"], "card_declined");
    assert_eq!(env.invoice_state(&inv).await, "open");

    let ok = env.pay(&inv, &idem(), "tok_success").await;
    assert_eq!(ok.status, 200);
    assert_eq!(env.invoice_state(&inv).await, "paid");

    assert_eq!(
        env.count("SELECT count(*) FROM payment_attempts WHERE business_id = $1")
            .await,
        2
    );
}

// A paid invoice can't be paid again.
#[tokio::test]
async fn paying_a_paid_invoice_is_rejected_without_a_psp_call() {
    let env = setup(Duration::ZERO, LONG, 0).await;
    let inv = env.invoice(1, 1000).await;
    assert_eq!(env.pay(&inv, &idem(), "tok_success").await.status, 200);
    let calls = env.psp.calls();

    let again = env.pay(&inv, &idem(), "tok_success").await;
    assert_eq!(again.status, 400);
    assert_eq!(again.body["code"], "invalid_state_for_payment");
    assert_eq!(env.psp.calls(), calls);
}

// The server computes the total. A total sent by the client is ignored.
#[tokio::test]
async fn server_computes_total_in_integer_cents_and_ignores_client_total() {
    let env = setup(Duration::ZERO, LONG, 0).await;
    let cust = env.customer().await;
    let r = env
        .post(
            "/invoices",
            json!({
                "customer_id": cust,
                "due_date": "2030-01-01T00:00:00Z",
                "total_amount_cents": 1,
                "line_items": [
                    { "description": "license", "quantity": 1, "unit_amount_cents": 50000 },
                    { "description": "hours", "quantity": 10, "unit_amount_cents": 10000 }
                ]
            }),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["total_amount_cents"], 150000);
    assert!(r.body["total_amount_cents"].is_i64());
    assert_eq!(r.body["state"], "open");
}

#[tokio::test]
async fn invoice_validation_rejects_bad_line_items_with_error_codes() {
    let env = setup(Duration::ZERO, LONG, 0).await;
    let cust = env.customer().await;
    let make = |items: serde_json::Value| json!({ "customer_id": cust, "due_date": "2030-01-01T00:00:00Z", "line_items": items });

    let cases = [
        json!([]),
        json!([{ "description": "x", "quantity": 0, "unit_amount_cents": 100 }]),
        json!([{ "description": "x", "quantity": 1, "unit_amount_cents": -5 }]),
        json!([{ "description": "", "quantity": 1, "unit_amount_cents": 5 }]),
        json!([{ "description": "free", "quantity": 1, "unit_amount_cents": 0 }]),
        json!([{ "description": "big", "quantity": i64::MAX, "unit_amount_cents": 2 }]),
    ];
    for items in cases {
        let r = env.post("/invoices", make(items.clone())).await;
        assert_eq!(r.status, 400, "items {items} -> {}", r.body);
        assert_eq!(r.body["code"], "validation_error", "items {items}");
    }

    let unknown_customer = env
        .post("/invoices", json!({ "customer_id": uuid::Uuid::new_v4(), "due_date": "2030-01-01T00:00:00Z", "line_items": [{ "description": "x", "quantity": 1, "unit_amount_cents": 5 }] }))
        .await;
    assert_eq!(unknown_customer.status, 404);
    assert_eq!(unknown_customer.body["code"], "customer_not_found");
}

#[tokio::test]
async fn errors_use_one_json_format() {
    let env = setup(Duration::ZERO, LONG, 0).await;

    let no_key = env
        .send(
            "POST",
            "/invoices/00000000-0000-0000-0000-000000000000/pay",
            Some(json!({"card_token":"tok_success"})),
            &[],
        )
        .await;
    assert_eq!(no_key.status, 400);
    assert_eq!(no_key.body["code"], "missing_idempotency_key");
    assert!(no_key.body["error"].is_string());

    let bad_id = env.get("/invoices/not-a-uuid").await;
    assert_eq!(bad_id.status, 400);
    assert_eq!(bad_id.body["code"], "invalid_id");

    let bad_state = env.get("/invoices?state=bogus").await;
    assert_eq!(bad_state.status, 400);
    assert_eq!(bad_state.body["code"], "invalid_state_filter");

    let bad_body = env.post("/customers", json!({ "name": 5 })).await;
    assert_eq!(bad_body.status, 400);
    assert_eq!(bad_body.body["code"], "invalid_body");

    let missing = env
        .get("/invoices/00000000-0000-0000-0000-000000000000")
        .await;
    assert_eq!(missing.status, 404);
    assert_eq!(missing.body["code"], "invoice_not_found");
}

#[tokio::test]
async fn duplicate_customer_email_is_a_conflict() {
    let env = setup(Duration::ZERO, LONG, 0).await;
    let email = format!("{}@example.com", uuid::Uuid::new_v4());
    assert_eq!(
        env.post("/customers", json!({ "name": "A", "email": email }))
            .await
            .status,
        201
    );
    let dup = env
        .post("/customers", json!({ "name": "B", "email": email }))
        .await;
    assert_eq!(dup.status, 409);
    assert_eq!(dup.body["code"], "customer_email_exists");
}

#[tokio::test]
async fn list_filters_by_state_and_is_scoped_to_business() {
    let env = setup(Duration::ZERO, LONG, 0).await;
    let open = env.invoice(1, 1000).await;
    let paid = env.invoice(1, 1000).await;
    assert_eq!(env.pay(&paid, &idem(), "tok_success").await.status, 200);

    let open_list = env.get("/invoices?state=open").await;
    let ids: Vec<&str> = open_list
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&open.as_str()));
    assert!(!ids.contains(&paid.as_str()));

    // Another business can't see these invoices.
    let other = setup(Duration::ZERO, LONG, 0).await;
    let other_list = other.get("/invoices").await;
    assert_eq!(other_list.body.as_array().unwrap().len(), 0);
    let cross = other.get(&format!("/invoices/{open}")).await;
    assert_eq!(cross.status, 404);
}

#[tokio::test]
async fn auth_is_required_for_every_endpoint() {
    let env = setup(Duration::ZERO, LONG, 0).await;
    let anon = reqwest::Client::new()
        .get(format!("{}/invoices", env.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    let body: serde_json::Value = anon.json().await.unwrap();
    assert_eq!(body["code"], "unauthorized");

    let bad = reqwest::Client::new()
        .get(format!("{}/invoices", env.base))
        .bearer_auth("not_a_real_key")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);
}
