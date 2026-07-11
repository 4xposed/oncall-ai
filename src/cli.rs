use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "oncall-ai", version, about = "AI on-call agent")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    Run,
}
