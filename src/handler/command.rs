use crate::{agent, config, session, slack};
use std::sync::Arc;
use tracing::debug;

const HELP_MESSAGE: &str = "Available commands:
- #help - Show this help message
- #new <agent> [workspace] - Start a new agent session
- #agents - List available agents
- #session - Show current agent session info
- #sessions - Show all active sessions
- #end - End current agent session
- #read <file_path> - Read local file content
- #diff [args] - Show git diff
- #mode - Show available modes and current mode
- #mode <value> - Switch to a different mode
- #model - Show available models and current model
- #model <value> - Switch to a different model
- !<command> - Execute shell command";

/// Handles bot commands (messages starting with #).
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

            // Validate workspace exists
            if !std::path::Path::new(&workspace_path).is_dir() {
                let _ = slack
                    .send_message(
                        channel,
                        Some(ts),
                        &format!("Workspace does not exist: {}", workspace_path),
                    )
                    .await;
                return;
            }

            let new_session_req = agent_client_protocol::NewSessionRequest::new(workspace_path);

            let session_id = match agent_manager
                .new_session(agent_name, new_session_req, agent_config.auto_approve)
                .await
            {
                Ok(resp) => {
                    // Store initial config options if provided
                    if let Some(config_options) = resp.config_options.clone() {
                        if let Err(e) = session_manager
                            .update_config_options(ts, config_options)
                            .await
                        {
                            debug!("Failed to store initial config options: {}", e);
                        }
                    }
                    // Store deprecated modes if provided
                    if let Some(modes) = resp.modes.clone() {
                        if let Err(e) = session_manager.update_modes(ts, modes).await {
                            debug!("Failed to store initial modes: {}", e);
                        }
                    }
                    resp.session_id
                }
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
                    workspace.clone(),
                    channel.to_string(),
                    session_id.clone(),
                )
                .await
            {
                Ok(_) => {
                    // Set default mode if configured
                    if let Some(default_mode) = &agent_config.default_mode {
                        debug!("Setting default mode: {}", default_mode);
                        if let Some(session) = session_manager.get_session(ts).await {
                            let mode_set = if let Some(config_options) = &session.config_options {
                                if let Some(mode_option) = config_options.iter().find(|opt| {
                                    matches!(
                                        opt.category,
                                        Some(agent_client_protocol::SessionConfigOptionCategory::Mode)
                                    )
                                }) {
                                    let req = agent_client_protocol::SetSessionConfigOptionRequest::new(
                                        session_id.clone(),
                                        mode_option.id.clone(),
                                        default_mode.clone(),
                                    );
                                    agent_manager.set_config_option(agent_name, req).await.is_ok()
                                } else {
                                    false
                                }
                            } else {
                                false
                            };

                            if !mode_set && session.modes.is_some() {
                                let req = agent_client_protocol::SetSessionModeRequest::new(
                                    session_id.clone(),
                                    default_mode.clone(),
                                );
                                let _ = agent_manager.set_mode(agent_name, req).await;
                            }
                        }
                    }

                    let workspace_path =
                        workspace.unwrap_or_else(|| config.bridge.default_workspace.clone());
                    let _ = slack
                        .send_message(
                            channel,
                            Some(ts),
                            &format!("Session started with agent: `{}`\nWorking directory: `{}`\nSend messages in this thread to interact with it.\n\n{}", agent_name, workspace_path, HELP_MESSAGE),
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
        "#sessions" => {
            debug!("Processing #sessions command");
            let sessions = session_manager.list_sessions().await;
            if sessions.is_empty() {
                let _ = slack
                    .send_message(channel, thread_ts, "No active sessions.")
                    .await;
            } else {
                let session_list: Vec<String> = sessions
                    .iter()
                    .map(|(_, session)| {
                        let status = if session.busy { "busy" } else { "idle" };
                        format!(
                            "• Agent: {} | Workspace: {} | Auto-approve: {} | Status: {}",
                            session.agent_name, session.workspace, session.auto_approve, status
                        )
                    })
                    .collect();
                let msg = format!(
                    "Active sessions ({}):\n{}",
                    sessions.len(),
                    session_list.join("\n")
                );
                let _ = slack.send_message(channel, thread_ts, &msg).await;
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
            let session = session_manager.get_session(thread_key).await;
            match session_manager.end_session(thread_key).await {
                Ok(_) => {
                    // Add reaction to user's #new message to mark as ended
                    if let Some(session) = session {
                        let _ = slack
                            .add_reaction(&session.channel, &session.initial_ts, "white_check_mark")
                            .await;
                    }
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

            if parts.len() < 2 {
                let _ = slack
                    .send_message(channel, thread_ts, "Usage: #read <file_path>")
                    .await;
                return;
            }

            let file_path = parts[1];

            // Get workspace from session if in a thread, otherwise use default workspace
            let workspace = if let Some(thread_key) = thread_ts {
                if let Some(session) = session_manager.get_session(thread_key).await {
                    crate::utils::expand_path(&session.workspace)
                } else {
                    crate::utils::expand_path(&config.bridge.default_workspace)
                }
            } else {
                crate::utils::expand_path(&config.bridge.default_workspace)
            };

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

            // Pass all remaining arguments to git diff
            let args: Vec<&str> = parts.iter().skip(1).copied().collect();
            if !args.is_empty() {
                cmd.args(&args);
            }

            match cmd.output() {
                Ok(output) => {
                    let diff = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if diff.is_empty() {
                        let _ = slack
                            .send_message(channel, thread_ts, "No changes to show.")
                            .await;
                    } else {
                        // Generate filename and message based on args
                        let (msg, filename) = if args.is_empty() {
                            ("📝 Diff: (whole repo)".to_string(), "repo.diff".to_string())
                        } else {
                            let args_str = args.join(" ");
                            (
                                format!("📝 Diff: {}", args_str),
                                format!("{}.diff", args_str.replace(['/', ' '], "_")),
                            )
                        };

                        match slack.send_message(channel, thread_ts, &msg).await {
                            Ok(ts) => {
                                let _ = slack
                                    .upload_file(channel, Some(&ts), &diff, &filename, Some("Diff"))
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
        "#mode" => {
            debug!("Processing #mode command in thread_ts={:?}", thread_ts);
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

            let session = session.unwrap();
            if parts.len() < 2 {
                // Show available modes
                // Try config_options first (new API)
                let mode_option_found = if let Some(config_options) = &session.config_options {
                    if let Some(mode_option) = config_options.iter().find(|opt| {
                        matches!(
                            opt.category,
                            Some(agent_client_protocol::SessionConfigOptionCategory::Mode)
                        )
                    }) {
                        if let agent_client_protocol::SessionConfigKind::Select(select) =
                            &mode_option.kind
                        {
                            let current = &select.current_value;
                            let options = match &select.options {
                                agent_client_protocol::SessionConfigSelectOptions::Ungrouped(
                                    opts,
                                ) => opts
                                    .iter()
                                    .map(|opt| {
                                        let marker =
                                            if opt.value == *current { "→" } else { " " };
                                        format!(
                                            "{} `{}` - {}",
                                            marker,
                                            opt.value,
                                            opt.description.as_deref().unwrap_or(&opt.name)
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                agent_client_protocol::SessionConfigSelectOptions::Grouped(
                                    groups,
                                ) => groups
                                    .iter()
                                    .map(|group| {
                                        let opts = group
                                            .options
                                            .iter()
                                            .map(|opt| {
                                                let marker =
                                                    if opt.value == *current { "→" } else { " " };
                                                format!(
                                                    "{} `{}` - {}",
                                                    marker,
                                                    opt.value,
                                                    opt.description.as_deref().unwrap_or(&opt.name)
                                                )
                                            })
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        format!("*{}*\n{}", group.name, opts)
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n\n"),
                                _ => String::from("Unknown option format"),
                            };
                            let msg = format!("Available modes:\n{}", options);
                            let _ = slack.send_message(channel, thread_ts, &msg).await;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                if mode_option_found {
                    return;
                }

                // Fallback to deprecated modes API
                if let Some(modes) = &session.modes {
                    let current = &modes.current_mode_id;
                    let options = modes
                        .available_modes
                        .iter()
                        .map(|mode| {
                            let marker = if mode.id == *current { "→" } else { " " };
                            format!(
                                "{} `{}` - {}",
                                marker,
                                mode.id,
                                mode.description.as_deref().unwrap_or(&mode.name)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let msg = format!("Available modes:\n{}", options);
                    let _ = slack.send_message(channel, thread_ts, &msg).await;
                } else {
                    let _ = slack
                        .send_message(channel, thread_ts, "No mode configuration available.")
                        .await;
                }
            } else {
                // Switch mode
                let mode_value = parts[1].to_string();
                let force_mode = mode_value.ends_with('!');
                let mode_value = if force_mode {
                    mode_value.trim_end_matches('!').to_string()
                } else {
                    mode_value
                };

                // Try config_options first (new API)
                let mode_switched = if force_mode || session.config_options.is_some() {
                    if let Some(config_options) = &session.config_options {
                        if let Some(mode_option) = config_options.iter().find(|opt| {
                            matches!(
                                opt.category,
                                Some(agent_client_protocol::SessionConfigOptionCategory::Mode)
                            )
                        }) {
                            let req = agent_client_protocol::SetSessionConfigOptionRequest::new(
                                session.session_id.clone(),
                                mode_option.id.clone(),
                                mode_value.clone(),
                            );
                            match agent_manager
                                .set_config_option(&session.agent_name, req)
                                .await
                            {
                                Ok(_) => {
                                    let _ = slack
                                        .send_message(
                                            channel,
                                            thread_ts,
                                            &format!("Mode switched to: `{}`", mode_value),
                                        )
                                        .await;
                                    true
                                }
                                Err(e) => {
                                    debug!("Failed to set mode via config_options: {}", e);
                                    false
                                }
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };

                if mode_switched {
                    return;
                }

                // Fallback to deprecated modes API
                if force_mode || session.modes.is_some() {
                    let req = agent_client_protocol::SetSessionModeRequest::new(
                        session.session_id.clone(),
                        mode_value.clone(),
                    );
                    match agent_manager.set_mode(&session.agent_name, req).await {
                        Ok(_) => {
                            let _ = slack
                                .send_message(
                                    channel,
                                    thread_ts,
                                    &format!("Mode switched to: `{}`", mode_value),
                                )
                                .await;
                        }
                        Err(e) => {
                            let _ = slack
                                .send_message(
                                    channel,
                                    thread_ts,
                                    &format!("Failed to switch mode: {}", e),
                                )
                                .await;
                        }
                    }
                } else {
                    let _ = slack
                        .send_message(
                            channel,
                            thread_ts,
                            "No mode configuration available. Use `#mode <value>!` to force set.",
                        )
                        .await;
                }
            }
        }
        "#model" => {
            debug!("Processing #model command in thread_ts={:?}", thread_ts);
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

            let session = session.unwrap();
            if parts.len() < 2 {
                // Show available models
                // Try config_options (new API)
                if let Some(config_options) = &session.config_options {
                    if let Some(model_option) = config_options.iter().find(|opt| {
                        matches!(
                            opt.category,
                            Some(agent_client_protocol::SessionConfigOptionCategory::Model)
                        )
                    }) {
                        if let agent_client_protocol::SessionConfigKind::Select(select) =
                            &model_option.kind
                        {
                            let current = &select.current_value;
                            let options = match &select.options {
                                agent_client_protocol::SessionConfigSelectOptions::Ungrouped(
                                    opts,
                                ) => opts
                                    .iter()
                                    .map(|opt| {
                                        let marker =
                                            if opt.value == *current { "→" } else { " " };
                                        format!(
                                            "{} `{}` - {}",
                                            marker,
                                            opt.value,
                                            opt.description.as_deref().unwrap_or(&opt.name)
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                agent_client_protocol::SessionConfigSelectOptions::Grouped(
                                    groups,
                                ) => groups
                                    .iter()
                                    .map(|group| {
                                        let opts = group
                                            .options
                                            .iter()
                                            .map(|opt| {
                                                let marker =
                                                    if opt.value == *current { "→" } else { " " };
                                                format!(
                                                    "{} `{}` - {}",
                                                    marker,
                                                    opt.value,
                                                    opt.description.as_deref().unwrap_or(&opt.name)
                                                )
                                            })
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        format!("*{}*\n{}", group.name, opts)
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n\n"),
                                _ => String::from("Unknown option format"),
                            };
                            let msg = format!("Available models:\n{}", options);
                            let _ = slack.send_message(channel, thread_ts, &msg).await;
                        } else {
                            let _ = slack
                                .send_message(channel, thread_ts, "Model option is not a selector.")
                                .await;
                        }
                    } else {
                        let _ = slack
                            .send_message(channel, thread_ts, "No model configuration available.")
                            .await;
                    }
                } else {
                    let _ = slack
                        .send_message(channel, thread_ts, "No model configuration available.")
                        .await;
                }
            } else {
                // Switch model
                let model_value = parts[1].to_string();

                // Try config_options (new API)
                if let Some(config_options) = &session.config_options {
                    if let Some(model_option) = config_options.iter().find(|opt| {
                        matches!(
                            opt.category,
                            Some(agent_client_protocol::SessionConfigOptionCategory::Model)
                        )
                    }) {
                        let req = agent_client_protocol::SetSessionConfigOptionRequest::new(
                            session.session_id.clone(),
                            model_option.id.clone(),
                            model_value.clone(),
                        );
                        match agent_manager
                            .set_config_option(&session.agent_name, req)
                            .await
                        {
                            Ok(_) => {
                                let _ = slack
                                    .send_message(
                                        channel,
                                        thread_ts,
                                        &format!("Model switched to: `{}`", model_value),
                                    )
                                    .await;
                            }
                            Err(e) => {
                                let _ = slack
                                    .send_message(
                                        channel,
                                        thread_ts,
                                        &format!("Failed to switch model: {}", e),
                                    )
                                    .await;
                            }
                        }
                    } else {
                        let _ = slack
                            .send_message(channel, thread_ts, "No model configuration available.")
                            .await;
                    }
                } else {
                    let _ = slack
                        .send_message(channel, thread_ts, "No model configuration available.")
                        .await;
                }
            }
        }
        "#help" | _ => {
            let _ = slack.send_message(channel, thread_ts, HELP_MESSAGE).await;
        }
    }
}
