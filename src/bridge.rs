use agent_client_protocol::SessionId;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
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

/// Shared map for tracking per-session plans (used for incremental plan updates)
pub type PlanBuffers = Arc<RwLock<HashMap<SessionId, Vec<agent_client_protocol::PlanEntry>>>>;
/// Shared map for tracking per-session plan message timestamps
pub type PlanMessages = Arc<RwLock<HashMap<SessionId, (String, String)>>>; // session_id -> (channel, ts)
/// Sessions that produced real ACP Plan updates.
pub type RealPlanSessions = Arc<RwLock<HashSet<SessionId>>>;
/// Fallback thought-derived plan steps for sessions with no ACP Plan updates.
pub type ThoughtPlanBuffers = Arc<RwLock<HashMap<SessionId, Vec<DerivedPlanTask>>>>;
/// Sessions whose thought-derived plan was already marked completed.
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

#[derive(Clone)]
struct AcpDebugLogger {
    file: Arc<Mutex<std::fs::File>>,
}

impl AcpDebugLogger {
    fn from_env() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os("JUAN_ACP_LOG_PATH") else {
            return Ok(None);
        };

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "Failed to open JUAN_ACP_LOG_PATH file: {}",
                    std::path::PathBuf::from(path).display()
                )
            })?;

        Ok(Some(Self {
            file: Arc::new(Mutex::new(file)),
        }))
    }

    fn log_notification(
        &self,
        agent_name: &str,
        session_id: &SessionId,
        channel: Option<&str>,
        thread_key: Option<&str>,
        update: &agent_client_protocol::SessionUpdate,
    ) {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let record = json!({
            "timestamp_ms": ts_ms,
            "event": "session_notification",
            "agent_name": agent_name,
            "session_id": session_id.to_string(),
            "channel": channel,
            "thread_key": thread_key,
            "update_kind": session_update_kind(update),
            "summary": summarize_session_update(update),
            "raw_debug": format!("{update:?}")
        });

        self.write_json_line(&record);
    }

    fn write_json_line(&self, value: &Value) {
        let line = match serde_json::to_string(value) {
            Ok(line) => line,
            Err(e) => {
                warn!("Failed to serialize ACP log line: {}", e);
                return;
            }
        };

        let mut file = match self.file.lock() {
            Ok(file) => file,
            Err(e) => {
                warn!("ACP log file mutex poisoned: {}", e);
                return;
            }
        };

        if let Err(e) = writeln!(file, "{}", line) {
            warn!("Failed to write ACP log line: {}", e);
        }
    }
}

