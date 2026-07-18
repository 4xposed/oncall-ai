//! The `oncall-ai` binary.

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use oncall_ai::cli::{Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run().await,
    }
}

/// Resolves home, loads configuration and serves webhooks until shutdown.
async fn run() -> anyhow::Result<()> {
    let env_home = std::env::var_os("ONCALL_HOME").map(PathBuf::from);
    let (home, home_source) = oncall_ai::config::resolve_home(env_home)?;
    let (config, config_source) = oncall_ai::config::load(&home)?;
    oncall_ai::log::init(&config.log)?;

    let shutdown = Shutdown::setup()?;

    let listener = tokio::net::TcpListener::bind(config.webhook.bind)
        .await
        .with_context(|| format!("bind webhook server to {}", config.webhook.bind))?;
    let addr = listener.local_addr().context("read bound address")?;

    tracing::info!(home = %home.display(), source = %home_source, "home resolved");
    tracing::info!(source = %config_source, "config loaded");

    let triager = oncall_ai::triage::Triager::new(&config.triage)
        .with_context(|| format!("initialize triage for endpoint {}", config.triage.endpoint))?;

    let (alerts_tx, alerts_rx) = tokio::sync::mpsc::channel(config.incidents.queue_capacity.get());
    let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(config.triage.queue_capacity.get());
    let (done_tx, done_rx) = tokio::sync::mpsc::channel(oncall_ai::incident::TRIAGED_CAPACITY);
    let worker_shutdown = tokio_util::sync::CancellationToken::new();
    let router = oncall_ai::webhook::router(&config.webhook, alerts_tx);

    // Before the spawns so they precede the "running" readiness line.
    tracing::info!(
        idle_ttl_secs = config.incidents.idle_ttl_secs.get(),
        "incident worker started"
    );
    tracing::info!(
        model = %config.triage.model,
        endpoint = config.triage.endpoint,
        "triage worker started"
    );
    let incident_handle = tokio::spawn(oncall_ai::incident::worker(
        alerts_rx,
        done_rx,
        requests_tx,
        oncall_ai::incident::InMemoryStore::new(),
        config.incidents.idle_ttl(),
        worker_shutdown.clone(),
    ));
    let triage_handle = tokio::spawn(oncall_ai::triage::worker(
        requests_rx,
        triager,
        config.triage.backoff(),
        done_tx,
        worker_shutdown.clone(),
    ));

    tracing::info!(addr = %addr, "running; ctrl-C to stop");

    oncall_ai::webhook::serve(listener, router, async move {
        shutdown.wait().await;
        tracing::info!("shutdown signal received, draining");
    })
    .await
    .context("webhook server failed")?;

    // Only after HTTP drains
    worker_shutdown.cancel();
    if let Err(error) = incident_handle.await {
        tracing::error!(%error, "incident worker task failed");
    }
    if let Err(error) = triage_handle.await {
        tracing::error!(%error, "triage worker task failed");
    }

    tracing::info!("drained, exiting");
    Ok(())
}

/// Shutdown signal listener, setup before the server starts.
#[cfg(unix)]
struct Shutdown {
    int: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
}

/// Shutdown signal listener, setup before the server starts.
#[cfg(not(unix))]
struct Shutdown {}

impl Shutdown {
    #[cfg(unix)]
    fn setup() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            int: signal(SignalKind::interrupt())?,
            term: signal(SignalKind::terminate())?,
        })
    }

    #[cfg(not(unix))]
    fn setup() -> std::io::Result<Self> {
        Ok(Self {})
    }

    #[cfg(unix)]
    async fn wait(mut self) {
        tokio::select! {
            _ = self.int.recv() => {}
            _ = self.term.recv() => {}
        }
    }

    #[cfg(not(unix))]
    async fn wait(self) {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "ctrl-C listener failed, shutting down");
        }
    }
}
