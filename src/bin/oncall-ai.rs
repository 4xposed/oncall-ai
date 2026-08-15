//! The `oncall-ai` binary.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use oncall_ai::config::{InvestigationConfig, ModelProvider};
use oncall_ai::investigation::{Investigation, InvestigationRequest};
use rig_agent::agent::Agent;
use rig_core::client::{Nothing, ProviderClient as _};
use rig_core::completion::CompletionModel;
use rig_core::providers::{anthropic, ollama};

#[derive(Parser)]
#[command(name = "oncall-ai", version, about = "AI on-call agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Run,
}

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

    let triager = oncall_ai::triage::Triager::new(&config.triage).context("initialize triage")?;

    let (alerts_tx, alerts_rx) = tokio::sync::mpsc::channel(config.incidents.queue_capacity.get());
    let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(config.triage.queue_capacity.get());
    let (done_tx, done_rx) = tokio::sync::mpsc::channel(oncall_ai::incident::TRIAGED_CAPACITY);
    let (investigation_tx, investigation_rx) =
        tokio::sync::mpsc::channel(config.investigation.queue_capacity.get());
    let (investigated_tx, investigated_rx) =
        tokio::sync::mpsc::channel(oncall_ai::incident::INVESTIGATED_CAPACITY);
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
        oncall_ai::incident::Channels {
            alerts_rx,
            triaged_rx: done_rx,
            investigated_rx,
            triage_tx: requests_tx,
            investigation_tx,
        },
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
    let investigation_handle = match config.investigation.model.provider() {
        ModelProvider::Ollama => {
            let client = ollama::Client::builder()
                .api_key(Nothing)
                .base_url(&config.investigation.endpoint)
                .build()
                .context("build ollama investigation client")?;
            let agent = oncall_ai::investigation::build_agent(&client, &config.investigation)
                .context("initialize investigation")?;
            spawn_investigation(
                agent,
                config.investigation,
                investigation_rx,
                investigated_tx,
                worker_shutdown.clone(),
            )
        }
        ModelProvider::Anthropic => {
            let api_key = require_anthropic_api_key(std::env::var("ANTHROPIC_API_KEY").ok())?;
            let client = build_anthropic_investigation_client(api_key)?;
            let agent = oncall_ai::investigation::build_agent(&client, &config.investigation)
                .context("initialize investigation")?;
            spawn_investigation(
                agent,
                config.investigation,
                investigation_rx,
                investigated_tx,
                worker_shutdown.clone(),
            )
        }
    };

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
    if let Err(error) = investigation_handle.await {
        tracing::error!(%error, "investigation worker task failed");
    }

    tracing::info!("drained, exiting");
    Ok(())
}

fn require_anthropic_api_key(api_key: Option<String>) -> anyhow::Result<String> {
    api_key.filter(|key| !key.is_empty()).context(
        "ANTHROPIC_API_KEY must be set when investigation.model names anthropic; \
         the key is read from the environment, never from config",
    )
}

fn build_anthropic_investigation_client(api_key: String) -> anyhow::Result<anthropic::Client> {
    anthropic::Client::from_val(api_key).context("build anthropic investigation client")
}

fn spawn_investigation(
    (agent, repo_root): (Agent<impl CompletionModel + 'static>, PathBuf),
    config: InvestigationConfig,
    requests_rx: tokio::sync::mpsc::Receiver<InvestigationRequest>,
    done_tx: tokio::sync::mpsc::Sender<Investigation>,
    shutdown: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<usize> {
    tracing::info!(
        model = %config.model,
        max_turns = config.max_turns.get(),
        repo_root = %repo_root.display(),
        "investigation worker started"
    );
    tokio::spawn(oncall_ai::investigation::worker(
        agent,
        config,
        requests_rx,
        done_tx,
        shutdown,
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_api_key_must_be_present_and_nonempty() {
        let missing = require_anthropic_api_key(None).expect_err("missing key must fail");
        let empty =
            require_anthropic_api_key(Some(String::new())).expect_err("empty key must fail");

        assert!(missing.to_string().contains("ANTHROPIC_API_KEY"));
        assert!(empty.to_string().contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn anthropic_api_key_preserves_a_valid_value() {
        assert_eq!(
            require_anthropic_api_key(Some("secret".to_owned())).expect("nonempty key is valid"),
            "secret"
        );
    }

    /// Characterization coverage for the provider construction path used by
    /// the Anthropic arm in `run`.
    #[test]
    fn anthropic_client_construction_adds_context_and_preserves_its_source() {
        let error = build_anthropic_investigation_client("invalid\napi-key".to_owned())
            .expect_err("an API key with a newline is not a valid HTTP header value");

        assert!(
            error
                .to_string()
                .contains("build anthropic investigation client"),
            "missing Anthropic client construction context: {error:#}"
        );
        assert!(
            error.chain().nth(1).is_some(),
            "client construction error must retain an underlying source: {error:#}"
        );
    }
}
