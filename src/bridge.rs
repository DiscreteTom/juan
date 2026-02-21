use agent_client_protocol::SessionId;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, trace};

use crate::{agent, config, handler, session, slack};

/// Shared message buffer for accumulating agent message chunks
pub type MessageBuffers = Arc<RwLock<HashMap<SessionId, String>>>;

/// Shared map for tracking tool call message timestamps
pub type ToolCallMessages = Arc<RwLock<HashMap<String, (String, String)>>>; // tool_call_id -> (channel, ts)

pub async fn run_bridge(config: Arc<config::Config>) -> Result<()> {
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

    // Create channel for agent notifications (agent -> main loop)
    let (notification_tx, mut notification_rx) = mpsc::unbounded_channel();
    let agent_manager = Arc::new(agent::AgentManager::new(notification_tx));
    info!("Agent manager initialized (agents will spawn on-demand)");

    // Create session manager to track Slack thread -> agent session mappings
    let session_manager = Arc::new(session::SessionManager::new(config.clone()));
    info!("Session manager initialized");

    // Create Slack client and event channel (Slack -> main loop)
    let slack = Arc::new(slack::SlackConnection::new(config.slack.bot_token.clone()));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    // Create shared message buffers for accumulating chunks
    let message_buffers: MessageBuffers = Arc::new(RwLock::new(HashMap::new()));

    // Create shared map for tracking tool call messages
    let tool_call_messages: ToolCallMessages = Arc::new(RwLock::new(HashMap::new()));

    // Spawn task to handle agent notifications and forward to Slack
    let slack_clone = slack.clone();
    let session_manager_clone = session_manager.clone();
    let buffers_clone = message_buffers.clone();
    let tool_messages_clone = tool_call_messages.clone();
    tokio::spawn(async move {
        debug!("Agent notification handler started");

        while let Some((agent_name, notification)) = notification_rx.recv().await {
            trace!(
                "Received notification from agent {}: session={}",
                agent_name, notification.session_id
            );

            // Find the Slack thread for this session
            let session_info = session_manager_clone
                .list_sessions()
                .await
                .into_iter()
                .find(|(_, session)| session.session_id == notification.session_id);

            if let Some((thread_key, session)) = session_info {
                debug!(
                    "Found thread_key {} for session {}",
                    thread_key, notification.session_id
                );

                match notification.update {
                    agent_client_protocol::SessionUpdate::AgentMessageChunk(chunk) => {
                        if let agent_client_protocol::ContentBlock::Text(text) = chunk.content {
                            // Buffer the chunk
                            buffers_clone
                                .write()
                                .await
                                .entry(notification.session_id.clone())
                                .or_insert_with(String::new)
                                .push_str(&text.text);
                        }
                    }
                    agent_client_protocol::SessionUpdate::AgentThoughtChunk(chunk) => {
                        if let agent_client_protocol::ContentBlock::Text(text) = chunk.content {
                            let _ = slack_clone
                                .send_message(
                                    &session.channel,
                                    Some(&thread_key),
                                    &format!("💭 {}", text.text),
                                )
                                .await;
                        }
                    }
                    agent_client_protocol::SessionUpdate::ToolCall(tool_call) => {
                        let input_str = tool_call
                            .raw_input
                            .as_ref()
                            .and_then(|v| serde_yaml_ng::to_string(v).ok())
                            .map(|yaml| {
                                let ticks = crate::utils::safe_backticks(&yaml);
                                format!("{}yaml\n{}\n{}", ticks, yaml, ticks)
                            })
                            .unwrap_or_else(|| "N/A".to_string());
                        let msg = format!("🔧 Tool: {}\nInput: {}", tool_call.title, input_str);
                        if let Ok(ts) = slack_clone
                            .send_message(&session.channel, Some(&thread_key), &msg)
                            .await
                        {
                            tool_messages_clone.write().await.insert(
                                tool_call.tool_call_id.to_string(),
                                (session.channel.clone(), ts),
                            );
                        }
                    }
                    agent_client_protocol::SessionUpdate::ToolCallUpdate(update) => {
                        if let Some(status) = update.fields.status {
                            if matches!(status, agent_client_protocol::ToolCallStatus::Completed) {
                                if let Some((channel, ts)) = tool_messages_clone
                                    .write()
                                    .await
                                    .remove(&update.tool_call_id.to_string())
                                {
                                    let msg =
                                        format!("🔧 Tool {} - ✅ Completed", update.tool_call_id);
                                    let _ = slack_clone.update_message(&channel, &ts, &msg).await;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    // Spawn task to connect to Slack and forward events to main loop
    let slack_clone = slack.clone();
    let app_token = config.slack.app_token.clone();
    tokio::spawn(async move {
        info!("Connecting to Slack...");
        if let Err(e) = slack_clone.connect(app_token, event_tx).await {
            tracing::error!("Slack connection error: {}", e);
        }
    });

    // Main event loop: process Slack events
    info!("Entering main event loop");
    while let Some(event) = event_rx.recv().await {
        debug!("Processing event from main loop");
        handler::handle_event(
            event,
            slack.clone(),
            config.clone(),
            agent_manager.clone(),
            session_manager.clone(),
            message_buffers.clone(),
        )
        .await;
    }

    Ok(())
}
