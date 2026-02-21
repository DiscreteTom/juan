/// Message handler for processing Slack events and routing to appropriate handlers.
///
/// This module is the main dispatcher for all Slack events, handling:
/// - Shell commands (starting with !)
/// - Bot commands (starting with #)
/// - Regular messages to agents
use crate::{agent, bridge, config, session, slack};
use std::sync::Arc;
use tokio::process::Command;
use tracing::{debug, trace};

/// Main entry point for handling Slack events.
/// Routes events to appropriate handlers based on message content.
pub async fn handle_event(
    event: slack::SlackEvent,
    slack: Arc<slack::SlackConnection>,
    config: Arc<config::Config>,
    agent_manager: Arc<agent::AgentManager>,
    session_manager: Arc<session::SessionManager>,
    message_buffers: bridge::MessageBuffers,
) {
    tracing::info!("Received event: {:?}", event);

    match event {
        slack::SlackEvent::Message {
            channel,
            ts,
            thread_ts,
            text,
            ..
        }
        | slack::SlackEvent::AppMention {
            channel,
            ts,
            thread_ts,
            text,
            ..
        } => {
            // Shell commands (!) - execute local commands
            if text.trim().starts_with('!') {
                handle_shell_command(
                    &text,
                    &channel,
                    thread_ts.as_deref(),
                    slack,
                    session_manager,
                )
                .await;
                return;
            }

            // Bot commands (#) - control sessions and agents
            if text.trim().starts_with('#') {
                handle_command(
                    &text,
                    &channel,
                    &ts,
                    thread_ts.as_deref(),
                    slack.clone(),
                    config.clone(),
                    agent_manager.clone(),
                    session_manager.clone(),
                )
                .await;
                return;
            }

            // Regular messages - forward to agent
            handle_message(
                &text,
                &channel,
                thread_ts.as_deref(),
                slack,
                agent_manager,
                session_manager,
                message_buffers,
            )
            .await;
        }
    }
}

