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
            return handle_init(&config, r#override);
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

/// Handles the `init` subcommand to generate a scaffold configuration file.
///
/// Creates a default configuration with example values that users can customize.
/// Fails if the file already exists unless --override is specified.
fn handle_init(output: &str, override_existing: bool) -> Result<()> {
    use std::collections::HashMap;
    use toml_scaffold::TomlScaffold;

    if !override_existing && std::path::Path::new(output).exists() {
        anyhow::bail!(
            "File already exists: {}. Use --override to overwrite.",
            output
        );
    }

    // Create a default configuration with example values
    let config = config::Config {
        slack: config::SlackConfig {
            bot_token: "xoxb-your-bot-token".to_string(),
            app_token: "xapp-your-app-token".to_string(),
        },
        bridge: config::BridgeConfig {
            default_workspace: "~".to_string(),
            auto_approve: false,
        },
        agents: vec![config::AgentConfig {
            name: "kiro".to_string(),
            description: "Kiro CLI - https://kiro.dev/cli/".to_string(),
            command: "kiro-cli".to_string(),
            args: vec!["acp".into()],
            env: HashMap::new(),
            auto_approve: false,
        }],
    };

    let scaffold = config.to_scaffold()?;
    std::fs::write(output, scaffold)?;
    println!("Config scaffold written to: {}", output);
    Ok(())
}