pub async fn run_bridge(config: Arc<config::Config>) -> Result<()> {
    info!("Slack bot configured");
    info!("Default workspace: {}", config.bridge.default_workspace);
    info!("Auto-approve: {}", config.bridge.auto_approve);
    info!("Plan display mode: {}", config.bridge.plan_display_mode);
    info!("Configured agents: {}", config.agents.len());

    for agent in &config.agents {
        info!(
            "  - {} ({}): {}",
            agent.name, agent.command, agent.description
        );
    }

    let acp_logger = AcpDebugLogger::from_env()?;
    if acp_logger.is_some() {
        info!("ACP debug logging enabled (JUAN_ACP_LOG_PATH)");
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

    // Create shared map for tracking per-session plans
    let plan_buffers: PlanBuffers = Arc::new(RwLock::new(HashMap::new()));
    // Create shared map for tracking per-session plan message timestamps
    let plan_messages: PlanMessages = Arc::new(RwLock::new(HashMap::new()));
    // Track whether a session emits native ACP Plan updates.
    let real_plan_sessions: RealPlanSessions = Arc::new(RwLock::new(HashSet::new()));
    // Track thought-derived plan steps (fallback when ACP Plan is absent).
    let thought_plan_buffers: ThoughtPlanBuffers = Arc::new(RwLock::new(HashMap::new()));
    let thought_plan_completed: ThoughtPlanCompleted = Arc::new(RwLock::new(HashSet::new()));

    // Spawn task to handle agent notifications and forward to Slack
    let slack_clone = slack.clone();
    let session_manager_clone = session_manager.clone();
    let buffers_clone = message_buffers.clone();
    let tool_messages_clone = tool_call_messages.clone();
    let plan_buffers_clone = plan_buffers.clone();
    let plan_messages_clone = plan_messages.clone();
    let plan_display_mode = config.bridge.plan_display_mode.clone();
    let real_plan_sessions_clone = real_plan_sessions.clone();
    let thought_plan_buffers_clone = thought_plan_buffers.clone();
    let thought_plan_completed_clone = thought_plan_completed.clone();
    let acp_logger_clone = acp_logger.clone();
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

                if let Some(logger) = &acp_logger_clone {
                    logger.log_notification(
                        &agent_name,
                        &notification.session_id,
                        Some(&session.channel),
                        Some(&thread_key),
                        &notification.update,
                    );
                }

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
                    agent_client_protocol::SessionUpdate::AgentThoughtChunk(chunk) => {
                        if let agent_client_protocol::ContentBlock::Text(text) = chunk.content {
                            trace!("Thought chunk (len={})", text.text.len());

                            let has_native_plan = real_plan_sessions_clone
                                .read()
                                .await
                                .contains(&notification.session_id);
                            if !has_native_plan {
                                if let Some((title, details)) =
                                    extract_thought_task(&text.text)
                                {
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

                        let (entries_to_display, display_mode_label) = {
                            let mut plans = plan_buffers_clone.write().await;
                            let session_plan =
                                plans.entry(notification.session_id.clone()).or_default();
                            let previous_plan = session_plan.clone();
                            apply_plan_update(session_plan, &plan);

                            if plan_display_mode == "incremental" {
                                (
                                    collect_incremental_plan_entries(&previous_plan, session_plan),
                                    "incremental",
                                )
                            } else {
                                (session_plan.clone(), "full")
                            }
                        };

                        if !entries_to_display.is_empty() {
                            if let Err(e) = upsert_plan_message(
                                &slack_clone,
                                &plan_messages_clone,
                                &notification.session_id,
                                &session.channel,
                                &thread_key,
                                display_mode_label,
                                &entries_to_display,
                            )
                            .await
                            {
                                tracing::error!("Failed to post ACP plan block: {}", e);
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
                                    "yaml",
                                    Some("Input"),
                                )
                                .await
                            {
                                tracing::error!("Failed to upload YAML file: {}", e);
                            }
                        }

                        // Upload diff files
                        for item in &tool_call.content {
                            if let agent_client_protocol::ToolCallContent::Diff(diff) = item {
                                let diff_text = if let Some(old_text) = &diff.old_text {
                                    format!(
                                        "--- {}\n+++ {}\n{}",
                                        diff.path.display(),
                                        diff.path.display(),
                                        generate_unified_diff(old_text, &diff.new_text)
                                    )
                                } else {
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
                                        "diff",
                                        Some("Diff"),
                                    )
                                    .await
                                {
                                    tracing::error!("Failed to upload diff file: {}", e);
                                }
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
            } else if let Some(logger) = &acp_logger_clone {
                logger.log_notification(
                    &agent_name,
                    &notification.session_id,
                    None,
                    None,
                    &notification.update,
                );
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

fn apply_plan_update(
    existing: &mut Vec<agent_client_protocol::PlanEntry>,
    plan: &agent_client_protocol::Plan,
) {
    *existing = plan.entries.clone();
}

fn session_update_kind(update: &agent_client_protocol::SessionUpdate) -> &'static str {
    match update {
        agent_client_protocol::SessionUpdate::AgentMessageChunk(_) => "AgentMessageChunk",
        agent_client_protocol::SessionUpdate::AgentThoughtChunk(_) => "AgentThoughtChunk",
        agent_client_protocol::SessionUpdate::Plan(_) => "Plan",
        agent_client_protocol::SessionUpdate::ToolCall(_) => "ToolCall",
        agent_client_protocol::SessionUpdate::ToolCallUpdate(_) => "ToolCallUpdate",
        _ => "Other",
    }
}

fn summarize_session_update(update: &agent_client_protocol::SessionUpdate) -> Value {
    match update {
        agent_client_protocol::SessionUpdate::AgentMessageChunk(chunk) => {
            let text = match &chunk.content {
                agent_client_protocol::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            };
            json!({
                "content_kind": content_block_kind(&chunk.content),
                "text_len": text.as_ref().map(|s| s.len()).unwrap_or(0),
                "text_preview": text
            })
        }
        agent_client_protocol::SessionUpdate::AgentThoughtChunk(chunk) => {
            let text = match &chunk.content {
                agent_client_protocol::ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            };
            json!({
                "content_kind": content_block_kind(&chunk.content),
                "text_len": text.as_ref().map(|s| s.len()).unwrap_or(0),
                "text_preview": text
            })
        }
        agent_client_protocol::SessionUpdate::Plan(plan) => {
            let entries = plan
                .entries
                .iter()
                .enumerate()
                .map(|(idx, entry)| {
                    json!({
                        "index": idx,
                        "status": format!("{:?}", entry.status),
                        "content": entry.content
                    })
                })
                .collect::<Vec<_>>();

            json!({
                "entries": entries,
                "plan_block_preview": build_plan_block_payload("full", &plan.entries)
            })
        }
        agent_client_protocol::SessionUpdate::ToolCall(tool_call) => {
            let has_diff = tool_call.content.iter().any(|item| {
                matches!(item, agent_client_protocol::ToolCallContent::Diff(_))
            });
            json!({
                "tool_call_id": tool_call.tool_call_id.to_string(),
                "title": tool_call.title,
                "kind": format!("{:?}", tool_call.kind),
                "has_raw_input": tool_call.raw_input.is_some(),
                "has_diff": has_diff,
                "content_count": tool_call.content.len()
            })
        }
        agent_client_protocol::SessionUpdate::ToolCallUpdate(update) => {
            json!({
                "tool_call_id": update.tool_call_id.to_string(),
                "status": update.fields.status.as_ref().map(|s| format!("{:?}", s)),
                "has_content": update.fields.content.is_some(),
                "has_raw_input": update.fields.raw_input.is_some(),
                "has_raw_output": update.fields.raw_output.is_some()
            })
        }
        _ => json!({}),
    }
}

fn content_block_kind(content: &agent_client_protocol::ContentBlock) -> &'static str {
    match content {
        agent_client_protocol::ContentBlock::Text(_) => "Text",
        agent_client_protocol::ContentBlock::Image(_) => "Image",
        _ => "Other",
    }
}

fn build_plan_block_payload(mode: &str, entries: &[agent_client_protocol::PlanEntry]) -> Value {
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
        format!("Thinking ({mode})")
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

fn collect_incremental_plan_entries(
    previous: &[agent_client_protocol::PlanEntry],
    current: &[agent_client_protocol::PlanEntry],
) -> Vec<agent_client_protocol::PlanEntry> {
    let mut changed = Vec::new();

    for (index, entry) in current.iter().enumerate() {
        if previous.get(index) != Some(entry) {
            changed.push(entry.clone());
        }
    }

    changed
}

fn format_plan_message(mode: &str, entries: &[agent_client_protocol::PlanEntry]) -> String {
    let mut lines = Vec::with_capacity(entries.len() + 1);
    lines.push(format!("*Plan ({})*", mode));

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
    mode: &str,
    entries: &[agent_client_protocol::PlanEntry],
) -> Result<()> {
    let fallback_text = format_plan_message(mode, entries);
    let plan_block = build_plan_block_payload(mode, entries);
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
            slack.update_message(&msg_channel, &msg_ts, &fallback_text)
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
                slack.send_message(channel, Some(thread_key), &fallback_text)
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

    // Prefer markdown emphasis headings like "**Interpreting the Greeting**".
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

    let mut non_empty = trimmed.lines().map(str::trim).filter(|line| !line.is_empty());
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
            if details.as_deref().is_some() {
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

fn extract_tool_call_content_summary(content: &[agent_client_protocol::ToolCallContent]) -> Option<String> {
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
        return json!({
            "type": "text",
            "text": text
        });
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
            let rest = match text.get(i + 1..) {
                Some(r) => r,
                None => "",
            };
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

        if starts_with_at(text, i, "**") {
            let rest = match text.get(i + 2..) {
                Some(r) => r,
                None => "",
            };
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

        if starts_with_at(text, i, "__") {
            let rest = match text.get(i + 2..) {
                Some(r) => r,
                None => "",
            };
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

        if starts_with_at(text, i, "*") {
            let rest = match text.get(i + 1..) {
                Some(r) => r,
                None => "",
            };
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

        if starts_with_at(text, i, "_") {
            let rest = match text.get(i + 1..) {
                Some(r) => r,
                None => "",
            };
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
        "elements": [
            {
                "type": "rich_text_section",
                "elements": rich_text_elements_from_markdown(&normalized)
            }
        ]
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
        "elements": [
            {
                "type": "rich_text_list",
                "style": "bullet",
                "elements": elements
            }
        ]
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
            slack.update_message(&msg_channel, &msg_ts, &fallback_text)
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
                slack.send_message(channel, Some(thread_key), &fallback_text)
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

fn generate_unified_diff(old_text: &str, new_text: &str) -> String {
    use similar::TextDiff;

    TextDiff::from_lines(old_text, new_text)
        .unified_diff()
        .context_radius(3)
        .to_string()
}
