# checkout

The checkout service. It creates orders, reserves inventory, charges the
customer through the payments provider, and persists the resulting order.

Alerts for this service fire under `service=checkout` in Grafana. The 5xx rate
alert (`HighErrorRate`) pages on-call.

## Layout

- `cmd/checkout/main.go` — process bootstrap, settings, HTTP server
- `internal/handlers/handlers.go` — request handlers for `/checkout`, and the
  only place a 5xx is returned
- `internal/db/db.go` — Postgres connection pool and order persistence
- `internal/payments/payments.go` — payments provider client
- `internal/models/models.go` — cart, order and receipt types
- `config/service.toml` — bind address, timeouts, feature flags
- `config/pool.toml` — connection pool sizing per environment
- `docs/runbook.md` — on-call runbook for checkout alerts

## Running locally

    go run ./cmd/checkout -config config/service.toml

`config/service.toml` is read at startup; every other config file listed above
is reference documentation for the operators who size this service.
