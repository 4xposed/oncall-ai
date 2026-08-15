// Package payments is the client for the payments provider.
package payments

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"time"

	"github.com/example/checkout/internal/models"
)

var (
	// ErrDeclined is the provider refusing the charge. A decision, not a fault.
	ErrDeclined = errors.New("card declined")

	// ErrInvalidCard is the customer's card details being unusable.
	ErrInvalidCard = errors.New("card details are not valid")

	// ErrUnavailable is the provider failing to answer.
	ErrUnavailable = errors.New("payments provider is unavailable")
)

// Client talks to the payments provider over HTTP.
type Client struct {
	http       *http.Client
	baseURL    string
	maxRetries int
}

// New builds a client with a per-request timeout and a bounded retry budget.
func New(baseURL string, timeout time.Duration, maxRetries int) *Client {
	return &Client{
		http:       &http.Client{Timeout: timeout},
		baseURL:    baseURL,
		maxRetries: maxRetries,
	}
}

// Authorize authorizes the order total and returns the provider's
// authorization id. Retries are bounded and only cover transport failures; a
// decline is a decision, not a fault, and is never retried.
func (c *Client) Authorize(ctx context.Context, order models.Order) (string, error) {
	for attempt := 0; ; attempt++ {
		authorization, err := c.tryAuthorize(ctx, order)
		switch {
		case err == nil:
			return authorization, nil
		case errors.Is(err, ErrUnavailable) && attempt < c.maxRetries:
			select {
			case <-time.After(backoff(attempt + 1)):
			case <-ctx.Done():
				return "", ErrUnavailable
			}
		default:
			return "", err
		}
	}
}

func (c *Client) tryAuthorize(ctx context.Context, order models.Order) (string, error) {
	body, err := json.Marshal(map[string]any{
		"customer_id":  order.CustomerID,
		"amount_cents": order.Total.Cents,
		"currency":     order.Total.Currency,
	})
	if err != nil {
		return "", fmt.Errorf("encode authorization request: %w", err)
	}

	request, err := http.NewRequestWithContext(ctx, http.MethodPost,
		c.baseURL+"/authorizations", bytes.NewReader(body))
	if err != nil {
		return "", fmt.Errorf("build authorization request: %w", err)
	}
	request.Header.Set("Content-Type", "application/json")

	response, err := c.http.Do(request)
	if err != nil {
		return "", ErrUnavailable
	}
	defer func() { _ = response.Body.Close() }()

	switch response.StatusCode {
	case http.StatusOK:
		var decoded struct {
			ID string `json:"id"`
		}
		if err := json.NewDecoder(response.Body).Decode(&decoded); err != nil {
			return "", ErrUnavailable
		}
		return decoded.ID, nil
	case http.StatusPaymentRequired:
		return "", fmt.Errorf("%w: insufficient funds", ErrDeclined)
	case http.StatusUnprocessableEntity:
		return "", ErrInvalidCard
	default:
		return "", ErrUnavailable
	}
}

func backoff(attempt int) time.Duration {
	return time.Duration(attempt) * 100 * time.Millisecond
}
