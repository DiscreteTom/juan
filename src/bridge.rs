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
pub type ToolCallMessages =
    Arc<RwLock<HashMap<String, (String, String, agent_client_protocol::ToolCall)>>>; // tool_call_id -> (channel, ts, tool_call)

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
                        trace!(
                            "ToolCall: id={}, title={}, kind={:?}",
                            tool_call.tool_call_id, tool_call.title, tool_call.kind
                        );

                        // Check if there's a diff in content
                        let has_diff = tool_call.content.iter().any(|item| {
                            matches!(item, agent_client_protocol::ToolCallContent::Diff(_))
                        });

                        let input_str = if has_diff {
                            String::new()
                        } else {
                            tool_call
                                .raw_input
                                .as_ref()
                                .and_then(|v| serde_yaml_ng::to_string(v).ok())
                                .map(|yaml| {
                                    let ticks = crate::utils::safe_backticks(&yaml);
                                    format!("\nInput: \n{}\n{}\n{}", ticks, yaml, ticks)
                                })
                                .unwrap_or_default()
                        };

                        let mut msg = format!("🔧 Tool: {}{}", tool_call.title, input_str);

                        // Render content if present
                        for item in &tool_call.content {
                            match item {
                                agent_client_protocol::ToolCallContent::Diff(diff) => {
                                    // Generate unified diff format
                                    let diff_text = if let Some(old_text) = &diff.old_text {
                                        format!(
                                            "--- {}\n+++ {}\n{}",
                                            diff.path.display(),
                                            diff.path.display(),
                                            generate_unified_diff(old_text, &diff.new_text)
                                        )
                                    } else {
                                        // New file
                                        format!(
                                            "--- /dev/null\n+++ {}\n{}",
                                            diff.path.display(),
                                            diff.new_text
                                                .lines()
                                                .map(|line| format!("+{}", line))
                                                .collect::<Vec<_>>()
                                                .join("\n")
                                        )
                                    };
                                    let ticks = crate::utils::safe_backticks(&diff_text);
                                    msg.push_str(&format!(
                                        "\nOutput:\n{}\n{}\n{}",
                                        ticks, diff_text, ticks
                                    ));
                                }
                                _ => {
                                    let content_str = format!("{:?}", item);
                                    let ticks = crate::utils::safe_backticks(&content_str);
                                    msg.push_str(&format!(
                                        "\nOutput:\n{}\n{}\n{}",
                                        ticks, content_str, ticks
                                    ));
                                }
                            }
                        }

                        let tool_call_id = tool_call.tool_call_id.to_string();
                        let mut tool_messages = tool_messages_clone.write().await;

                        if let Some((channel, ts, _)) = tool_messages.get(&tool_call_id).cloned() {
                            // Update existing message
                            let _ = slack_clone.update_message(&channel, &ts, &msg).await;
                            tool_messages.insert(tool_call_id, (channel, ts, tool_call));
                        } else {
                            // Send new message
                            drop(tool_messages);
                            if let Ok(ts) = slack_clone
                                .send_message(&session.channel, Some(&thread_key), &msg)
                                .await
                            {
                                tool_messages_clone
                                    .write()
                                    .await
                                    .insert(tool_call_id, (session.channel.clone(), ts, tool_call));
                            }
                        }
                    }
                    agent_client_protocol::SessionUpdate::ToolCallUpdate(update) => {
                        trace!(
                            "ToolCallUpdate: id={}, status={:?}, content={:?}",
                            update.tool_call_id, update.fields.status, update.fields.content
                        );
                        if let Some(status) = update.fields.status {
                            let is_terminal = matches!(
                                status,
                                agent_client_protocol::ToolCallStatus::Completed
                                    | agent_client_protocol::ToolCallStatus::Failed
                            );

                            if is_terminal {
                                if let Some((channel, ts, tool_call)) = tool_messages_clone
                                    .write()
                                    .await
                                    .remove(&update.tool_call_id.to_string())
                                {
                                    // Check if there's a diff in content
                                    let has_diff = tool_call.content.iter().any(|item| {
                                        matches!(
                                            item,
                                            agent_client_protocol::ToolCallContent::Diff(_)
                                        )
                                    });

                                    let input_str = if has_diff {
                                        String::new()
                                    } else {
                                        tool_call
                                            .raw_input
                                            .as_ref()
                                            .and_then(|v| serde_yaml_ng::to_string(v).ok())
                                            .map(|yaml| {
                                                let ticks = crate::utils::safe_backticks(&yaml);
                                                format!("\nInput: \n{}\n{}\n{}", ticks, yaml, ticks)
                                            })
                                            .unwrap_or_default()
                                    };

                                    let status_emoji = match status {
                                        agent_client_protocol::ToolCallStatus::Completed => "✅",
                                        agent_client_protocol::ToolCallStatus::Failed => "❌",
                                        _ => unreachable!(),
                                    };

                                    let mut msg = format!(
                                        "{} Tool: {}{}",
                                        status_emoji, tool_call.title, input_str
                                    );

                                    // Render content from original tool call
                                    for item in &tool_call.content {
                                        match item {
                                            agent_client_protocol::ToolCallContent::Diff(diff) => {
                                                // Generate unified diff format
                                                let diff_text =
                                                    if let Some(old_text) = &diff.old_text {
                                                        format!(
                                                            "--- {}\n+++ {}\n{}",
                                                            diff.path.display(),
                                                            diff.path.display(),
                                                            generate_unified_diff(
                                                                old_text,
                                                                &diff.new_text
                                                            )
                                                        )
                                                    } else {
                                                        // New file
                                                        format!(
                                                            "--- /dev/null\n+++ {}\n{}",
                                                            diff.path.display(),
                                                            diff.new_text
                                                                .lines()
                                                                .map(|line| format!("+{}", line))
                                                                .collect::<Vec<_>>()
                                                                .join("\n")
                                                        )
                                                    };
                                                let ticks =
                                                    crate::utils::safe_backticks(&diff_text);
                                                msg.push_str(&format!(
                                                    "\nOutput:\n{}\n{}\n{}",
                                                    ticks, diff_text, ticks
                                                ));
                                            }
                                            _ => {
                                                let content_str = format!("{:?}", item);
                                                let ticks =
                                                    crate::utils::safe_backticks(&content_str);
                                                msg.push_str(&format!(
                                                    "\nOutput:\n{}\n{}\n{}",
                                                    ticks, content_str, ticks
                                                ));
                                            }
                                        }
                                    }

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

fn generate_unified_diff(old_text: &str, new_text: &str) -> String {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();

    let mut result = Vec::new();
    let mut i = 0;
    let mut j = 0;

    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() && old_lines[i] == new_lines[j] {
            result.push(format!(" {}", old_lines[i]));
            i += 1;
            j += 1;
        } else {
            // Simple diff: show removals then additions
            while i < old_lines.len() && (j >= new_lines.len() || old_lines[i] != new_lines[j]) {
                result.push(format!("-{}", old_lines[i]));
                i += 1;
            }
            while j < new_lines.len() && (i >= old_lines.len() || old_lines[i] != new_lines[j]) {
                result.push(format!("+{}", new_lines[j]));
                j += 1;
            }
        }
    }

    result.join("\n")
}
