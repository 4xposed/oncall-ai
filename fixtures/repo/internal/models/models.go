// Package models holds the cart, order and receipt types.
package models

import (
	"errors"
	"fmt"
	"time"
)

// OrderID is the primary key of a placed order.
type OrderID int64

// String renders the id the way it appears in logs and receipts.
func (id OrderID) String() string {
	return fmt.Sprintf("order-%d", int64(id))
}

// Currency is the ISO code an order is priced in.
type Currency string

const (
	EUR Currency = "EUR"
	GBP Currency = "GBP"
	USD Currency = "USD"
)

// Money is an integer amount in the minor unit of Currency.
type Money struct {
	Cents    int64    `json:"cents"`
	Currency Currency `json:"currency"`
}

// CartItem is one line of a cart.
type CartItem struct {
	SKU       string `json:"sku"`
	Quantity  int32  `json:"quantity"`
	UnitPrice Money  `json:"unit_price"`
}

// Cart is what the client posts to /checkout.
type Cart struct {
	CustomerID int64      `json:"customer_id"`
	Items      []CartItem `json:"items"`
	ExpiresAt  time.Time  `json:"expires_at"`
}

// IsExpired reports whether the cart may no longer be checked out.
func (c Cart) IsExpired() bool {
	return !c.ExpiresAt.After(time.Now())
}

var (
	// ErrEmptyCart is returned for a cart with no lines.
	ErrEmptyCart = errors.New("cart is empty")

	// ErrMixedCurrency is returned when lines disagree on currency.
	ErrMixedCurrency = errors.New("cart mixes currencies")
)

// Order is a validated cart, priced and ready to charge.
type Order struct {
	CustomerID int64
	Items      []CartItem
	Total      Money
}

// OrderFromCart validates and prices a cart.
func OrderFromCart(cart Cart) (Order, error) {
	if len(cart.Items) == 0 {
		return Order{}, ErrEmptyCart
	}
	currency := cart.Items[0].UnitPrice.Currency

	var cents int64
	for _, item := range cart.Items {
		if item.UnitPrice.Currency != currency {
			return Order{}, ErrMixedCurrency
		}
		cents += item.UnitPrice.Cents * int64(item.Quantity)
	}

	return Order{
		CustomerID: cart.CustomerID,
		Items:      cart.Items,
		Total:      Money{Cents: cents, Currency: currency},
	}, nil
}

// Receipt is what /checkout returns on success.
type Receipt struct {
	OrderID       OrderID `json:"order_id"`
	Authorization string  `json:"authorization"`
	Total         Money   `json:"total"`
}
