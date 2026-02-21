mod agent_manager;
mod cli;
mod config;
mod message_handler;
mod session_manager;
mod slack_client;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(args.log_level)
        .init();

    if let Some(cli::Command::Init { config, r#override }) = args.command {
        return handle_init(&config, r#override);
    }

    info!("Loading configuration from: {}", args.config);
    let config = Arc::new(config::Config::load(&args.config)?);

    info!("Configuration loaded successfully");
    info!("Slack bot configured");
    info!("Default workspace: {}", config.bridge.default_workspace);
    info!("Auto-approve: {}", config.bridge.auto_approve);
    info!("Configured agents: {}", config.agents.len());

    for agent in &config.agents {
        info!(
            "  - {} ({}): {}",
            agent.name, agent.command, agent.description
        );
    }

    let (notification_tx, mut notification_rx) = mpsc::unbounded_channel();
    let agent_manager = Arc::new(agent_manager::AgentManager::new(notification_tx));
    info!("Agent manager initialized (agents will spawn on-demand)");

    let session_manager = Arc::new(session_manager::SessionManager::new(config.clone()));
    info!("Session manager initialized");

    let slack = Arc::new(slack_client::SlackConnection::new(
        config.slack.bot_token.clone(),
    ));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    let slack_clone = slack.clone();
    let session_manager_clone = session_manager.clone();
    tokio::spawn(async move {
        while let Some((_agent_name, notification)) = notification_rx.recv().await {
            match notification.update {
                agent_client_protocol::SessionUpdate::AgentMessageChunk(chunk) => {
                    if let agent_client_protocol::ContentBlock::Text(text) = chunk.content {
                        for (thread_key, session) in session_manager_clone.list_sessions().await {
                            if session.session_id == notification.session_id {
                                let _ = slack_clone
                                    .send_message(&thread_key, None, &text.text)
                                    .await;
                                break;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    });

    let slack_clone = slack.clone();
    let app_token = config.slack.app_token.clone();
    tokio::spawn(async move {
        if let Err(e) = slack_clone.connect(app_token, event_tx).await {
            tracing::error!("Slack connection error: {}", e);
        }
    });

    while let Some(event) = event_rx.recv().await {
        message_handler::handle_event(
            event,
            slack.clone(),
            config.clone(),
            agent_manager.clone(),
            session_manager.clone(),
        )
        .await;
    }

    Ok(())
}

fn handle_init(output: &str, override_existing: bool) -> Result<()> {
    use std::collections::HashMap;
    use toml_scaffold::TomlScaffold;

    if !override_existing && std::path::Path::new(output).exists() {
        anyhow::bail!(
            "File already exists: {}. Use --override to overwrite.",
            output
        );
    }

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
