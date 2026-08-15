// Package db owns the Postgres connection pool and order persistence.
package db

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"log/slog"
	"time"

	_ "github.com/jackc/pgx/v5/stdlib"

	"github.com/example/checkout/internal/models"
)

// maxPoolSize is the upper bound on concurrent Postgres connections held by
// this process. Sizing rules and the per-environment minimums live in
// config/pool.toml.
const maxPoolSize = 4

// acquireTimeout is how long a request waits for a free connection before
// giving up.
const acquireTimeout = 250 * time.Millisecond

// maxLifetime recycles connections rather than holding them for the life of
// the process, so a failed-over primary does not strand the pool on dead
// sockets.
const maxLifetime = 30 * time.Minute

var (
	// ErrPoolTimeout is returned when no connection became free before
	// acquireTimeout elapsed. The query never reached Postgres.
	ErrPoolTimeout = errors.New("timed out waiting for a database connection")

	// ErrNotFound is returned when an order id matches no row.
	ErrNotFound = errors.New("no such order")
)

// Store is the checkout service's handle on Postgres.
type Store struct {
	db *sql.DB
}

// Connect opens the pool. The pool is opened once per process and shared by
// every in-flight request.
func Connect(databaseURL string) (*Store, error) {
	handle, err := sql.Open("pgx", databaseURL)
	if err != nil {
		return nil, fmt.Errorf("open checkout database: %w", err)
	}
	handle.SetMaxOpenConns(maxPoolSize)
	handle.SetConnMaxLifetime(maxLifetime)
	slog.Info("checkout pool opened", "max_connections", maxPoolSize)
	return &Store{db: handle}, nil
}

// InsertOrder persists a placed order and returns its id.
func (s *Store) InsertOrder(ctx context.Context, order models.Order) (models.OrderID, error) {
	ctx, cancel := context.WithTimeout(ctx, acquireTimeout)
	defer cancel()

	const query = `insert into orders (customer_id, total_cents, currency, status)
	               values ($1, $2, $3, 'placed')
	               returning id`

	var id int64
	err := s.db.QueryRowContext(ctx, query,
		order.CustomerID, order.Total.Cents, order.Total.Currency).Scan(&id)
	if err != nil {
		return 0, classify(err)
	}
	return models.OrderID(id), nil
}

// ReserveInventory holds stock for every line in the order, or nothing at all.
func (s *Store) ReserveInventory(ctx context.Context, order models.Order) error {
	ctx, cancel := context.WithTimeout(ctx, acquireTimeout)
	defer cancel()

	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return classify(err)
	}
	defer func() { _ = tx.Rollback() }()

	const query = `update inventory set reserved = reserved + $1
	               where sku = $2 and available - reserved >= $1`

	for _, item := range order.Items {
		if _, err := tx.ExecContext(ctx, query, item.Quantity, item.SKU); err != nil {
			return classify(err)
		}
	}
	if err := tx.Commit(); err != nil {
		return classify(err)
	}
	return nil
}

// LoadOrder reads a previously placed order.
func (s *Store) LoadOrder(ctx context.Context, id models.OrderID) (models.Order, error) {
	ctx, cancel := context.WithTimeout(ctx, acquireTimeout)
	defer cancel()

	const query = `select customer_id, total_cents, currency from orders where id = $1`

	var order models.Order
	err := s.db.QueryRowContext(ctx, query, int64(id)).
		Scan(&order.CustomerID, &order.Total.Cents, &order.Total.Currency)
	if err != nil {
		return models.Order{}, classify(err)
	}
	return order, nil
}

// classify separates "never got a connection" from "the query itself failed".
// The handler turns the first into a 503 and the second into a 500, so the
// distinction decides which alert fires.
func classify(err error) error {
	switch {
	case errors.Is(err, context.DeadlineExceeded):
		return ErrPoolTimeout
	case errors.Is(err, sql.ErrNoRows):
		return ErrNotFound
	default:
		return err
	}
}
