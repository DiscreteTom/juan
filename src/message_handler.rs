use crate::{agent_manager, config, session_manager, slack_client};
use std::sync::Arc;

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
        _ if command.starts_with('#') => false,
        _ => false,
    }
}

async fn handle_message(
    text: &str,
    channel: &str,
    thread_ts: Option<&str>,
    slack: Arc<slack_client::SlackConnection>,
    agent_manager: Arc<agent_manager::AgentManager>,
    session_manager: Arc<session_manager::SessionManager>,
) {
    let thread_key = thread_ts.unwrap_or(channel);

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

    if session.session_id.to_string().starts_with("session-") {
        let workspace_path = session_manager.expand_workspace_path(&session.workspace);
        let new_session_req = agent_client_protocol::NewSessionRequest::new(workspace_path);

        match agent_manager
            .new_session(&session.agent_name, new_session_req)
            .await
        {
            Ok(resp) => {
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

    let session = session_manager.get_session(thread_key).await.unwrap();
    let prompt_req = agent_client_protocol::PromptRequest::new(
        session.session_id.clone(),
        vec![agent_client_protocol::ContentBlock::Text(
            agent_client_protocol::TextContent::new(text.to_string()),
        )],
    );

    let thinking_msg = match slack.send_message(channel, thread_ts, "Thinking...").await {
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
