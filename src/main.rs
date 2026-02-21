mod agent_manager;
mod config;
mod session_manager;
mod slack_client;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "anywhere")]
#[command(about = "Chat-to-ACP Bridge", long_about = None)]
struct Args {
    #[arg(long, default_value = "anywhere.toml")]
    config: String,

    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(args.log_level)
        .init();

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

    // Spawn agents
    let (notification_tx, mut notification_rx) = mpsc::unbounded_channel();
    let agent_manager = Arc::new(agent_manager::AgentManager::new(notification_tx));
    info!("Agent manager initialized (agents will spawn on-demand)");

    // Create session manager
    let session_manager = Arc::new(session_manager::SessionManager::new(config.clone()));
    info!("Session manager initialized");

    // Connect to Slack
    let slack = Arc::new(slack_client::SlackConnection::new(
        config.slack.bot_token.clone(),
    ));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    // Handle agent notifications in background
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

    // Event loop
    while let Some(event) = event_rx.recv().await {
        info!("Received event: {:?}", event);

        match event {
            slack_client::SlackEvent::Message {
                channel,
                thread_ts,
                text,
                ..
            }
            | slack_client::SlackEvent::AppMention {
                channel,
                thread_ts,
                text,
                ..
            } => {
                // Handle commands starting with #
                if text.trim().starts_with('#') {
                    let parts: Vec<&str> = text.trim().split_whitespace().collect();
                    let command = parts[0];

                    match command {
                        "#agent" => {
                            if thread_ts.is_some() {
                                let _ = slack
                                    .send_message(
                                        &channel,
                                        thread_ts.as_deref(),
                                        "Cannot create agent in a thread. Use #agent in the main channel.",
                                    )
                                    .await;
                                continue;
                            }

                            if parts.len() < 2 {
                                let _ = slack
                                    .send_message(
                                        &channel,
                                        None,
                                        "Usage: #agent <agent_name> [workspace_path]",
                                    )
                                    .await;
                                continue;
                            }

                            let agent_name = parts[1];
                            let workspace = parts.get(2).map(|s| s.to_string());

                            // Spawn agent if not already running
                            if agent_manager
                                .list_agents()
                                .await
                                .iter()
                                .find(|a| a == &agent_name)
                                .is_none()
                            {
                                let agent_config =
                                    config.agents.iter().find(|a| a.name == agent_name);
                                if let Some(cfg) = agent_config {
                                    match agent_manager.spawn_agents(vec![cfg.clone()]).await {
                                        Ok(_) => info!("Spawned agent: {}", agent_name),
                                        Err(e) => {
                                            let _ = slack
                                                .send_message(
                                                    &channel,
                                                    None,
                                                    &format!("Failed to spawn agent: {}", e),
                                                )
                                                .await;
                                            continue;
                                        }
                                    }
                                } else {
                                    let _ = slack
                                        .send_message(
                                            &channel,
                                            None,
                                            &format!("Agent not found: {}", agent_name),
                                        )
                                        .await;
                                    continue;
                                }
                            }

                            // Create session and start a thread
                            let thread_ts = match slack
                                .send_message(
                                    &channel,
                                    None,
                                    &format!("Starting session with agent: {}", agent_name),
                                )
                                .await
                            {
                                Ok(ts) => ts,
                                Err(e) => {
                                    let _ = slack
                                        .send_message(
                                            &channel,
                                            None,
                                            &format!("Failed to send message: {}", e),
                                        )
                                        .await;
                                    continue;
                                }
                            };

                            match session_manager
                                .create_session(
                                    thread_ts.clone(),
                                    agent_name.to_string(),
                                    workspace,
                                )
                                .await
                            {
                                Ok(_) => {
                                    let _ = slack
                                        .send_message(
                                            &channel,
                                            Some(&thread_ts),
                                            "Session started! Send messages in this thread to chat with the agent.",
                                        )
                                        .await;
                                }
                                Err(e) => {
                                    let _ = slack
                                        .send_message(
                                            &channel,
                                            Some(&thread_ts),
                                            &format!("Failed to create session: {}", e),
                                        )
                                        .await;
                                }
                            }
                        }
                        "#agents" => {
                            let agent_list: Vec<String> = config
                                .agents
                                .iter()
                                .map(|a| format!("• {} - {}", a.name, a.description))
                                .collect();
                            let msg = format!("Available agents:\n{}", agent_list.join("\n"));
                            let _ = slack
                                .send_message(&channel, thread_ts.as_deref(), &msg)
                                .await;
                        }
                        "#session" => {
                            if thread_ts.is_none() {
                                let _ = slack
                                    .send_message(
                                        &channel,
                                        None,
                                        "This command can only be used in a thread.",
                                    )
                                    .await;
                                continue;
                            }

                            let thread_key = thread_ts.as_ref().unwrap();
                            if let Some(session) = session_manager.get_session(thread_key).await {
                                let msg = format!(
                                    "Current session:\n• Agent: {}\n• Workspace: {}\n• Auto-approve: {}",
                                    session.agent_name, session.workspace, session.auto_approve
                                );
                                let _ = slack
                                    .send_message(&channel, thread_ts.as_deref(), &msg)
                                    .await;
                            } else {
                                let _ = slack
                                    .send_message(
                                        &channel,
                                        thread_ts.as_deref(),
                                        "No active session in this thread.",
                                    )
                                    .await;
                            }
                        }
                        "#end" => {
                            if thread_ts.is_none() {
                                let _ = slack
                                    .send_message(
                                        &channel,
                                        None,
                                        "This command can only be used in a thread.",
                                    )
                                    .await;
                                continue;
                            }

                            let thread_key = thread_ts.as_ref().unwrap();
                            match session_manager.end_session(thread_key).await {
                                Ok(_) => {
                                    let _ = slack
                                        .send_message(
                                            &channel,
                                            thread_ts.as_deref(),
                                            "Session ended.",
                                        )
                                        .await;
                                }
                                Err(e) => {
                                    let _ = slack
                                        .send_message(
                                            &channel,
                                            thread_ts.as_deref(),
                                            &format!("Error: {}", e),
                                        )
                                        .await;
                                }
                            }
                        }
                        _ => {
                            // Not a recognized command, continue to normal message handling
                        }
                    }

                    // If it was a command, don't process as normal message
                    if command.starts_with('#')
                        && matches!(
                            command,
                            "#agent" | "#agents" | "#session" | "#end" | "#read" | "#diff"
                        )
                    {
                        continue;
                    }
                }

                let thread_key = thread_ts.clone().unwrap_or_else(|| channel.clone());

                // Check if session exists
                let session = match session_manager.get_session(&thread_key).await {
                    Some(s) => s,
                    None => {
                        let _ = slack
                            .send_message(
                                &channel,
                                thread_ts.as_deref(),
                                "No active session. Use /agent <name> to start one.",
                            )
                            .await;
                        continue;
                    }
                };

                // Create ACP session if needed
                if session.session_id.to_string().starts_with("session-") {
                    let workspace_path = session_manager.expand_workspace_path(&session.workspace);
                    let new_session_req =
                        agent_client_protocol::NewSessionRequest::new(workspace_path);

                    match agent_manager
                        .new_session(&session.agent_name, new_session_req)
                        .await
                    {
                        Ok(resp) => {
                            if let Err(e) = session_manager
                                .update_session_id(&thread_key, resp.session_id)
                                .await
                            {
                                tracing::error!("Failed to update session ID: {}", e);
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to create ACP session: {}", e);
                            let _ = slack
                                .send_message(
                                    &channel,
                                    thread_ts.as_deref(),
                                    &format!("Error: Failed to create session: {}", e),
                                )
                                .await;
                            continue;
                        }
                    }
                }

                // Send prompt to agent
                let session = session_manager.get_session(&thread_key).await.unwrap();
                let prompt_req = agent_client_protocol::PromptRequest::new(
                    session.session_id.clone(),
                    vec![agent_client_protocol::ContentBlock::Text(
                        agent_client_protocol::TextContent::new(text),
                    )],
                );

                // Send "thinking" message
                let thinking_msg = match slack
                    .send_message(&channel, thread_ts.as_deref(), "Thinking...")
                    .await
                {
                    Ok(ts) => Some(ts),
                    Err(e) => {
                        tracing::error!("Failed to send thinking message: {}", e);
                        None
                    }
                };

                match agent_manager.prompt(&session.agent_name, prompt_req).await {
                    Ok(resp) => {
                        let response_text = format!("Agent response: {:?}", resp.stop_reason);

                        if let Some(msg_ts) = thinking_msg {
                            let _ = slack
                                .update_message(&channel, &msg_ts, &response_text)
                                .await;
                        } else {
                            let _ = slack
                                .send_message(&channel, thread_ts.as_deref(), &response_text)
                                .await;
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to send prompt: {}", e);
                        let error_msg = format!("Error: {}", e);

                        if let Some(msg_ts) = thinking_msg {
                            let _ = slack.update_message(&channel, &msg_ts, &error_msg).await;
                        } else {
                            let _ = slack
                                .send_message(&channel, thread_ts.as_deref(), &error_msg)
                                .await;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
