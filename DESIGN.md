# Design

This document explains how the service is built and why.

## 1. Data model

```
business
 ├── api_keys
 ├── customers
 │    └── invoices
 │         ├── invoice_line_items
 │         └── payment_attempts
 └── webhooks
      └── webhook_deliveries
```

Every table is tied to a business. Every query filters by `business_id`, so one business can never see another business's data.

### Tables

- **businesses:** the tenant.
- **api_keys:** stores a hash of each key. Revoking a key sets `revoked_at`, so the row stays.
- **customers:** each business can use an email only once.
- **invoices:** the total is fixed when the invoice is created, so a payment always charges the same amount. An index on state and date keeps the list filter fast.
- **invoice_line_items:** the lines that make up the total.
- **payment_attempts:** one row per `Idempotency-Key`. A partial unique index allows only one pending or successful charge per invoice.
- **webhooks:** one registered URL and its signing secret.
- **webhook_deliveries:** the outbox. Each row is saved in the same transaction as the change it reports.

### Some rules I followed

- IDs are UUIDs. Writers don't need to coordinate, and IDs can't be guessed.
- Money is whole cents, stored as `BIGINT`. Math uses checked operations, so an overflow fails instead of wrapping. There are no floats in the money path.
- Times are stored as `TIMESTAMPTZ`.

### At 100 times the load

I would split the two busiest tables by month, move the outbox to a message queue, and send list queries to a read replica.

## 2. Invoice states

```
open ──(PSP confirms the charge)──▶ paid
```

- An invoice starts as `open`.
- The only change is `open → paid`, and only a confirmed charge can cause it.
- `paid` is final. A refund would be a separate record.

An invalid change is blocked in two places. The handler returns 400 before it calls the PSP. The database update also checks that the invoice is still `open`.

I left out `draft`, `void`, and `uncollectible`. The spec doesn't need them.

## 3. Payment failures

A payment has three steps. No database transaction stays open while we wait for the PSP.

1. Lock the invoice, check the key and state, save a `pending` attempt, and commit.
2. Call the PSP, sending the attempt id as its idempotency key. The timeout is 35 seconds.
3. Save the result. If the payment succeeded, mark the invoice `paid` and write the webhook rows in the same transaction.

**Two clients pay at the same time.** The second request waits for the lock. Then it sees the first attempt and gets 409 `payment_in_progress`. If the invoice is already paid, it gets 400. Only one charge is made. I used a row lock because it is visible in the schema. Advisory locks are easy to forget, and `SERIALIZABLE` causes retry storms.

**The PSP times out.** The API returns 202 with the attempt `pending` and `error_code: psp_unknown`. The invoice stays `open`. I don't mark the payment `failed`, because the card may have been charged. The caller retries with the same key, and the service asks the PSP again with the same attempt id. The PSP returns the first result. A different key gets 409 until then, so the invoice is never stuck.

**The PSP charges, then the service crashes.** The `pending` row was saved before the PSP call, so it survives the crash. A retry sends the same attempt id, and the PSP returns the original charge. The card is charged once. This only works if the PSP honors idempotency keys. The mock does, and a real PSP must.

**The same key with a different body.** The attempt stores a hash of the invoice and card token. A different hash returns 422 `idempotency_key_reused`, and nothing is charged.

**Paying a paid invoice.** The API returns 400 `invalid_state_for_payment` without calling the PSP. A retry of the original request with its key still returns the saved success, because the key is checked first.

## 4. Webhooks

- **Saving the event.** Each change saves its webhook rows in the same transaction. The API never waits for a receiver.
- **Sending.** A worker runs every second. It takes up to 50 due rows and pushes their retry time 60 seconds ahead, so no other worker takes them. If the worker crashes, the rows become due again. Delivery is at least once, so receivers should ignore duplicates using `Dodo-Delivery-Id`.
- **Signing.** The header is `Dodo-Signature: t=<unix>,v1=<hmac>`. The HMAC is SHA-256 over `t.body`, using the endpoint's secret. Receivers reject a timestamp more than 300 seconds old, which blocks replays. The secret is shown once, when the URL is registered.
- **Retries.** Each delivery gets 6 tries. The waits after failed tries are 1 minute, 5 minutes, 30 minutes, 2 hours, and 6 hours. Each try times out after 10 seconds. Any 2xx reply counts as success.
- **Giving up.** A delivery that still fails is marked `failed` and kept. The API does not replay it. The business reconciles by reading `GET /invoices?state=paid`.

## 5. API keys

- **Format:** a random string. In this build, the only key is the one in `DEV_API_KEY`. A real service would also need a way to create keys.
- **Storage:** only a SHA-256 hash is stored. Keys are long and random, so a fast hash is safe.
- **Sending:** in the `Authorization: Bearer` header. Production must use TLS. This build serves plain HTTP for local use.
- **Rotating:** create the new key, switch over, then revoke the old one. Both work during the switch.
- **Revoking:** set `revoked_at`. The check runs on every request, so the key stops working right away.
- **If a key leaks:** the attacker can access one business and charge its invoices. Other businesses are not affected.

## 6. What I cut

1. **Refunds and partial payments.** They need their own table and would complicate the state machine.
2. **Void, uncollectible, and dunning.** The spec doesn't need them.
3. **A PSP reconciliation job.** A stuck `pending` attempt should be checked with the PSP on a schedule. The mock has no status endpoint, so retries handle it for now.
4. **Rate limiting.** It belongs at the gateway, and the spec excludes it.
5. **Webhook replay, webhook listing, key management endpoints, and customer events.** Merchants reconcile with `GET /invoices`.

## 7. Production gaps

1. **Pending attempts.** Nothing checks the PSP for an attempt that stays pending. A job should do that and raise an alert.
2. **Monitoring.** Metrics for PSP outcomes and webhook results, and one request id across the API, the PSP calls, and the worker.
3. **Key management.** Creating, rotating, and auditing keys, and rate limits per key.
