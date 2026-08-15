// Command checkout is the checkout service entrypoint.
package main

import (
	"flag"
	"log/slog"
	"net/http"
	"os"
	"time"

	"github.com/BurntSushi/toml"

	"github.com/example/checkout/internal/db"
	"github.com/example/checkout/internal/handlers"
	"github.com/example/checkout/internal/payments"
)

type settings struct {
	Bind        string           `toml:"bind"`
	DatabaseURL string           `toml:"database_url"`
	Payments    paymentsSettings `toml:"payments"`
	Features    featureSettings  `toml:"features"`
}

type paymentsSettings struct {
	URL        string `toml:"url"`
	TimeoutMS  int64  `toml:"timeout_ms"`
	MaxRetries int    `toml:"max_retries"`
}

type featureSettings struct {
	ReserveBeforeCharge bool `toml:"reserve_before_charge"`
}

func main() {
	path := flag.String("config", "config/service.toml", "path to service.toml")
	flag.Parse()

	if err := run(*path); err != nil {
		slog.Error("checkout failed to start", "error", err)
		os.Exit(1)
	}
}

func run(path string) error {
	var config settings
	if _, err := toml.DecodeFile(path, &config); err != nil {
		return err
	}

	store, err := db.Connect(config.DatabaseURL)
	if err != nil {
		return err
	}

	app := &handlers.App{
		Store: store,
		Payments: payments.New(
			config.Payments.URL,
			time.Duration(config.Payments.TimeoutMS)*time.Millisecond,
			config.Payments.MaxRetries,
		),
		ReserveBeforeCharge: config.Features.ReserveBeforeCharge,
	}

	slog.Info("checkout listening", "addr", config.Bind)
	return http.ListenAndServe(config.Bind, app.Routes())
}
