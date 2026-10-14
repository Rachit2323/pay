# Invoice & Payment Service

An invoice and payment service in Rust (Axum, sqlx, PostgreSQL) with a mock PSP and signed webhooks.

See [DESIGN.md](DESIGN.md) for the design, [openapi.yaml](openapi.yaml) for the API, and [AI_USAGE.md](AI_USAGE.md) for how AI was used.


## Demo video

Video: https://youtu.be/HjQ-tFeXPHw

## Run it

```bash
docker compose up --build
```

This starts three services with no manual steps:

| Service | Port | Notes |
|---|---|---|
| `postgres` | 5432 | Migrations run automatically when the API starts. |
| `invoice_service` | 3000 | `DEV_API_KEY` seeds a demo business with key `biz_test_key_12345`. Development only. |
| `mock_psp` | 3001 | Chooses outcomes from the card token. |

If port 5432 is taken on your machine, set `POSTGRES_PORT=5433 docker compose up --build`.

Configuration (all optional outside compose):

| Variable | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | required | Postgres connection string |
| `PSP_BASE_URL` | `http://localhost:3001` | Mock or real PSP |
| `DEV_API_KEY` | unset | If set, seeds a demo business and this key at startup |
| `SERVER_PORT` | `3000` | |

## Demo commands (in order)

Use these in the demo video. Each step builds on the one before it, so run them in order in the same terminal.

**Step 0: start the stack** (if port 5432 is already used on your Mac, add `POSTGRES_PORT=5433` in front)

```bash
docker compose up -d --build
```

**Step 0b (optional, for webhooks): start the Rust receiver in a second terminal**

```bash
cargo run --example receiver
```

It listens on port 8080 and prints every webhook it receives.

**Step 1: set the shortcuts**

```bash
export API=http://localhost:3000
export KEY=biz_test_key_12345
```

**Step 1b: register the receiver as the webhook endpoint** (do this before creating invoices, so their events are sent to it)

```bash
curl -s -X POST $API/webhooks \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"endpoint_url":"http://host.docker.internal:8080/"}'
```

The reply includes a `secret`, which is shown only once. The receiver from Step 0b must be running.

**Step 2: no API key gets 401**

```bash
curl -s $API/customers
```

**Step 3: create a customer**

```bash
EMAIL="john-$(date +%s)@example.com"
CUSTOMER=$(curl -s -X POST $API/customers \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d "{\"name\":\"John Doe\",\"email\":\"$EMAIL\"}" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
echo "customer: $CUSTOMER"
```

**Step 4: get the customer**

```bash
curl -s $API/customers/$CUSTOMER -H "Authorization: Bearer $KEY"
```

**Step 5: list customers**

```bash
curl -s $API/customers -H "Authorization: Bearer $KEY"
```

**Step 6: same email again gets 409**

```bash
curl -s -X POST $API/customers \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d "{\"name\":\"John Again\",\"email\":\"$EMAIL\"}"
```

**Step 7: create an invoice (total is computed by the server)**

```bash
INVOICE=$(curl -s -X POST $API/invoices \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d "{\"customer_id\":\"$CUSTOMER\",\"due_date\":\"2030-01-01T00:00:00Z\",\"line_items\":[{\"description\":\"License\",\"quantity\":1,\"unit_amount_cents\":50000},{\"description\":\"Support\",\"quantity\":10,\"unit_amount_cents\":10000}]}" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
echo "invoice: $INVOICE"
```

**Step 8: get the invoice** (`total_amount_cents` is 150000, `state` is `open`)

```bash
curl -s $API/invoices/$INVOICE -H "Authorization: Bearer $KEY"
```

**Step 9: list open invoices**

```bash
curl -s "$API/invoices?state=open" -H "Authorization: Bearer $KEY"
```

**Step 10: pay with a declined card (failure case)**

```bash
DECLINE_KEY="pay-declined-$RANDOM"
curl -s -X POST $API/invoices/$INVOICE/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: $DECLINE_KEY" \
  -d '{"card_token":"tok_card_declined"}'
```

Expect `"status":"failed"` and `"error_code":"card_declined"`. The invoice is still open.

**Step 11: pay with a good card (success case)**

```bash
PAY_KEY="pay-ok-$RANDOM"
curl -s -X POST $API/invoices/$INVOICE/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: $PAY_KEY" \
  -d '{"card_token":"tok_success"}'
```

Expect `"status":"succeeded"`.

**Step 12: the invoice is now paid**

```bash
curl -s $API/invoices/$INVOICE -H "Authorization: Bearer $KEY"
```

**Step 13: retry with the same key (same answer, no second charge)**

```bash
curl -s -X POST $API/invoices/$INVOICE/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: $PAY_KEY" \
  -d '{"card_token":"tok_success"}'
```

**Step 14: same key with a different card gets 422**

```bash
curl -s -X POST $API/invoices/$INVOICE/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: $PAY_KEY" \
  -d '{"card_token":"tok_card_declined"}'
```

**Step 15: pay the paid invoice with a new key gets 400**

```bash
curl -s -X POST $API/invoices/$INVOICE/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-again-$RANDOM" \
  -d '{"card_token":"tok_success"}'
```

**Step 17: a bad webhook URL gets 400**