/// Handles bot commands (messages starting with #).
///
/// Supported commands:
/// - #help - Show available commands
/// - #agent <name> [workspace] - Start a new session
/// - #agents - List available agents
/// - #session - Show current session info
/// - #end - End current session
/// - #read <file_path> - Read local file content
/// - #diff [file_path] - Show git diff
async fn handle_command(
    text: &str,
    channel: &str,
    ts: &str,
    thread_ts: Option<&str>,
    slack: Arc<slack::SlackConnection>,
    config: Arc<config::Config>,
    agent_manager: Arc<agent::AgentManager>,
    session_manager: Arc<session::SessionManager>,
) {
    let parts: Vec<&str> = text.trim().split_whitespace().collect();
    let command = parts[0];

    match command {
        "#agent" => {
            debug!("Processing #agent command: parts={:?}", parts);
            // Can only create sessions in main channel, not in existing threads
            if thread_ts.is_some() {
                let _ = slack
                    .send_message(
                        channel,
                        thread_ts,
                        "Cannot create agent in a thread. Use #agent in the main channel.",
                    )
                    .await;
                return;
            }

            if parts.len() < 2 {
                let _ = slack
                    .send_message(
                        channel,
                        Some(ts),
                        "Usage: #agent <agent_name> [workspace_path]",
                    )
                    .await;
                return;
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
                debug!("Agent {} not running, spawning...", agent_name);
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
                        return;
                    }
                } else {
                    let _ = slack
                        .send_message(
                            channel,
                            Some(ts),
                            &format!("Agent not found: {}", agent_name),
                        )
                        .await;
                    return;
                }
            }

            // Create session (uses ts as thread key, creating a new thread)
            debug!(
                "Creating session for thread_key={}, agent={}",
                ts, agent_name
            );
            match session_manager
                .create_session(
                    ts.to_string(),
                    agent_name.to_string(),
                    workspace,
                    channel.to_string(),
                )
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
        }
        "#session" => {
            debug!("Processing #session command in thread_ts={:?}", thread_ts);
            // Show current session info (only works in threads)
            if thread_ts.is_none() {
                let _ = slack
                    .send_message(channel, None, "This command can only be used in a thread.")
                    .await;
                return;
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
        }
        "#end" => {
            debug!("Processing #end command in thread_ts={:?}", thread_ts);
            // End current session (only works in threads)
            if thread_ts.is_none() {
                let _ = slack
                    .send_message(channel, None, "This command can only be used in a thread.")
                    .await;
                return;
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
        }
        "#read" => {
            debug!("Processing #read command in thread_ts={:?}", thread_ts);
            // Read file (only works in threads)
            if thread_ts.is_none() {
                let _ = slack
                    .send_message(channel, None, "This command can only be used in a thread.")
                    .await;
                return;
            }

            if parts.len() < 2 {
                let _ = slack
                    .send_message(channel, thread_ts, "Usage: #read <file_path>")
                    .await;
                return;
            }

            let thread_key = thread_ts.unwrap();
            let session = session_manager.get_session(thread_key).await;
            if session.is_none() {
                let _ = slack
                    .send_message(channel, thread_ts, "No active session in this thread.")
                    .await;
                return;
            }

            let file_path = parts[1];
            let workspace = crate::utils::expand_path(&session.unwrap().workspace);
            let full_path = std::path::Path::new(&workspace).join(file_path);

            if full_path.is_dir() {
                match std::fs::read_dir(&full_path) {
                    Ok(entries) => {
                        let mut files: Vec<String> = entries
                            .filter_map(|e| e.ok())
                            .map(|e| {
                                let name = e.file_name().to_string_lossy().to_string();
                                if e.path().is_dir() {
                                    format!("{}/", name)
                                } else {
                                    name
                                }
                            })
                            .collect();
                        files.sort();
                        let list = files.join("\n");
                        let ticks = crate::utils::safe_backticks(&list);
                        let msg = format!("{}:\n{}\n{}\n{}", file_path, ticks, list, ticks);
                        let _ = slack.send_message(channel, thread_ts, &msg).await;
                    }
                    Err(e) => {
                        let _ = slack
                            .send_message(
                                channel,
                                thread_ts,
                                &format!("Error reading directory: {}", e),
                            )
                            .await;
                    }
                }
            } else {
                match std::fs::read_to_string(&full_path) {
                    Ok(content) => {
                        let ticks = crate::utils::safe_backticks(&content);
                        let msg = format!("{}:\n{}\n{}\n{}", file_path, ticks, content, ticks);
                        let _ = slack.send_message(channel, thread_ts, &msg).await;
                    }
                    Err(e) => {
                        let _ = slack
                            .send_message(channel, thread_ts, &format!("Error reading file: {}", e))
                            .await;
                    }
                }
            }
        }
        "#diff" => {
            debug!("Processing #diff command in thread_ts={:?}", thread_ts);
            // Show git diff (only works in threads)
            if thread_ts.is_none() {
                let _ = slack
                    .send_message(channel, None, "This command can only be used in a thread.")
                    .await;
                return;
            }

            let thread_key = thread_ts.unwrap();
            let session = session_manager.get_session(thread_key).await;
            if session.is_none() {
                let _ = slack
                    .send_message(channel, thread_ts, "No active session in this thread.")
                    .await;
                return;
            }

            let workspace = crate::utils::expand_path(&session.unwrap().workspace);
            let mut cmd = std::process::Command::new("git");
            cmd.arg("diff").current_dir(&workspace);

            if parts.len() >= 2 {
                cmd.arg(parts[1]);
            }

            match cmd.output() {
                Ok(output) => {
                    let diff = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if diff.is_empty() {
                        let _ = slack
                            .send_message(channel, thread_ts, "No changes to show.")
                            .await;
                    } else {
                        let ticks = crate::utils::safe_backticks(&diff);
                        let msg = format!("{}\n{}\n{}", ticks, diff, ticks);
                        let _ = slack.send_message(channel, thread_ts, &msg).await;
                    }
                }
                Err(e) => {
                    let _ = slack
                        .send_message(
                            channel,
                            thread_ts,
                            &format!("Error running git diff: {}", e),
                        )
                        .await;
                }
            }
        }
        "#help" | _ => {
            let _ = slack
                .send_message(
                    channel,
                    thread_ts,
                    "Available commands:\n\
                    • #help - Show this help message\n\
                    • #agent <name> [workspace] - Start a new agent session in a thread\n\
                    • #agents - List available agents\n\
                    • #session - Show current agent session info\n\
                    • #end - End current agent session\n\
                    • #read <file_path> - Read local file content\n\
                    • #diff [file_path] - Show git diff",
                )
                .await;
        }
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
    slack: Arc<slack::SlackConnection>,
    agent_manager: Arc<agent::AgentManager>,
    session_manager: Arc<session::SessionManager>,
    message_buffers: bridge::MessageBuffers,
) {
    let thread_key = thread_ts.unwrap_or(channel);
    debug!(
        "Handling message in thread_key={}, text_len={}",
        thread_key,
        text.len()
    );
    trace!("Message text: {}", text);

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
        debug!("Creating ACP session for thread_key={}", thread_key);
        let workspace_path = crate::utils::expand_path(&session.workspace);
        trace!("Expanded workspace path: {}", workspace_path);
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
    debug!(
        "Sending prompt to agent={}, session_id={}",
        session.agent_name, session.session_id
    );
    let prompt_req = agent_client_protocol::PromptRequest::new(
        session.session_id.clone(),
        vec![agent_client_protocol::ContentBlock::Text(
            agent_client_protocol::TextContent::new(text.to_string()),
        )],
    );

    // Send prompt to agent - response will stream via notifications
    match agent_manager.prompt(&session.agent_name, prompt_req).await {
        Ok(resp) => {
            // Prompt completed - flush any buffered message chunks
            tracing::info!("Prompt completed with stop_reason: {:?}", resp.stop_reason);

            if let Some(buffer) = message_buffers.write().await.remove(&session.session_id) {
                if !buffer.is_empty() {
                    debug!("Flushing {} chars from message buffer", buffer.len());
                    let _ = slack.send_message(channel, thread_ts, &buffer).await;
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to send prompt: {}", e);
            let _ = slack
                .send_message(channel, thread_ts, &format!("Error: {}", e))
                .await;
        }
    }
}

/// Handles shell commands (messages starting with !).
/// Executes the command locally and sends output back to Slack.
async fn handle_shell_command(
    text: &str,
    channel: &str,
    thread_ts: Option<&str>,
    slack: Arc<slack::SlackConnection>,
    session_manager: Arc<session::SessionManager>,
) {
    let cmd = text.trim().strip_prefix('!').unwrap_or("").trim();
    debug!("Executing shell command: {}", cmd);

    if cmd.is_empty() {
        let _ = slack
            .send_message(channel, thread_ts, "Usage: !<command>")
            .await;
        return;
    }

    // Get workspace from session if in a thread
    let workspace = if let Some(thread) = thread_ts {
        if let Some(session) = session_manager.get_session(thread).await {
            Some(crate::utils::expand_path(&session.workspace))
        } else {
            None
        }
    } else {
        None
    };

    // Execute command via shell
    let mut command = if cfg!(windows) {
        let mut cmd_process = Command::new("cmd");
        cmd_process.arg("/C").arg(cmd);
        cmd_process
    } else {
        let mut sh_process = Command::new("sh");
        sh_process.arg("-c").arg(cmd);
        sh_process
    };

    if let Some(dir) = workspace {
        command.current_dir(dir);
    }

    let output = command.output().await;

    // Format response with stdout/stderr
    let response = match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

            if out.status.success() {
                if stdout.is_empty() && stderr.is_empty() {
                    "Command executed successfully (no output)".to_string()
                } else if stderr.is_empty() {
                    let ticks = crate::utils::safe_backticks(&stdout);
                    format!("{}\n{}\n{}", ticks, stdout, ticks)
                } else {
                    let stdout_ticks = crate::utils::safe_backticks(&stdout);
                    let stderr_ticks = crate::utils::safe_backticks(&stderr);
                    format!(
                        "{}\n{}\n{}\n\nStderr:\n{}\n{}\n{}",
                        stdout_ticks, stdout, stdout_ticks, stderr_ticks, stderr, stderr_ticks
                    )
                }
            } else {
                if stderr.is_empty() {
                    let ticks = crate::utils::safe_backticks(&stdout);
                    format!(
                        "Exit code: {}\n{}\n{}\n{}",
                        out.status.code().unwrap_or(-1),
                        ticks,
                        stdout,
                        ticks
                    )
                } else {
                    let stdout_ticks = crate::utils::safe_backticks(&stdout);
                    let stderr_ticks = crate::utils::safe_backticks(&stderr);
                    format!(
                        "Exit code: {}\n{}\n{}\n{}\n\nStderr:\n{}\n{}\n{}",
                        out.status.code().unwrap_or(-1),
                        stdout_ticks,
                        stdout,
                        stdout_ticks,
                        stderr_ticks,
                        stderr,
                        stderr_ticks
                    )
                }
            }
        }
        Err(e) => format!("Failed to execute command: {}", e),
    };

    let _ = slack.send_message(channel, thread_ts, &response).await;
}
