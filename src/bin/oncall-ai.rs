use std::path::PathBuf;

use clap::Parser;
use oncall_ai::cli::{Cli, Command};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run().await,
    }
}

async fn run() -> anyhow::Result<()> {
    let env_home = std::env::var_os("ONCALL_HOME").map(PathBuf::from);
    let (home, home_source) = oncall_ai::config::resolve_home(env_home)?;
    let (config, config_source) = oncall_ai::config::load(&home)?;
    oncall_ai::log::init(&config.log)?;

    let shutdown = Shutdown::install()?;

    tracing::info!(home = %home.display(), source = %home_source, "home resolved");
    tracing::info!(source = %config_source, "config loaded");
    tracing::info!("running; ctrl-C to stop");

    shutdown.wait().await;
    tracing::info!("shutdown signal received, exiting");
    Ok(())
}

#[cfg(unix)]
struct Shutdown {
    int: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
}

#[cfg(not(unix))]
struct Shutdown {}

impl Shutdown {
    #[cfg(unix)]
    fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            int: signal(SignalKind::interrupt())?,
            term: signal(SignalKind::terminate())?,
        })
    }

    #[cfg(not(unix))]
    fn install() -> std::io::Result<Self> {
        Ok(Self {})
    }

    /// Resolves when a shutdown signal arrives.
    #[cfg(unix)]
    async fn wait(mut self) {
        tokio::select! {
            _ = self.int.recv() => {}
            _ = self.term.recv() => {}
        }
    }

    #[cfg(not(unix))]
    async fn wait(self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}
