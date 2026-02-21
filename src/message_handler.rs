/// Message handler for processing Slack events and routing to appropriate handlers.
///
/// This module is the main dispatcher for all Slack events, handling:
/// - Shell commands (starting with !)
/// - Bot commands (starting with #)
/// - Regular messages to agents
use crate::{agent_manager, config, session_manager, slack_client};
use std::sync::Arc;
use tokio::process::Command;

/// Main entry point for handling Slack events.
/// Routes events to appropriate handlers based on message content.
pub async fn handle_event(
    event: slack_client::SlackEvent,
    slack: Arc<slack_client::SlackConnection>,
    config: Arc<config::Config>,
    agent_manager: Arc<agent_manager::AgentManager>,
    session_manager: Arc<session_manager::SessionManager>,
) {
    tracing::info!("Received event: {:?}", event);

    match event {
        slack_client::SlackEvent::Message {
            channel,
            ts,
            thread_ts,
            text,
            ..
        }
        | slack_client::SlackEvent::AppMention {
            channel,
            ts,
            thread_ts,
            text,
            ..
        } => {
            // Shell commands (!) - execute local commands
            if text.trim().starts_with('!') {
                handle_shell_command(&text, &channel, thread_ts.as_deref(), slack).await;
                return;
            }

            // Bot commands (#) - control sessions and agents
            if text.trim().starts_with('#') {
                if handle_command(
                    &text,
                    &channel,
                    &ts,
                    thread_ts.as_deref(),
                    slack.clone(),
                    config.clone(),
                    agent_manager.clone(),
                    session_manager.clone(),
                )
                .await
                {
                    return;
                }
            }

            // Regular messages - forward to agent
            handle_message(
                &text,
                &channel,
                thread_ts.as_deref(),
                slack,
                agent_manager,
                session_manager,
            )
            .await;
        }
    }
}

/// Handles bot commands (messages starting with #).
///
/// Supported commands:
/// - #agent <name> [workspace] - Start a new session
/// - #agents - List available agents
/// - #session - Show current session info
/// - #end - End current session
///
/// Returns true if the message was handled as a command, false otherwise.
async fn handle_command(
    text: &str,
    channel: &str,
    ts: &str,
    thread_ts: Option<&str>,
    slack: Arc<slack_client::SlackConnection>,
    config: Arc<config::Config>,
    agent_manager: Arc<agent_manager::AgentManager>,
    session_manager: Arc<session_manager::SessionManager>,
) -> bool {
    let parts: Vec<&str> = text.trim().split_whitespace().collect();
    let command = parts[0];

    match command {
        "#agent" => {
            // Can only create sessions in main channel, not in existing threads
            if thread_ts.is_some() {
                let _ = slack
                    .send_message(
                        channel,
                        thread_ts,
                        "Cannot create agent in a thread. Use #agent in the main channel.",
                    )
                    .await;
                return true;
            }

            if parts.len() < 2 {
                let _ = slack
                    .send_message(
                        channel,
                        Some(ts),
                        "Usage: #agent <agent_name> [workspace_path]",
                    )
                    .await;
                return true;
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
                let agent_config = config.agents.iter().find(|a| a.name == agent_name);
                if let Some(cfg) = agent_config {
                    if let Err(e) = agent_manager.spawn_agents(vec![cfg.clone()]).await {
                        let _ = slack
                            .send_message(
                                channel,
                                Some(ts),
                                &format!("Failed to spawn agent: {}", e),
                            )
                            .await;
                        return true;
                    }
                } else {
                    let _ = slack
                        .send_message(
                            channel,
                            Some(ts),
                            &format!("Agent not found: {}", agent_name),
                        )
                        .await;
                    return true;
                }
            }

            // Create session (uses ts as thread key, creating a new thread)
            match session_manager
                .create_session(ts.to_string(), agent_name.to_string(), workspace)
                .await
            {
                Ok(_) => {
                    let _ = slack
                        .send_message(
                            channel,
                            Some(ts),
                            &format!("Session started with agent: {}. Send messages in this thread to chat.", agent_name),
                        )
                        .await;
                }
                Err(e) => {
                    let _ = slack
                        .send_message(
                            channel,
                            Some(ts),
                            &format!("Failed to create session: {}", e),
                        )
                        .await;
                }
            }
            true
        }
        "#agents" => {
            // List all configured agents with descriptions
            let agent_list: Vec<String> = config
                .agents
                .iter()
                .map(|a| format!("• {} - {}", a.name, a.description))
                .collect();
            let msg = format!("Available agents:\n{}", agent_list.join("\n"));
            let _ = slack.send_message(channel, thread_ts, &msg).await;
            true
        }
        "#session" => {
            // Show current session info (only works in threads)
            if thread_ts.is_none() {
                let _ = slack
                    .send_message(channel, None, "This command can only be used in a thread.")
                    .await;
                return true;
            }

            let thread_key = thread_ts.unwrap();
            if let Some(session) = session_manager.get_session(thread_key).await {
                let msg = format!(
                    "Current session:\n• Agent: {}\n• Workspace: {}\n• Auto-approve: {}",
                    session.agent_name, session.workspace, session.auto_approve
                );
                let _ = slack.send_message(channel, thread_ts, &msg).await;
            } else {
                let _ = slack
                    .send_message(channel, thread_ts, "No active session in this thread.")
                    .await;
            }
            true
        }
        "#end" => {
            // End current session (only works in threads)
            if thread_ts.is_none() {
                let _ = slack
                    .send_message(channel, None, "This command can only be used in a thread.")
                    .await;
                return true;
            }

            let thread_key = thread_ts.unwrap();
            match session_manager.end_session(thread_key).await {
                Ok(_) => {
                    let _ = slack
                        .send_message(channel, thread_ts, "Session ended.")
                        .await;
                }
                Err(e) => {
                    let _ = slack
                        .send_message(channel, thread_ts, &format!("Error: {}", e))
                        .await;
                }
            }
            true
        }
        // Unknown command starting with # - not handled
        _ if command.starts_with('#') => false,
        _ => false,
    }
}

