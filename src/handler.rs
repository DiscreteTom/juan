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

const HELP_MESSAGE: &str = "Available commands:
• #help - Show this help message
• #new <name> [workspace] - Start a new agent session in a thread
• #agents - List available agents
• #session - Show current agent session info
• #end - End current agent session
• #read <file_path> - Read local file content
• #diff [file_path] - Show git diff
• !<command> - Execute shell command";

async fn handle_permission_response(
    text: &str,
    options: Vec<agent_client_protocol::PermissionOption>,
    response_tx: tokio::sync::oneshot::Sender<Option<String>>,
    slack: &slack::SlackConnection,
    channel: &str,
    thread_key: &str,
) {
    debug!("Handling permission response: text={}", text);
    let text = text.trim();

    if text.eq_ignore_ascii_case("deny") {
        let _ = slack
            .send_message(channel, Some(thread_key), "❌ Permission denied")
            .await;
        let _ = response_tx.send(None);
        return;
    }

    // Try to parse as a number
    if let Ok(choice) = text.parse::<usize>() {
        if choice > 0 && choice <= options.len() {
            let selected = &options[choice - 1];
            let _ = slack
                .send_message(
                    channel,
                    Some(thread_key),
                    &format!("✅ Approved: {}", selected.name),
                )
                .await;
            let _ = response_tx.send(Some(selected.option_id.to_string()));
            return;
        }
    }

    let _ = slack
        .send_message(
            channel,
            Some(thread_key),
            "❌ Invalid response. Permission denied.",
        )
        .await;
    let _ = response_tx.send(None);
}

