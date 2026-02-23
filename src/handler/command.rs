use crate::{agent, config, session, slack};
use std::sync::Arc;
use tracing::debug;

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
pub async fn handle_command(
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

const HELP_MESSAGE: &str = "Available commands:
• #help - Show this help message
• #new <name> [workspace] - Start a new agent session in a thread
• #agents - List available agents
• #session - Show current agent session info
• #end - End current agent session
• #read <file_path> - Read local file content
• #diff [file_path] - Show git diff
• !<command> - Execute shell command";
