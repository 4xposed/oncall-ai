// Package handlers owns the mapping from internal errors to status codes, and
// is the only place the service returns a 5xx.
package handlers

import (
	"encoding/json"
	"errors"
	"log/slog"
	"net/http"

	"github.com/example/checkout/internal/db"
	"github.com/example/checkout/internal/models"
	"github.com/example/checkout/internal/payments"
)

// App holds everything a request needs.
type App struct {
	Store               *db.Store
	Payments            *payments.Client
	ReserveBeforeCharge bool
}

// Routes builds the service's HTTP surface.
func (a *App) Routes() *http.ServeMux {
	mux := http.NewServeMux()
	mux.HandleFunc("POST /checkout", a.Checkout)
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
	})
	return mux
}

// Checkout places an order: reserve stock, charge the card, persist the order.
func (a *App) Checkout(w http.ResponseWriter, r *http.Request) {
	var cart models.Cart
	if err := json.NewDecoder(r.Body).Decode(&cart); err != nil {
		w.WriteHeader(http.StatusBadRequest)
		return
	}
	if cart.IsExpired() {
		w.WriteHeader(http.StatusConflict)
		return
	}

	order, err := models.OrderFromCart(cart)
	if err != nil {
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	if a.ReserveBeforeCharge {
		if err := a.Store.ReserveInventory(r.Context(), order); err != nil {
			w.WriteHeader(statusForDB(err))
			return
		}
	}

	authorization, err := a.Payments.Authorize(r.Context(), order)
	if err != nil {
		w.WriteHeader(statusForPayment(err))
		return
	}

	id, err := a.Store.InsertOrder(r.Context(), order)
	if err != nil {
		w.WriteHeader(statusForDB(err))
		return
	}

	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(models.Receipt{
		OrderID:       id,
		Authorization: authorization,
		Total:         order.Total,
	})
}

// statusForDB reports a pool timeout as "service unavailable": the request
// never reached Postgres, so the caller may safely retry.
func statusForDB(err error) int {
	switch {
	case errors.Is(err, db.ErrPoolTimeout):
		slog.Warn("checkout could not acquire a connection", "error", err)
		return http.StatusServiceUnavailable
	case errors.Is(err, db.ErrNotFound):
		return http.StatusNotFound
	default:
		slog.Error("checkout query failed", "error", err)
		return http.StatusInternalServerError
	}
}

// statusForPayment keeps payments failures in the 4xx range: they are the
// customer's problem or the provider's, never ours, so they must not page the
// on-call for this service.
func statusForPayment(err error) int {
	switch {
	case errors.Is(err, payments.ErrDeclined):
		return http.StatusPaymentRequired
	case errors.Is(err, payments.ErrInvalidCard):
		return http.StatusBadRequest
	default:
		slog.Warn("payments provider unavailable", "error", err)
		return http.StatusFailedDependency
	}
}
