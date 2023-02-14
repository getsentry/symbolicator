use clap::Parser;

use settings::{Cli, Commands};

mod event;
mod oauth;
mod settings;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt::init();

    match cli.command {
        Commands::Event(args) => event::process_event(args),
        Commands::Auth => oauth::authenticate().await,
    }
}
