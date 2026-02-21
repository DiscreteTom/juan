/// Main entry point for the anywhere bridge application.
///
/// This module orchestrates the entire application by:
/// 1. Parsing CLI arguments
/// 2. Loading configuration
/// 3. Initializing managers (agent, session)
/// 4. Connecting to Slack
/// 5. Processing events in the main loop
mod agent_manager;
mod bridge;
mod cli;
mod config;
mod message_handler;
mod session_manager;
mod slack_client;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tracing::info;

use crate::bridge::run_bridge;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse();

    match args.command {
        cli::Command::Init { config, r#override } => {
            return config::Config::init(&config, r#override);
        }
        cli::Command::Run { config, log_level } => {
            // Initialize logging with the specified level
            tracing_subscriber::fmt().with_env_filter(log_level).init();

            info!("Loading configuration from: {}", config);
            let config = Arc::new(config::Config::load(&config)?);

            info!("Configuration loaded successfully");
            run_bridge(config).await?;
        }
    }

    Ok(())
}
