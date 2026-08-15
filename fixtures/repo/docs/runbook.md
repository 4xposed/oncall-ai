# Runbook: HighErrorRate on checkout

The alert fires when the checkout 5xx rate stays above 2% for five minutes.
It is a customer-visible page: every 5xx here is an order that was not placed.

## What produces a 5xx

`internal/handlers/handlers.go` is the only place in this service that returns
a 5xx:

- `503 Service Unavailable` — the request could not get a database connection
  before the acquire timeout elapsed.
- `500 Internal Server Error` — an unexpected database or serialization error.

Everything else is a 4xx by design. A declined card is `402`, an expired cart
is `409`, and a malformed body is `400`, so a payments provider incident shows
up as a 4xx rate rather than as this alert.

## First checks

1. Split the 5xx rate by status code. A spike that is almost entirely `503`
   means requests are queueing on connection acquisition, not failing inside
   Postgres. A spike of `500` means the queries themselves are failing.
2. Check Postgres health: connection counts, `pg_stat_activity`, replication
   lag. A healthy database serving a saturated client pool looks fine from the
   database side and terrible from ours.
3. Check the payments provider status page — but see above, its failures are
   4xx here, so this is an elimination step rather than a likely cause.
4. Check whether a deploy went out in the alert window.

## Escalation

Page the payments team only after step 3 has actually implicated them. For
anything in steps 1 and 2, the checkout service owns the fix.