```bash
curl -s -X POST $API/webhooks \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"endpoint_url":"ftp://example.com/x"}'
```

**Step 18: show the webhook deliveries in the database** (the short wait lets the worker send them)

```bash
sleep 3
docker exec dodo_postgres psql -U postgres -d dodo_payments \
  -c "select event_type, status, attempt_count, last_response_code from webhook_deliveries order by created_at;"
```

With the receiver running (Step 0b), the `invoice.created` and `invoice.paid` rows show `delivered`. The receiver window prints each event and its signature.

**Step 19 (optional, takes about 30 seconds): the PSP timeout**

```bash
INVOICE2=$(curl -s -X POST $API/invoices \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d "{\"customer_id\":\"$CUSTOMER\",\"due_date\":\"2030-01-01T00:00:00Z\",\"line_items\":[{\"description\":\"Timeout\",\"quantity\":1,\"unit_amount_cents\":700}]}" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
curl -s -X POST $API/invoices/$INVOICE2/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: pay-timeout-$RANDOM" \
  -d '{"card_token":"tok_timeout"}'
```

It returns after about 30 seconds with `"status":"succeeded"`.

**Step 20: stop everything**

```bash
docker compose down
```

## Errors

Every error is `{"error": "<message>", "code": "<machine code>"}`. Common codes: `missing_idempotency_key`, `invalid_state_for_payment`, `payment_in_progress` (409), `idempotency_key_reused` (422), `validation_error`, `unauthorized`.

## Payments: what a client should do

- Send a new `Idempotency-Key` for each logical payment. Reuse the same key only to retry the same request.
- `200` is final. `202` means the PSP outcome is unknown and the attempt is `pending`. Retry with the **same key and same body** to resolve it. Do not change the key, or you may start a second charge.
- `409 payment_in_progress` means another attempt on this invoice is in flight. Wait and retry.

## Webhooks

Register an endpoint with `POST /webhooks`. The response includes a `secret`, shown once.

Deliveries are `POST`ed to your endpoint with these headers:

```
Dodo-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>
Dodo-Event: invoice.paid
Dodo-Delivery-Id: <uuid>        # dedupe on this; delivery is at-least-once
```

Verify by recomputing the HMAC over `"<t>.<raw body>"` with your endpoint secret. Reject if `|now - t| > 300` seconds.

Events: `invoice.created`, `invoice.paid`, `invoice.payment_failed`. A failed delivery is retried 6 times in total (1 min, 5 min, 30 min, 2 h, 6 h). After that it is marked `failed` in the database and is not replayed by the API. To reconcile, re-read invoice state with `GET /invoices?state=paid`.

## Testing

### Automated tests

The tests run the real app against Postgres. The fake PSP and fake business server run inside the test, so nothing else needs to be started.

```bash
createdb dodo_test
export TEST_DATABASE_URL=postgresql://postgres:postgres@localhost:5432/dodo_test
cargo test
```

Expected result: 18 passed, 0 failed.

- `tests/payments.rs` (14 tests)
  - Ten concurrent pays on one invoice, with different keys, produce one charge.
  - Ten concurrent pays with one key produce one attempt and one charge.
  - A retry with the same key returns the same answer and does not call the PSP.
  - A timeout or network error leaves the invoice `open`, and a retry resolves it.
  - Invalid state changes, money validation, the error format, auth, and tenant isolation.
- `tests/webhooks.rs` (4 tests)
  - A signed delivery reaches a receiver, and its signature verifies.
  - A receiver that keeps failing gets 6 tries, then the row is marked `failed`.
  - Webhook URL validation.

### Manual checks with curl

The commands in [Demo commands](#demo-commands-in-order) are also the manual test. Run them in order against `docker compose up`, and check the results:

| Step | Check | Expected |
|---|---|---|
| 2 | No API key | 401 `unauthorized` |
| 3–5 | Create, get, and list a customer | 201, 200, 200 |
| 6 | Duplicate email | 409 `customer_email_exists` |
| 7–9 | Create, get, and list an invoice | total `150000`, state `open` |
| 10 | Declined card | `failed`, `card_declined`, invoice still `open` |
| 11–12 | Successful payment | `succeeded`, invoice `paid` |
| 13 | Same key again | same answer, no second charge |
| 14 | Same key, different card | 422 `idempotency_key_reused` |
| 15 | Pay a paid invoice | 400 `invalid_state_for_payment` |
| 1b, 17 | Register the receiver, then a bad URL | 201, then 400 |
| 18 | Webhook rows | `delivered`, when the receiver is running |
| 19 | Timeout (optional) | `succeeded` after about 30 s |

## Layout

```
src/
  lib.rs        router and shared state
  main.rs       startup: config, migrations, dev key seed, webhook worker, server
  handlers.rs   endpoints, including the payment state machine
  db.rs         queries (take a connection, so they run inside transactions)
  auth.rs       API key hashing and the auth middleware
  psp.rs        PSP client; maps every outcome to succeeded, declined, or unknown
  webhooks.rs   outbox enqueue, HMAC signing, delivery worker
  error.rs      the single error format
  models.rs     domain types and request bodies
migrations/     schema
mock_psp/       mock payment processor (separate binary)
tests/          integration tests
```


