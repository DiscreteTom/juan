use agent_client_protocol::SessionId;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc, oneshot};
use tracing::{debug, info, trace, warn};

use crate::{agent, config, handler, session, slack};

/// Shared message buffer for accumulating agent message chunks
pub type MessageBuffers = Arc<RwLock<HashMap<SessionId, String>>>;

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

pub type PlanBuffers = Arc<RwLock<HashMap<SessionId, Vec<agent_client_protocol::PlanEntry>>>>;
pub type PlanMessages = Arc<RwLock<HashMap<SessionId, (String, String)>>>;
pub type RealPlanSessions = Arc<RwLock<HashSet<SessionId>>>;
pub type ThoughtPlanBuffers = Arc<RwLock<HashMap<SessionId, Vec<DerivedPlanTask>>>>;
pub type ThoughtPlanCompleted = Arc<RwLock<HashSet<SessionId>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
enum DerivedPlanTaskKind {
    Thought,
    Tool,
}

#[derive(Clone, Debug)]
pub struct DerivedPlanTask {
    task_id: String,
    title: String,
    status: String,
    details: Option<String>,
    output: Option<String>,
    kind: DerivedPlanTaskKind,
}

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
    let (permission_request_tx, mut permission_request_rx) = mpsc::unbounded_channel();
    let agent_manager = Arc::new(agent::AgentManager::new(
        notification_tx,
        permission_request_tx,
    ));
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

    // Create shared map for tracking pending permission requests
    let pending_permissions: PendingPermissions = Arc::new(RwLock::new(HashMap::new()));

    let plan_buffers: PlanBuffers = Arc::new(RwLock::new(HashMap::new()));
    let plan_messages: PlanMessages = Arc::new(RwLock::new(HashMap::new()));
    let real_plan_sessions: RealPlanSessions = Arc::new(RwLock::new(HashSet::new()));
    let thought_plan_buffers: ThoughtPlanBuffers = Arc::new(RwLock::new(HashMap::new()));
    let thought_plan_completed: ThoughtPlanCompleted = Arc::new(RwLock::new(HashSet::new()));

    // Spawn task to handle agent notifications and forward to Slack
    let slack_clone = slack.clone();
    let session_manager_clone = session_manager.clone();
    let buffers_clone = message_buffers.clone();
    let tool_messages_clone = tool_call_messages.clone();
    let plan_buffers_clone = plan_buffers.clone();
    let plan_messages_clone = plan_messages.clone();
    let real_plan_sessions_clone = real_plan_sessions.clone();
    let thought_plan_buffers_clone = thought_plan_buffers.clone();
    let thought_plan_completed_clone = thought_plan_completed.clone();
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

                            let has_native_plan = real_plan_sessions_clone
                                .read()
                                .await
                                .contains(&notification.session_id);
                            if !has_native_plan {
                                let should_finalize = {
                                    let completed = thought_plan_completed_clone.read().await;
                                    !completed.contains(&notification.session_id)
                                };

                                if should_finalize {
                                    let tasks = {
                                        let mut buffers = thought_plan_buffers_clone.write().await;
                                        if let Some(entries) =
                                            buffers.get_mut(&notification.session_id)
                                        {
                                            finalize_in_progress_thought_tasks(entries);
                                        }
                                        buffers
                                            .get(&notification.session_id)
                                            .cloned()
                                            .unwrap_or_default()
                                    };

                                    if !tasks.is_empty() {
                                        if let Err(e) = upsert_thought_plan_message(
                                            &slack_clone,
                                            &plan_messages_clone,
                                            &notification.session_id,
                                            &session.channel,
                                            &thread_key,
                                            &tasks,
                                            true,
                                        )
                                        .await
                                        {
                                            tracing::error!(
                                                "Failed to post derived completed plan block: {}",
                                                e
                                            );
                                        }
                                        thought_plan_completed_clone
                                            .write()
                                            .await
                                            .insert(notification.session_id.clone());
                                    }
                                }
                            }
                        }
                    }
                    agent_client_protocol::SessionUpdate::ConfigOptionUpdate(update) => {
                        // Update stored config options
                        if let Err(e) = session_manager_clone
                            .update_config_options(&thread_key, update.config_options)
                            .await
                        {
                            debug!("Failed to update config options: {}", e);
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
                                debug!("Failed to update mode: {}", e);
                            }
                        }
                    }
                    agent_client_protocol::SessionUpdate::Plan(plan) => {
                        real_plan_sessions_clone
                            .write()
                            .await
                            .insert(notification.session_id.clone());
                        thought_plan_buffers_clone
                            .write()
                            .await
                            .remove(&notification.session_id);
                        thought_plan_completed_clone
                            .write()
                            .await
                            .remove(&notification.session_id);

                        let entries = {
                            let mut plans = plan_buffers_clone.write().await;
                            let session_plan =
                                plans.entry(notification.session_id.clone()).or_default();
                            *session_plan = plan.entries.clone();
                            session_plan.clone()
                        };

                        if !entries.is_empty() {
                            if let Err(e) = upsert_plan_message(
                                &slack_clone,
                                &plan_messages_clone,
                                &notification.session_id,
                                &session.channel,
                                &thread_key,
                                &entries,
                            )
                            .await
                            {
                                tracing::error!("Failed to post ACP plan block: {}", e);
                            }
                        }
                    }
                    agent_client_protocol::SessionUpdate::AgentThoughtChunk(chunk) => {
                        if let agent_client_protocol::ContentBlock::Text(text) = chunk.content {
                            trace!("Thought chunk (len={})", text.text.len());

                            let has_native_plan = real_plan_sessions_clone
                                .read()
                                .await
                                .contains(&notification.session_id);
                            if !has_native_plan {
                                if let Some((title, details)) = extract_thought_task(&text.text) {
                                    let tasks = {
                                        let mut buffers = thought_plan_buffers_clone.write().await;
                                        let entries = buffers
                                            .entry(notification.session_id.clone())
                                            .or_insert_with(Vec::new);
                                        upsert_thought_task(entries, title, details);
                                        entries.clone()
                                    };

                                    if !tasks.is_empty() {
                                        thought_plan_completed_clone
                                            .write()
                                            .await
                                            .remove(&notification.session_id);
                                        if let Err(e) = upsert_thought_plan_message(
                                            &slack_clone,
                                            &plan_messages_clone,
                                            &notification.session_id,
                                            &session.channel,
                                            &thread_key,
                                            &tasks,
                                            false,
                                        )
                                        .await
                                        {
                                            tracing::error!(
                                                "Failed to post derived in-progress plan block: {}",
                                                e
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    agent_client_protocol::SessionUpdate::ToolCall(tool_call) => {
                        // Flush accumulated message chunks before tool call
                        if let Some(buffer) =
                            buffers_clone.write().await.remove(&notification.session_id)
                        {
                            if !buffer.is_empty() {
                                let _ = slack_clone
                                    .send_message(&session.channel, Some(&thread_key), &buffer)
                                    .await;
                            }
                        }
                        trace!(
                            "ToolCall: id={}, title={}, kind={:?}",
                            tool_call.tool_call_id, tool_call.title, tool_call.kind
                        );

                        let has_native_plan = real_plan_sessions_clone
                            .read()
                            .await
                            .contains(&notification.session_id);
                        if !has_native_plan {
                            let tasks = {
                                let mut buffers = thought_plan_buffers_clone.write().await;
                                let entries = buffers
                                    .entry(notification.session_id.clone())
                                    .or_insert_with(Vec::new);
                                upsert_tool_task_from_tool_call(entries, &tool_call);
                                entries.clone()
                            };

                            if !tasks.is_empty() {
                                thought_plan_completed_clone
                                    .write()
                                    .await
                                    .remove(&notification.session_id);
                                if let Err(e) = upsert_thought_plan_message(
                                    &slack_clone,
                                    &plan_messages_clone,
                                    &notification.session_id,
                                    &session.channel,
                                    &thread_key,
                                    &tasks,
                                    false,
                                )
                                .await
                                {
                                    tracing::error!(
                                        "Failed to post derived tool call plan block: {}",
                                        e
                                    );
                                }
                            }
                            continue;
                        }

                        // Check if there's a diff in content
                        let has_diff = tool_call.content.iter().any(|item| {
                            matches!(item, agent_client_protocol::ToolCallContent::Diff(_))
                        });

                        // Prepare YAML input if needed
                        let input_yaml = if !has_diff {
                            tool_call
                                .raw_input
                                .as_ref()
                                .and_then(|v| serde_yaml_ng::to_string(v).ok())
                        } else {
                            None
                        };

                        let msg = format!("🔧 Tool: {}", tool_call.title);

                        let tool_call_id = tool_call.tool_call_id.to_string();
                        let mut tool_messages = tool_messages_clone.write().await;

                        // Send or update message first to get timestamp
                        let msg_ts = if let Some((channel, ts, _)) =
                            tool_messages.get(&tool_call_id).cloned()
                        {
                            // Update existing message
                            let _ = slack_clone.update_message(&channel, &ts, &msg).await;
                            tool_messages.insert(
                                tool_call_id.clone(),
                                (channel, ts.clone(), tool_call.clone()),
                            );
                            ts
                        } else {
                            // Send new message
                            drop(tool_messages);
                            match slack_clone
                                .send_message(&session.channel, Some(&thread_key), &msg)
                                .await
                            {
                                Ok(ts) => {
                                    tool_messages_clone.write().await.insert(
                                        tool_call_id.clone(),
                                        (session.channel.clone(), ts.clone(), tool_call.clone()),
                                    );
                                    ts
                                }
                                Err(e) => {
                                    tracing::error!("Failed to send message: {}", e);
                                    continue;
                                }
                            }
                        };

                        // Now upload files to the message
                        if let Some(yaml_content) = input_yaml {
                            if let Err(e) = slack_clone
                                .upload_file(
                                    &session.channel,
                                    Some(&msg_ts),
                                    &yaml_content,
                                    "input.yaml",
                                    Some("Input"),
                                )
                                .await
                            {
                                tracing::error!("Failed to upload YAML file: {}", e);
                            }
                        }

                        // Upload diff files
                        for item in &tool_call.content {
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
                                    if let Err(e) = slack_clone
                                        .upload_file(
                                            &session.channel,
                                            Some(&msg_ts),
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
                                    if let agent_client_protocol::ContentBlock::Text(text) =
                                        &content.content
                                    {
                                        if let Err(e) = slack_clone
                                            .upload_file(
                                                &session.channel,
                                                Some(&msg_ts),
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
                    agent_client_protocol::SessionUpdate::ToolCallUpdate(update) => {
                        trace!(
                            "ToolCallUpdate: id={}, status={:?}, content={:?}",
                            update.tool_call_id, update.fields.status, update.fields.content
                        );

                        let has_native_plan = real_plan_sessions_clone
                            .read()
                            .await
                            .contains(&notification.session_id);
                        if !has_native_plan {
                            let tasks = {
                                let mut buffers = thought_plan_buffers_clone.write().await;
                                let entries = buffers
                                    .entry(notification.session_id.clone())
                                    .or_insert_with(Vec::new);
                                apply_tool_call_update_to_tasks(entries, &update);
                                entries.clone()
                            };

                            if !tasks.is_empty() {
                                if let Err(e) = upsert_thought_plan_message(
                                    &slack_clone,
                                    &plan_messages_clone,
                                    &notification.session_id,
                                    &session.channel,
                                    &thread_key,
                                    &tasks,
                                    false,
                                )
                                .await
                                {
                                    tracing::error!(
                                        "Failed to post derived tool call update plan block: {}",
                                        e
                                    );
                                }
                            }
                            continue;
                        }

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
                                    let status_emoji = match status {
                                        agent_client_protocol::ToolCallStatus::Completed => "✅",
                                        agent_client_protocol::ToolCallStatus::Failed => "❌",
                                        _ => unreachable!(),
                                    };

                                    let msg = format!("{} Tool: {}", status_emoji, tool_call.title);

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
                .list_sessions()
                .await
                .into_iter()
                .find(|(_, session)| session.session_id == permission_req.session_id);

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
            pending_permissions.clone(),
            plan_buffers.clone(),
            plan_messages.clone(),
            real_plan_sessions.clone(),
            thought_plan_buffers.clone(),
            thought_plan_completed.clone(),
        )
        .await;
    }

    Ok(())
}

fn generate_unified_diff(old_text: &str, new_text: &str) -> String {
    use similar::TextDiff;

    TextDiff::from_lines(old_text, new_text)
        .iter_all_changes()
        .map(|change| format!("{}{}", change.tag(), change.value()))
        .collect()
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().take(max_chars) {
        out.push(ch);
    }
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
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
        "Thinking completed".to_string()
    } else {
        "Thinking".to_string()
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

async fn upsert_plan_message(
    slack: &Arc<slack::SlackConnection>,
    plan_messages: &PlanMessages,
    session_id: &SessionId,
    channel: &str,
    thread_key: &str,
    entries: &[agent_client_protocol::PlanEntry],
) -> Result<()> {
    let fallback_text = format_plan_message(entries);
    let plan_block = build_plan_block_payload(entries);
    let existing = plan_messages.read().await.get(session_id).cloned();

    if let Some((msg_channel, msg_ts)) = existing {
        if let Err(e) = slack
            .update_message_with_blocks(
                &msg_channel,
                &msg_ts,
                &fallback_text,
                vec![plan_block.clone()],
            )
            .await
        {
            warn!(
                "Failed to update plan block message, falling back to text update: {}",
                e
            );
            slack
                .update_message(&msg_channel, &msg_ts, &fallback_text)
                .await?;
        }
    } else {
        let ts = match slack
            .send_message_with_blocks(channel, Some(thread_key), &fallback_text, vec![plan_block])
            .await
        {
            Ok(ts) => ts,
            Err(e) => {
                warn!(
                    "Failed to send plan block message, falling back to text message: {}",
                    e
                );
                slack
                    .send_message(channel, Some(thread_key), &fallback_text)
                    .await?
            }
        };
        plan_messages
            .write()
            .await
            .insert(session_id.clone(), (channel.to_string(), ts));
    }

    Ok(())
}

fn extract_thought_task(text: &str) -> Option<(String, Option<String>)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(start) = trimmed.find("**") {
        let rest = &trimmed[start + 2..];
        if let Some(end) = rest.find("**") {
            let title = rest[..end].trim();
            if !title.is_empty() {
                let detail_text = rest[end + 2..].trim();
                let details = if detail_text.is_empty() {
                    None
                } else {
                    Some(truncate_text(detail_text, 900))
                };
                return Some((truncate_text(title, 120), details));
            }
        }
    }

    let mut non_empty = trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = non_empty.next()?;
    let title = first
        .trim_start_matches('#')
        .trim()
        .trim_matches('*')
        .trim();
    let remainder = non_empty.collect::<Vec<_>>().join("\n");
    let details = if remainder.trim().is_empty() {
        None
    } else {
        Some(truncate_text(remainder.trim(), 900))
    };

    Some((truncate_text(title, 120), details))
}

fn finalize_in_progress_thought_tasks(tasks: &mut [DerivedPlanTask]) {
    for task in tasks.iter_mut() {
        if task.kind == DerivedPlanTaskKind::Thought && task.status == "in_progress" {
            task.status = "complete".to_string();
        }
    }
}

fn upsert_thought_task(tasks: &mut Vec<DerivedPlanTask>, title: String, details: Option<String>) {
    if let Some(last) = tasks.last_mut() {
        if last.kind == DerivedPlanTaskKind::Thought
            && last.status == "in_progress"
            && last.title == title
        {
            if details.is_some() {
                last.details = details;
            }
            return;
        }
    }

    finalize_in_progress_thought_tasks(tasks);

    let thought_index = tasks
        .iter()
        .filter(|task| task.kind == DerivedPlanTaskKind::Thought)
        .count()
        + 1;
    tasks.push(DerivedPlanTask {
        task_id: format!("thought_{thought_index:03}"),
        title,
        status: "in_progress".to_string(),
        details,
        output: None,
        kind: DerivedPlanTaskKind::Thought,
    });
}

fn derive_tool_task_title(tool_call: &agent_client_protocol::ToolCall) -> String {
    let raw_id = tool_call.tool_call_id.to_string();
    let base = raw_id.split('-').next().unwrap_or_default();
    if !base.is_empty() {
        let label = base.replace('_', " ");
        return format!("Tool: {}", truncate_text(label.trim(), 90));
    }
    format!("Tool: {}", truncate_text(tool_call.title.trim(), 90))
}

fn map_tool_call_status_to_plan_status(status: agent_client_protocol::ToolCallStatus) -> String {
    match status {
        agent_client_protocol::ToolCallStatus::Pending => "pending".to_string(),
        agent_client_protocol::ToolCallStatus::InProgress => "in_progress".to_string(),
        agent_client_protocol::ToolCallStatus::Completed => "complete".to_string(),
        agent_client_protocol::ToolCallStatus::Failed => "error".to_string(),
        _ => "in_progress".to_string(),
    }
}

fn extract_tool_call_content_summary(
    content: &[agent_client_protocol::ToolCallContent],
) -> Option<String> {
    let mut lines = Vec::new();
    for item in content {
        match item {
            agent_client_protocol::ToolCallContent::Content(c) => {
                if let agent_client_protocol::ContentBlock::Text(t) = &c.content {
                    if !t.text.trim().is_empty() {
                        lines.push(truncate_text(t.text.trim(), 240));
                    }
                }
            }
            agent_client_protocol::ToolCallContent::Diff(diff) => {
                lines.push(format!("Updated {}", diff.path.display()));
            }
            agent_client_protocol::ToolCallContent::Terminal(t) => {
                lines.push(format!("Terminal {}", t.terminal_id));
            }
            _ => {}
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(truncate_text(&lines.join("\n"), 900))
    }
}

fn upsert_tool_task_from_tool_call(
    tasks: &mut Vec<DerivedPlanTask>,
    tool_call: &agent_client_protocol::ToolCall,
) {
    let task_id = format!("tool_{}", tool_call.tool_call_id);
    let status = map_tool_call_status_to_plan_status(tool_call.status);
    let details = Some(truncate_text(tool_call.title.trim(), 900));
    let output = extract_tool_call_content_summary(&tool_call.content);

    if let Some(task) = tasks.iter_mut().find(|task| task.task_id == task_id) {
        task.title = derive_tool_task_title(tool_call);
        task.status = status;
        task.details = details;
        if output.is_some() {
            task.output = output;
        }
        return;
    }

    tasks.push(DerivedPlanTask {
        task_id,
        title: derive_tool_task_title(tool_call),
        status,
        details,
        output,
        kind: DerivedPlanTaskKind::Tool,
    });
}

fn apply_tool_call_update_to_tasks(
    tasks: &mut Vec<DerivedPlanTask>,
    update: &agent_client_protocol::ToolCallUpdate,
) {
    let task_id = format!("tool_{}", update.tool_call_id);
    if let Some(task) = tasks.iter_mut().find(|task| task.task_id == task_id) {
        if let Some(status) = update.fields.status {
            task.status = map_tool_call_status_to_plan_status(status);
        }
        if let Some(title) = &update.fields.title {
            task.details = Some(truncate_text(title.trim(), 900));
        }
        if let Some(content) = &update.fields.content {
            if let Some(summary) = extract_tool_call_content_summary(content) {
                task.output = Some(summary);
            }
        }
    }
}

fn rich_text_text_element(text: &str, bold: bool, italic: bool, code: bool) -> Value {
    if !bold && !italic && !code {
        return json!({"type": "text", "text": text});
    }

    let mut style = serde_json::Map::new();
    if bold {
        style.insert("bold".to_string(), Value::Bool(true));
    }
    if italic {
        style.insert("italic".to_string(), Value::Bool(true));
    }
    if code {
        style.insert("code".to_string(), Value::Bool(true));
    }

    let mut obj = serde_json::Map::new();
    obj.insert("type".to_string(), Value::String("text".to_string()));
    obj.insert("text".to_string(), Value::String(text.to_string()));
    obj.insert("style".to_string(), Value::Object(style));
    Value::Object(obj)
}

fn rich_text_elements_from_markdown(text: &str) -> Vec<Value> {
    fn starts_with_at(text: &str, i: usize, pat: &str) -> bool {
        text.get(i..)
            .map(|rest| rest.starts_with(pat))
            .unwrap_or(false)
    }

    let mut elements = Vec::new();
    let mut plain = String::new();
    let mut i = 0;

    let flush_plain = |elements: &mut Vec<Value>, plain: &mut String| {
        if !plain.is_empty() {
            elements.push(rich_text_text_element(plain, false, false, false));
            plain.clear();
        }
    };

    while i < text.len() {
        if starts_with_at(text, i, "`") {
            if let Some(rest) = text.get(i + 1..) {
                if let Some(end_rel) = rest.find('`') {
                    flush_plain(&mut elements, &mut plain);
                    let inner = &rest[..end_rel];
                    if !inner.is_empty() {
                        elements.push(rich_text_text_element(inner, false, false, true));
                    }
                    i += 1 + end_rel + 1;
                    continue;
                }
            }
        }

        if starts_with_at(text, i, "**") {
            if let Some(rest) = text.get(i + 2..) {
                if let Some(end_rel) = rest.find("**") {
                    flush_plain(&mut elements, &mut plain);
                    let inner = &rest[..end_rel];
                    if !inner.is_empty() {
                        elements.push(rich_text_text_element(inner, true, false, false));
                    }
                    i += 2 + end_rel + 2;
                    continue;
                }
            }
        }

        if starts_with_at(text, i, "__") {
            if let Some(rest) = text.get(i + 2..) {
                if let Some(end_rel) = rest.find("__") {
                    flush_plain(&mut elements, &mut plain);
                    let inner = &rest[..end_rel];
                    if !inner.is_empty() {
                        elements.push(rich_text_text_element(inner, true, false, false));
                    }
                    i += 2 + end_rel + 2;
                    continue;
                }
            }
        }

        if starts_with_at(text, i, "*") {
            if let Some(rest) = text.get(i + 1..) {
                if let Some(first) = rest.chars().next() {
                    if !first.is_whitespace() {
                        if let Some(end_rel) = rest.find('*') {
                            let inner = &rest[..end_rel];
                            if !inner.trim().is_empty() {
                                flush_plain(&mut elements, &mut plain);
                                elements.push(rich_text_text_element(inner, false, true, false));
                                i += 1 + end_rel + 1;
                                continue;
                            }
                        }
                    }
                }
            }
        }

        if starts_with_at(text, i, "_") {
            if let Some(rest) = text.get(i + 1..) {
                if let Some(first) = rest.chars().next() {
                    if !first.is_whitespace() {
                        if let Some(end_rel) = rest.find('_') {
                            let inner = &rest[..end_rel];
                            if !inner.trim().is_empty() {
                                flush_plain(&mut elements, &mut plain);
                                elements.push(rich_text_text_element(inner, false, true, false));
                                i += 1 + end_rel + 1;
                                continue;
                            }
                        }
                    }
                }
            }
        }

        if let Some(ch) = text.get(i..).and_then(|rest| rest.chars().next()) {
            plain.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    flush_plain(&mut elements, &mut plain);

    if elements.is_empty() {
        vec![rich_text_text_element(text, false, false, false)]
    } else {
        elements
    }
}

fn rich_text_value(text: &str) -> Value {
    let normalized = truncate_text(text, 1800);
    json!({
        "type": "rich_text",
        "elements": [{
            "type": "rich_text_section",
            "elements": rich_text_elements_from_markdown(&normalized)
        }]
    })
}

fn split_text_into_sentences(text: &str, max_items: usize) -> Vec<String> {
    fn split_line(line: &str) -> Vec<String> {
        let mut sentences = Vec::new();
        let mut current = String::new();
        let mut chars = line.chars().peekable();

        while let Some(ch) = chars.next() {
            current.push(ch);
            if matches!(ch, '.' | '!' | '?') {
                while let Some(next) = chars.peek() {
                    if next.is_whitespace() {
                        current.push(*next);
                        chars.next();
                    } else {
                        break;
                    }
                }

                let sentence = current.trim();
                if !sentence.is_empty() {
                    sentences.push(sentence.to_string());
                }
                current.clear();
            }
        }

        let tail = current.trim();
        if !tail.is_empty() {
            sentences.push(tail.to_string());
        }

        sentences
    }

    let mut out = Vec::new();
    for raw_line in text.replace('\r', "").lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let line = line
            .trim_start_matches(|c: char| matches!(c, '-' | '*'))
            .trim();

        for sentence in split_line(line) {
            if out.len() >= max_items {
                return out;
            }
            if !sentence.is_empty() {
                out.push(sentence);
            }
        }
    }

    out
}

fn rich_text_bullets_value(text: &str) -> Value {
    let bullets = split_text_into_sentences(text, 12)
        .into_iter()
        .map(|sentence| truncate_text(&sentence, 240))
        .filter(|sentence| !sentence.is_empty())
        .collect::<Vec<_>>();

    if bullets.len() <= 1 {
        return rich_text_value(text);
    }

    let elements = bullets
        .into_iter()
        .map(|sentence| {
            json!({
                "type": "rich_text_section",
                "elements": rich_text_elements_from_markdown(&sentence)
            })
        })
        .collect::<Vec<_>>();

    json!({
        "type": "rich_text",
        "elements": [{
            "type": "rich_text_list",
            "style": "bullet",
            "elements": elements
        }]
    })
}

fn build_thought_plan_block_payload(tasks: &[DerivedPlanTask], completed: bool) -> Value {
    let title = if completed {
        "Thinking completed"
    } else {
        "Thinking"
    };

    let task_values = tasks
        .iter()
        .map(|task| {
            let mut value = json!({
                "task_id": task.task_id,
                "title": task.title,
                "status": task.status
            });

            if let Some(details) = &task.details {
                if let Some(obj) = value.as_object_mut() {
                    let details_value = if task.kind == DerivedPlanTaskKind::Thought {
                        rich_text_bullets_value(details)
                    } else {
                        rich_text_value(details)
                    };
                    obj.insert("details".to_string(), details_value);
                }
            }

            if let Some(output) = &task.output {
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("output".to_string(), rich_text_value(output));
                }
            }

            value
        })
        .collect::<Vec<_>>();

    json!({
        "type": "plan",
        "title": title,
        "tasks": task_values
    })
}

fn format_thought_plan_message(tasks: &[DerivedPlanTask], completed: bool) -> String {
    let mut lines = Vec::with_capacity(tasks.len() + 1);
    if completed {
        lines.push("*Plan (derived-complete)*".to_string());
    } else {
        lines.push("*Plan (derived)*".to_string());
    }

    for task in tasks {
        let marker = match task.status.as_str() {
            "complete" => "[x]",
            "in_progress" => "[>]",
            "error" => "[!]",
            _ => "[ ]",
        };
        lines.push(format!("{} {}", marker, task.title));
        if let Some(details) = &task.details {
            if task.kind == DerivedPlanTaskKind::Thought {
                for sentence in split_text_into_sentences(details, 6) {
                    lines.push(format!("   - {}", truncate_text(&sentence, 140)));
                }
            } else {
                lines.push(format!("   {}", truncate_text(details, 140)));
            }
        }
    }

    lines.join("\n")
}

async fn upsert_thought_plan_message(
    slack: &Arc<slack::SlackConnection>,
    plan_messages: &PlanMessages,
    session_id: &SessionId,
    channel: &str,
    thread_key: &str,
    tasks: &[DerivedPlanTask],
    completed: bool,
) -> Result<()> {
    let fallback_text = format_thought_plan_message(tasks, completed);
    let plan_block = build_thought_plan_block_payload(tasks, completed);
    let existing = plan_messages.read().await.get(session_id).cloned();

    if let Some((msg_channel, msg_ts)) = existing {
        if let Err(e) = slack
            .update_message_with_blocks(
                &msg_channel,
                &msg_ts,
                &fallback_text,
                vec![plan_block.clone()],
            )
            .await
        {
            warn!(
                "Failed to update derived plan block message, falling back to text update: {}",
                e
            );
            slack
                .update_message(&msg_channel, &msg_ts, &fallback_text)
                .await?;
        }
    } else {
        let ts = match slack
            .send_message_with_blocks(channel, Some(thread_key), &fallback_text, vec![plan_block])
            .await
        {
            Ok(ts) => ts,
            Err(e) => {
                warn!(
                    "Failed to send derived plan block message, falling back to text message: {}",
                    e
                );
                slack
                    .send_message(channel, Some(thread_key), &fallback_text)
                    .await?
            }
        };
        plan_messages
            .write()
            .await
            .insert(session_id.clone(), (channel.to_string(), ts));
    }

    Ok(())
}
