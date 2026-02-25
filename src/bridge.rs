use agent_client_protocol::SessionId;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc, oneshot};
use tracing::{debug, info, trace, warn};

use crate::{agent, config, handler, session, slack};

pub enum NotificationWrapper {
    Agent(agent_client_protocol::SessionNotification),
    PromptCompleted { session_id: SessionId },
}

/// Shared message buffer for accumulating agent message chunks
pub type MessageBuffers = Arc<RwLock<HashMap<SessionId, String>>>;

/// Shared thought buffer for accumulating agent thought chunks
pub type ThoughtBuffers = Arc<RwLock<HashMap<SessionId, String>>>;

/// Shared map for tracking tool call message timestamps
pub type ToolCallMessages =
    Arc<RwLock<HashMap<String, (String, String, agent_client_protocol::ToolCall)>>>; // tool_call_id -> (channel, ts, tool_call)

/// Shared map for tracking pending permission requests
pub type PendingPermissions = Arc<
    RwLock<
        HashMap<
            String, // thread_key
            (
                Vec<agent_client_protocol::PermissionOption>,
                oneshot::Sender<Option<String>>,
            ),
        >,
    >,
>;

pub async fn run_bridge(config: Arc<config::Config>) -> Result<()> {
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
    let (permission_request_tx, mut permission_request_rx) = mpsc::unbounded_channel();
    let agent_manager = Arc::new(agent::AgentManager::new(
        notification_tx.clone(),
        permission_request_tx,
    ));
    debug!("Agent manager initialized (agents will spawn on-demand)");

    // Create session manager to track Slack thread -> agent session mappings
    let session_manager = Arc::new(session::SessionManager::new(config.clone()));
    debug!("Session manager initialized");

    // Create Slack client and event channel (Slack -> main loop)
    let slack = Arc::new(slack::SlackConnection::new(config.slack.bot_token.clone()));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    // Create shared message buffers for accumulating chunks
    let message_buffers: MessageBuffers = Arc::new(RwLock::new(HashMap::new()));
    let thought_buffers: ThoughtBuffers = Arc::new(RwLock::new(HashMap::new()));

    // Create shared map for tracking tool call messages
    let tool_call_messages: ToolCallMessages = Arc::new(RwLock::new(HashMap::new()));

    // Create shared map for tracking pending permission requests
    let pending_permissions: PendingPermissions = Arc::new(RwLock::new(HashMap::new()));

    // Spawn task to handle agent notifications and forward to Slack
    let slack_clone = slack.clone();
    let session_manager_clone = session_manager.clone();
    let buffers_clone = message_buffers.clone();
    let thought_buffers_clone = thought_buffers.clone();
    let tool_messages_clone = tool_call_messages.clone();
    tokio::spawn(async move {
        debug!("Agent notification handler started");

        while let Some(wrapper) = notification_rx.recv().await {
            match wrapper {
                NotificationWrapper::PromptCompleted { session_id } => {
                    let session_info = session_manager_clone.find_by_session_id(&session_id).await;

                    if let Some((thread_key, session)) = session_info {
                        flush_message_buffer(
                            &buffers_clone,
                            &session_id,
                            &slack_clone,
                            &session.channel,
                            &thread_key,
                        )
                        .await;
                        flush_thought_buffer(
                            &thought_buffers_clone,
                            &session_id,
                            &slack_clone,
                            &session.channel,
                            &thread_key,
                        )
                        .await;
                    }
                }
                NotificationWrapper::Agent(notification) => {
                    trace!("Received notification: session={}", notification.session_id);

                    // Find the Slack thread for this session
                    let session_info = session_manager_clone
                        .find_by_session_id(&notification.session_id)
                        .await;

                    if let Some((thread_key, session)) = session_info {
                        debug!(
                            "Found thread_key {} for session {}",
                            thread_key, notification.session_id
                        );

                        let is_message_chunk = matches!(
                            notification.update,
                            agent_client_protocol::SessionUpdate::AgentMessageChunk(_)
                        );
                        let is_thought_chunk = matches!(
                            notification.update,
                            agent_client_protocol::SessionUpdate::AgentThoughtChunk(_)
                        );

                        match notification.update {
                            agent_client_protocol::SessionUpdate::AgentMessageChunk(chunk) => {
                                if let agent_client_protocol::ContentBlock::Text(text) =
                                    chunk.content
                                {
                                    // Buffer the message chunk
                                    buffers_clone
                                        .write()
                                        .await
                                        .entry(notification.session_id.clone())
                                        .or_insert_with(String::new)
                                        .push_str(&text.text);
                                }
                            }
                            agent_client_protocol::SessionUpdate::AgentThoughtChunk(chunk) => {
                                if let agent_client_protocol::ContentBlock::Text(text) =
                                    chunk.content
                                {
                                    // Buffer the thought chunk
                                    thought_buffers_clone
                                        .write()
                                        .await
                                        .entry(notification.session_id.clone())
                                        .or_insert_with(String::new)
                                        .push_str(&text.text);
                                }
                            }
                            agent_client_protocol::SessionUpdate::ConfigOptionUpdate(update) => {
                                // Update stored config options
                                if let Err(e) = session_manager_clone
                                    .update_config_options(&thread_key, update.config_options)
                                    .await
                                {
                                    warn!("Failed to update config options: {}", e);
                                }
                            }
                            agent_client_protocol::SessionUpdate::CurrentModeUpdate(update) => {
                                // Update stored mode (deprecated API)
                                if let Some(modes) = &session.modes {
                                    let mut updated_modes = modes.clone();
                                    updated_modes.current_mode_id = update.current_mode_id;
                                    if let Err(e) = session_manager_clone
                                        .update_modes(&thread_key, updated_modes)
                                        .await
                                    {
                                        warn!("Failed to update mode: {}", e);
                                    }
                                }
                            }
                            agent_client_protocol::SessionUpdate::Plan(plan) => {
                                if !plan.entries.is_empty() {
                                    if let Err(e) = send_plan_message(
                                        &slack_clone,
                                        &session.channel,
                                        &thread_key,
                                        &plan.entries,
                                    )
                                    .await
                                    {
                                        tracing::error!("Failed to post ACP plan block: {}", e);
                                    }
                                }
                            }
                            agent_client_protocol::SessionUpdate::ToolCall(tool_call) => {
                                trace!(
                                    "ToolCall: id={}, title={}, kind={:?}",
                                    tool_call.tool_call_id, tool_call.title, tool_call.kind
                                );

                                let tool_call_id = tool_call.tool_call_id.to_string();

                                // Check if this is a new tool call or an update, and what needs uploading
                                let (msg_ts, channel, needs_raw_input_upload, needs_content_upload) = {
                                    let mut tool_messages = tool_messages_clone.write().await;

                                    if let Some((channel, ts, stored_tool_call)) =
                                        tool_messages.get_mut(&tool_call_id)
                                    {
                                        // Existing tool call - check what changed
                                        let title_changed =
                                            tool_call.title != stored_tool_call.title;
                                        let needs_raw_input =
                                            tool_call.raw_input != stored_tool_call.raw_input;
                                        let needs_content =
                                            tool_call.content != stored_tool_call.content;

                                        // Update message text only if title changed
                                        if title_changed {
                                            let msg = format!("🔧 Tool: {}", tool_call.title);
                                            let _ =
                                                slack_clone.update_message(channel, ts, &msg).await;
                                        }

                                        // Update stored tool call
                                        *stored_tool_call = tool_call.clone();

                                        (
                                            ts.clone(),
                                            channel.clone(),
                                            needs_raw_input,
                                            needs_content,
                                        )
                                    } else {
                                        // New tool call - need to send message and upload everything
                                        drop(tool_messages);
                                        let msg = format!("🔧 Tool: {}", tool_call.title);
                                        match slack_clone
                                            .send_message(&session.channel, Some(&thread_key), &msg)
                                            .await
                                        {
                                            Ok(ts) => {
                                                tool_messages_clone.write().await.insert(
                                                    tool_call_id.clone(),
                                                    (
                                                        session.channel.clone(),
                                                        ts.clone(),
                                                        tool_call.clone(),
                                                    ),
                                                );
                                                (ts, session.channel.clone(), true, true)
                                            }
                                            Err(e) => {
                                                tracing::error!("Failed to send message: {}", e);
                                                continue;
                                            }
                                        }
                                    }
                                };

                                // Upload files only if they changed
                                upload_tool_call_files(
                                    &slack_clone,
                                    &channel,
                                    &msg_ts,
                                    tool_call.raw_input.as_ref(),
                                    &tool_call.content,
                                    needs_raw_input_upload,
                                    needs_content_upload,
                                )
                                .await;
                            }
                            agent_client_protocol::SessionUpdate::ToolCallUpdate(update) => {
                                trace!(
                                    "ToolCallUpdate: id={}, status={:?}, content={:?}",
                                    update.tool_call_id,
                                    update.fields.status,
                                    update.fields.content
                                );

                                let tool_call_id = update.tool_call_id.to_string();

                                // Get current state and check what needs updating
                                let (channel, ts, needs_raw_input_upload, needs_content_upload) = {
                                    let mut tool_messages = tool_messages_clone.write().await;

                                    if let Some((channel, ts, stored_tool_call)) =
                                        tool_messages.get_mut(&tool_call_id)
                                    {
                                        let channel = channel.clone();
                                        let ts = ts.clone();

                                        // Check if title changed
                                        if let Some(title) = &update.fields.title {
                                            if title != &stored_tool_call.title {
                                                let msg = format!("🔧 Tool: {}", title);
                                                let _ = slack_clone
                                                    .update_message(&channel, &ts, &msg)
                                                    .await;
                                                stored_tool_call.title = title.clone();
                                            }
                                        }

                                        let needs_raw_input = update
                                            .fields
                                            .raw_input
                                            .as_ref()
                                            .map(|raw_input| {
                                                stored_tool_call.raw_input.as_ref()
                                                    != Some(raw_input)
                                            })
                                            .unwrap_or(false);

                                        let needs_content = update
                                            .fields
                                            .content
                                            .as_ref()
                                            .map(|content| &stored_tool_call.content != content)
                                            .unwrap_or(false);

                                        // Update stored values
                                        if let Some(raw_input) = &update.fields.raw_input {
                                            if needs_raw_input {
                                                stored_tool_call.raw_input =
                                                    Some(raw_input.clone());
                                            }
                                        }
                                        if let Some(content) = &update.fields.content {
                                            if needs_content {
                                                stored_tool_call.content = content.clone();
                                            }
                                        }

                                        (channel, ts, needs_raw_input, needs_content)
                                    } else {
                                        continue;
                                    }
                                };

                                // Upload files if needed (without holding lock)
                                upload_tool_call_files(
                                    &slack_clone,
                                    &channel,
                                    &ts,
                                    update.fields.raw_input.as_ref(),
                                    update.fields.content.as_ref().unwrap_or(&vec![]),
                                    needs_raw_input_upload,
                                    needs_content_upload,
                                )
                                .await;

                                // Update tool call message when status changes to terminal state
                                if let Some(status) = update.fields.status {
                                    if let Some(status_emoji) = match status {
                                        agent_client_protocol::ToolCallStatus::Completed => {
                                            Some("✅")
                                        }
                                        agent_client_protocol::ToolCallStatus::Failed => Some("❌"),
                                        _ => None,
                                    } {
                                        // Remove from tracking and update the Slack message
                                        if let Some((channel, ts, tool_call)) =
                                            tool_messages_clone.write().await.remove(&tool_call_id)
                                        {
                                            let msg = format!(
                                                "{} Tool: {}",
                                                status_emoji, tool_call.title
                                            );

                                            let _ = slack_clone
                                                .update_message(&channel, &ts, &msg)
                                                .await;
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }

                        // Centralized flush logic: flush buffers if not currently accumulating
                        if !is_message_chunk {
                            flush_message_buffer(
                                &buffers_clone,
                                &notification.session_id,
                                &slack_clone,
                                &session.channel,
                                &thread_key,
                            )
                            .await;
                        }
                        if !is_thought_chunk {
                            flush_thought_buffer(
                                &thought_buffers_clone,
                                &notification.session_id,
                                &slack_clone,
                                &session.channel,
                                &thread_key,
                            )
                            .await;
                        }
                    }
                }
            }
        }
    });

    // Spawn task to handle permission requests from agents
    let slack_clone = slack.clone();
    let session_manager_clone = session_manager.clone();
    let pending_permissions_clone = pending_permissions.clone();
    tokio::spawn(async move {
        debug!("Permission request handler started");

        while let Some(permission_req) = permission_request_rx.recv().await {
            debug!(
                "Received permission request from agent {} for session {}",
                permission_req.agent_name, permission_req.session_id
            );

            // Find the Slack thread for this session
            let session_info = session_manager_clone
                .find_by_session_id(&permission_req.session_id)
                .await;

            if let Some((thread_key, session)) = session_info {
                // Format permission options
                let options_text = permission_req
                    .options
                    .iter()
                    .enumerate()
                    .map(|(i, opt)| format!("{}. {}", i + 1, opt.name))
                    .collect::<Vec<_>>()
                    .join("\n");

                let msg = format!(
                    "⚠️ Permission Required\n\n{}\n\nReply with the number to approve, or 'deny' to reject.",
                    options_text
                );

                if let Err(e) = slack_clone
                    .send_message(&session.channel, Some(&thread_key), &msg)
                    .await
                {
                    tracing::error!("Failed to send permission request message: {}", e);
                    let _ = permission_req.response_tx.send(None);
                    continue;
                }

                // Store the pending permission request
                debug!(
                    "Storing pending permission for thread_key={}, options_count={}",
                    thread_key,
                    permission_req.options.len()
                );
                pending_permissions_clone.write().await.insert(
                    thread_key.clone(),
                    (permission_req.options, permission_req.response_tx),
                );
            } else {
                tracing::error!(
                    "Session not found for permission request: {}",
                    permission_req.session_id
                );
                let _ = permission_req.response_tx.send(None);
            }
        }
    });

    // Spawn task to connect to Slack and forward events to main loop
    let slack_clone = slack.clone();
    let app_token = config.slack.app_token.clone();
    tokio::spawn(async move {
        debug!("Connecting to Slack...");
        if let Err(e) = slack_clone.connect(app_token, event_tx).await {
            tracing::error!("Slack connection error: {}", e);
        }
    });

    // Main event loop: process Slack events
    debug!("Entering main event loop");
    while let Some(event) = event_rx.recv().await {
        debug!("Processing event from main loop");
        // Spawn a new task for each event to prevent blocking
        let slack = slack.clone();
        let config = config.clone();
        let agent_manager = agent_manager.clone();
        let session_manager = session_manager.clone();
        let pending_permissions = pending_permissions.clone();
        let notification_tx = notification_tx.clone();

        tokio::spawn(async move {
            handler::handle_event(
                event,
                slack,
                config,
                agent_manager,
                session_manager,
                pending_permissions,
                notification_tx,
            )
            .await;
        });
    }

    Ok(())
}

async fn upload_yaml_input(
    slack: &slack::SlackConnection,
    channel: &str,
    thread_ts: &str,
    raw_input: Option<&serde_json::Value>,
) {
    if let Some(yaml_content) = raw_input.and_then(|v| serde_yaml_ng::to_string(v).ok()) {
        let trimmed = yaml_content.trim();
        if !trimmed.is_empty() && trimmed != "{}" {
            if let Err(e) = slack
                .upload_file(
                    channel,
                    Some(thread_ts),
                    &yaml_content,
                    "input.yaml",
                    Some("Input"),
                )
                .await
            {
                tracing::error!("Failed to upload YAML file: {}", e);
            }
        }
    }
}

async fn upload_tool_call_content(
    slack: &slack::SlackConnection,
    channel: &str,
    thread_ts: &str,
    content: &[agent_client_protocol::ToolCallContent],
) {
    for item in content {
        match item {
            agent_client_protocol::ToolCallContent::Diff(diff) => {
                let diff_text = if let Some(old_text) = &diff.old_text {
                    generate_unified_diff(old_text, &diff.new_text)
                } else {
                    diff.new_text
                        .lines()
                        .map(|line| format!("+{}", line))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let filename = format!(
                    "{}.diff",
                    diff.path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                );
                if let Err(e) = slack
                    .upload_file(
                        channel,
                        Some(thread_ts),
                        &diff_text,
                        &filename,
                        Some("Diff"),
                    )
                    .await
                {
                    tracing::error!("Failed to upload diff file: {}", e);
                }
            }
            agent_client_protocol::ToolCallContent::Content(content) => {
                if let agent_client_protocol::ContentBlock::Text(text) = &content.content {
                    if let Err(e) = slack
                        .upload_file(
                            channel,
                            Some(thread_ts),
                            &text.text,
                            "context.txt",
                            Some("Context"),
                        )
                        .await
                    {
                        tracing::error!("Failed to upload context file: {}", e);
                    }
                }
            }
            _ => {}
        }
    }
}

fn generate_unified_diff(old_text: &str, new_text: &str) -> String {
    use similar::TextDiff;

    TextDiff::from_lines(old_text, new_text)
        .iter_all_changes()
        .map(|change| format!("{}{}", change.tag(), change.value()))
        .collect()
}

async fn flush_message_buffer(
    buffers: &MessageBuffers,
    session_id: &SessionId,
    slack: &slack::SlackConnection,
    channel: &str,
    thread_key: &str,
) {
    if let Some(buffer) = buffers.write().await.remove(session_id) {
        if !buffer.is_empty() {
            debug!("Flushing {} chars from message buffer", buffer.len());
            let _ = slack.send_message(channel, Some(thread_key), &buffer).await;
        }
    }
}

async fn flush_thought_buffer(
    buffers: &ThoughtBuffers,
    session_id: &SessionId,
    slack: &slack::SlackConnection,
    channel: &str,
    thread_key: &str,
) {
    if let Some(buffer) = buffers.write().await.remove(session_id) {
        if !buffer.is_empty() {
            debug!("Flushing {} chars from thought buffer", buffer.len());
            let _ = slack
                .send_message(channel, Some(thread_key), &format_thought_message(&buffer))
                .await;
        }
    }
}

fn format_thought_message(text: &str) -> String {
    text.lines()
        .map(|line| format!("> {}", line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_plan_block_payload(entries: &[agent_client_protocol::PlanEntry]) -> Value {
    let total = entries.len();
    let completed = entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.status,
                agent_client_protocol::PlanEntryStatus::Completed
            )
        })
        .count();

    let title = if total > 0 && completed == total {
        "Plan finished".to_string()
    } else {
        "Plan updated".to_string()
    };

    let tasks = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            json!({
                "task_id": format!("entry_{:03}", index + 1),
                "title": entry.content,
                "status": map_plan_status_to_slack_status(&entry.status)
            })
        })
        .collect::<Vec<_>>();

    json!({
        "type": "plan",
        "title": title,
        "tasks": tasks
    })
}

fn map_plan_status_to_slack_status(
    status: &agent_client_protocol::PlanEntryStatus,
) -> &'static str {
    match status {
        agent_client_protocol::PlanEntryStatus::Completed => "complete",
        agent_client_protocol::PlanEntryStatus::InProgress => "in_progress",
        agent_client_protocol::PlanEntryStatus::Pending => "pending",
        _ => "pending",
    }
}

fn format_plan_message(entries: &[agent_client_protocol::PlanEntry]) -> String {
    let mut lines = Vec::with_capacity(entries.len() + 1);
    lines.push("*Plan*".to_string());

    for entry in entries {
        let marker = match entry.status {
            agent_client_protocol::PlanEntryStatus::Completed => "[x]",
            agent_client_protocol::PlanEntryStatus::InProgress => "[>]",
            agent_client_protocol::PlanEntryStatus::Pending => "[ ]",
            _ => "[?]",
        };
        lines.push(format!("{} {}", marker, entry.content));
    }

    lines.join("\n")
}

/// Upload tool call files if they changed
async fn upload_tool_call_files(
    slack: &Arc<slack::SlackConnection>,
    channel: &str,
    ts: &str,
    raw_input: Option<&serde_json::Value>,
    content: &[agent_client_protocol::ToolCallContent],
    needs_raw_input: bool,
    needs_content: bool,
) {
    if needs_raw_input {
        upload_yaml_input(slack, channel, ts, raw_input).await;
    }
    if needs_content {
        upload_tool_call_content(slack, channel, ts, content).await;
    }
}

async fn send_plan_message(
    slack: &Arc<slack::SlackConnection>,
    channel: &str,
    thread_key: &str,
    entries: &[agent_client_protocol::PlanEntry],
) -> Result<()> {
    let fallback_text = format_plan_message(entries);
    let plan_block = build_plan_block_payload(entries);

    match slack
        .send_message_with_blocks(channel, Some(thread_key), &fallback_text, vec![plan_block])
        .await
    {
        Ok(_) => Ok(()),
        Err(e) => {
            warn!(
                "Failed to send plan block message, falling back to text message: {}",
                e
            );
            slack
                .send_message(channel, Some(thread_key), &fallback_text)
                .await?;
            Ok(())
        }
    }
}