/// Main entry point for handling Slack events.
/// Routes events to appropriate handlers based on message content.
pub async fn handle_event(
    event: slack::SlackEvent,
    slack: Arc<slack::SlackConnection>,
    config: Arc<config::Config>,
    agent_manager: Arc<agent::AgentManager>,
    session_manager: Arc<session::SessionManager>,
    message_buffers: bridge::MessageBuffers,
    pending_permissions: bridge::PendingPermissions,
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
            // Check if this is a response to a pending permission request FIRST
            let thread_key = thread_ts.as_deref().unwrap_or(&ts);
            debug!(
                "Checking for pending permission: thread_key={}, pending_count={}",
                thread_key,
                pending_permissions.read().await.len()
            );
            if let Some((options, response_tx)) =
                pending_permissions.write().await.remove(thread_key)
            {
                debug!("Found pending permission request, handling response");
                handle_permission_response(
                    &text,
                    options,
                    response_tx,
                    &slack,
                    &channel,
                    thread_key,
                )
                .await;
                return;
            }

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
/// - #new <name> [workspace] - Start a new session
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
        "#new" => {
            debug!("Processing #new command: parts={:?}", parts);
            // Can only create sessions in main channel, not in existing threads
            if thread_ts.is_some() {
                let _ = slack
                    .send_message(
                        channel,
                        thread_ts,
                        "Cannot create agent in a thread. Use #new in the main channel.",
                    )
                    .await;
                return;
            }

            if parts.len() < 2 {
                let _ = slack
                    .send_message(
                        channel,
                        Some(ts),
                        "Usage: #new <agent_name> [workspace_path]",
                    )
                    .await;
                return;
            }

            let agent_name = parts[1];
            let workspace = parts.get(2).map(|s| s.to_string());

            // Look up agent config
            let agent_config = config.agents.iter().find(|a| a.name == agent_name);
            let agent_config = match agent_config {
                Some(cfg) => cfg,
                None => {
                    let _ = slack
                        .send_message(
                            channel,
                            Some(ts),
                            &format!("Agent not found: {}", agent_name),
                        )
                        .await;
                    return;
                }
            };

            // Spawn agent if not already running
            if agent_manager
                .list_agents()
                .await
                .iter()
                .find(|a| a == &agent_name)
                .is_none()
            {
                debug!("Agent {} not running, spawning...", agent_name);
                if let Err(e) = agent_manager.spawn_agents(vec![agent_config.clone()]).await {
                    let _ = slack
                        .send_message(channel, Some(ts), &format!("Failed to spawn agent: {}", e))
                        .await;
                    return;
                }
            }

            // Create ACP session
            debug!("Creating ACP session for agent={}", agent_name);
            let workspace_path = workspace
                .clone()
                .unwrap_or_else(|| config.bridge.default_workspace.clone());
            let workspace_path = crate::utils::expand_path(&workspace_path);
            let new_session_req = agent_client_protocol::NewSessionRequest::new(workspace_path);

            let session_id = match agent_manager
                .new_session(agent_name, new_session_req, agent_config.auto_approve)
                .await
            {
                Ok(resp) => resp.session_id,
                Err(e) => {
                    let _ = slack
                        .send_message(
                            channel,
                            Some(ts),
                            &format!("Failed to create ACP session: {}", e),
                        )
                        .await;
                    return;
                }
            };

            // Create session
            debug!(
                "Creating session for thread_key={}, agent={}, session_id={}",
                ts, agent_name, session_id
            );
            match session_manager
                .create_session(
                    ts.to_string(),
                    agent_name.to_string(),
                    workspace,
                    channel.to_string(),
                    session_id,
                )
                .await
            {
                Ok(_) => {
                    let _ = slack
                        .send_message(
                            channel,
                            Some(ts),
                            &format!("Session started with agent: {}. Send messages in this thread to chat.\n\n{}", agent_name, HELP_MESSAGE),
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
                    .send_message(
                        channel,
                        None,
                        "This command can only be used in an agent thread.",
                    )
                    .await;
                return;
            }

            let thread_key = thread_ts.unwrap();
            if let Some(session) = session_manager.get_session(thread_key).await {
                let status = if session.busy { "busy" } else { "idle" };
                let msg = format!(
                    "Current session:\n• Agent: {}\n• Workspace: {}\n• Auto-approve: {}\n• Status: {}",
                    session.agent_name, session.workspace, session.auto_approve, status
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
                    .send_message(
                        channel,
                        None,
                        "This command can only be used in an agent thread.",
                    )
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
                    .send_message(
                        channel,
                        None,
                        "This command can only be used in an agent thread.",
                    )
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
                        let msg = format!("📄 File: {}", file_path);
                        match slack.send_message(channel, thread_ts, &msg).await {
                            Ok(ts) => {
                                let _ = slack
                                    .upload_file(
                                        channel,
                                        Some(&ts),
                                        &content,
                                        file_path,
                                        "text",
                                        Some("Content"),
                                    )
                                    .await;
                            }
                            Err(e) => {
                                tracing::error!("Failed to send message: {}", e);
                            }
                        }
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
                    .send_message(
                        channel,
                        None,
                        "This command can only be used in an agent thread.",
                    )
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

            let file_path = if parts.len() >= 2 {
                cmd.arg(parts[1]);
                Some(parts[1])
            } else {
                None
            };

            match cmd.output() {
                Ok(output) => {
                    let diff = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if diff.is_empty() {
                        let _ = slack
                            .send_message(channel, thread_ts, "No changes to show.")
                            .await;
                    } else if let Some(path) = file_path {
                        // Single file - use file upload
                        let msg = format!("📝 Diff: {}", path);
                        match slack.send_message(channel, thread_ts, &msg).await {
                            Ok(ts) => {
                                let filename = format!("{}.diff", path.replace('/', "_"));
                                let _ = slack
                                    .upload_file(
                                        channel,
                                        Some(&ts),
                                        &diff,
                                        &filename,
                                        "diff",
                                        Some("Diff"),
                                    )
                                    .await;
                            }
                            Err(e) => {
                                tracing::error!("Failed to send message: {}", e);
                            }
                        }
                    } else {
                        // Whole repo - use file upload
                        let msg = "📝 Diff: (whole repo)";
                        match slack.send_message(channel, thread_ts, msg).await {
                            Ok(ts) => {
                                let _ = slack
                                    .upload_file(
                                        channel,
                                        Some(&ts),
                                        &diff,
                                        "repo.diff",
                                        "diff",
                                        Some("Diff"),
                                    )
                                    .await;
                            }
                            Err(e) => {
                                tracing::error!("Failed to send message: {}", e);
                            }
                        }
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
            let _ = slack.send_message(channel, thread_ts, HELP_MESSAGE).await;
        }
    }
}

/// Handles regular messages to agents (not commands or shell commands).
///
/// Flow:
/// 1. Check if thread has an active session
/// 2. Send prompt to agent via ACP
/// 3. Update Slack with response
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
                .send_message(channel, thread_ts, "No active session. Use #help for help.")
                .await;
            return;
        }
    };

    // Check if session is busy
    if session.busy {
        let _ = slack
            .send_message(
                channel,
                thread_ts,
                "Session is busy processing a previous message. Please wait.",
            )
            .await;
        return;
    }

    // Mark session as busy
    if let Err(e) = session_manager.set_busy(thread_key, true).await {
        tracing::error!("Failed to set session busy: {}", e);
        return;
    }

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
    // We don't wait for completion to avoid blocking the event loop
    let agent_manager_clone = agent_manager.clone();
    let session_manager_clone = session_manager.clone();
    let slack_clone = slack.clone();
    let channel = channel.to_string();
    let thread_ts = thread_ts.map(|s| s.to_string());
    let thread_key = thread_key.to_string();
    let session_id = session.session_id.clone();
    let agent_name = session.agent_name.clone();

    tokio::spawn(async move {
        match agent_manager_clone.prompt(&agent_name, prompt_req).await {
            Ok(resp) => {
                // Prompt completed - flush any buffered message chunks
                tracing::info!("Prompt completed with stop_reason: {:?}", resp.stop_reason);

                // TODO: optimize this - sleep to ensure all messages are collected
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                if let Some(buffer) = message_buffers.write().await.remove(&session_id) {
                    if !buffer.is_empty() {
                        debug!("Flushing {} chars from message buffer", buffer.len());
                        let _ = slack_clone
                            .send_message(&channel, thread_ts.as_deref(), &buffer)
                            .await;
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to send prompt: {}", e);
                let _ = slack_clone
                    .send_message(&channel, thread_ts.as_deref(), &format!("Error: {}", e))
                    .await;
            }
        }

        // Mark session as not busy
        if let Err(e) = session_manager_clone.set_busy(&thread_key, false).await {
            tracing::error!("Failed to unset session busy: {}", e);
        }
    });
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