/// Handles regular messages to agents (not commands or shell commands).
///
/// Flow:
/// 1. Check if thread has an active session
/// 2. Create ACP session if this is the first message
/// 3. Send prompt to agent via ACP
/// 4. Update Slack with response
async fn handle_message(
    text: &str,
    channel: &str,
    thread_ts: Option<&str>,
    slack: Arc<slack_client::SlackConnection>,
    agent_manager: Arc<agent_manager::AgentManager>,
    session_manager: Arc<session_manager::SessionManager>,
) {
    let thread_key = thread_ts.unwrap_or(channel);

    // Verify session exists for this thread
    let session = match session_manager.get_session(thread_key).await {
        Some(s) => s,
        None => {
            let _ = slack
                .send_message(
                    channel,
                    thread_ts,
                    "No active session. Use /agent <name> to start one.",
                )
                .await;
            return;
        }
    };

    // If session ID is still placeholder, create actual ACP session
    if session.session_id.to_string().starts_with("session-") {
        let workspace_path = session_manager.expand_workspace_path(&session.workspace);
        let new_session_req = agent_client_protocol::NewSessionRequest::new(workspace_path);

        match agent_manager
            .new_session(&session.agent_name, new_session_req)
            .await
        {
            Ok(resp) => {
                // Update session with real ACP session ID
                if let Err(e) = session_manager
                    .update_session_id(thread_key, resp.session_id)
                    .await
                {
                    tracing::error!("Failed to update session ID: {}", e);
                }
            }
            Err(e) => {
                tracing::error!("Failed to create ACP session: {}", e);
                let _ = slack
                    .send_message(
                        channel,
                        thread_ts,
                        &format!("Error: Failed to create session: {}", e),
                    )
                    .await;
                return;
            }
        }
    }

    // Get updated session with real session ID
    let session = session_manager.get_session(thread_key).await.unwrap();
    let prompt_req = agent_client_protocol::PromptRequest::new(
        session.session_id.clone(),
        vec![agent_client_protocol::ContentBlock::Text(
            agent_client_protocol::TextContent::new(text.to_string()),
        )],
    );

    // Send "Thinking..." message that will be updated with response
    let thinking_msg = match slack.send_message(channel, thread_ts, "Thinking...").await {
        Ok(ts) => Some(ts),
        Err(e) => {
            tracing::error!("Failed to send thinking message: {}", e);
            None
        }
    };

    // Send prompt to agent and wait for response
    match agent_manager.prompt(&session.agent_name, prompt_req).await {
        Ok(resp) => {
            let response_text = format!("Agent response: {:?}", resp.stop_reason);

            if let Some(msg_ts) = thinking_msg {
                let _ = slack.update_message(channel, &msg_ts, &response_text).await;
            } else {
                let _ = slack.send_message(channel, thread_ts, &response_text).await;
            }
        }
        Err(e) => {
            tracing::error!("Failed to send prompt: {}", e);
            let error_msg = format!("Error: {}", e);

            if let Some(msg_ts) = thinking_msg {
                let _ = slack.update_message(channel, &msg_ts, &error_msg).await;
            } else {
                let _ = slack.send_message(channel, thread_ts, &error_msg).await;
            }
        }
    }
}

/// Handles shell commands (messages starting with !).
/// Executes the command locally and sends output back to Slack.
async fn handle_shell_command(
    text: &str,
    channel: &str,
    thread_ts: Option<&str>,
    slack: Arc<slack_client::SlackConnection>,
) {
    let cmd = text.trim().strip_prefix('!').unwrap_or("").trim();

    if cmd.is_empty() {
        let _ = slack
            .send_message(channel, thread_ts, "Usage: !<command>")
            .await;
        return;
    }

    // Execute command via shell
    let output = Command::new("sh").arg("-c").arg(cmd).output().await;

    // Format response with stdout/stderr
    let response = match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);

            if out.status.success() {
                if stdout.is_empty() && stderr.is_empty() {
                    "Command executed successfully (no output)".to_string()
                } else {
                    format!("```\n{}{}\n```", stdout, stderr)
                }
            } else {
                format!(
                    "Exit code: {}\n```\n{}{}\n```",
                    out.status.code().unwrap_or(-1),
                    stdout,
                    stderr
                )
            }
        }
        Err(e) => format!("Failed to execute command: {}", e),
    };

    let _ = slack.send_message(channel, thread_ts, &response).await;
}
