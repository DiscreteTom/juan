mod command;
mod message;
mod permission;
mod shell;

use crate::{
    agent::AgentManager,
    bridge::{MessageBuffers, PendingPermissions, PlanBuffers, PlanMessages, ThoughtBuffers},
    config::Config,
    handler::{
        command::handle_command, message::handle_message, permission::handle_permission_response,
        shell::handle_shell_command,
    },
    session::SessionManager,
    slack::{SlackConnection, SlackEvent},
};
use std::sync::Arc;
use tracing::debug;

/// Main entry point for handling Slack events.
/// Routes events to appropriate handlers based on message content.
pub async fn handle_event(
    event: SlackEvent,
    slack: Arc<SlackConnection>,
    config: Arc<Config>,
    agent_manager: Arc<AgentManager>,
    session_manager: Arc<SessionManager>,
    message_buffers: MessageBuffers,
    thought_buffers: ThoughtBuffers,
    pending_permissions: PendingPermissions,
    plan_buffers: PlanBuffers,
    plan_messages: PlanMessages,
    notification_tx: tokio::sync::mpsc::UnboundedSender<crate::bridge::NotificationWrapper>,
) {
    tracing::info!("Received event: {:?}", event);

    let SlackEvent {
        channel,
        ts,
        thread_ts,
        text,
    } = event;

    // Check if this is a response to a pending permission request FIRST
    let thread_key = thread_ts.as_deref().unwrap_or(&ts);
    debug!(
        "Checking for pending permission: thread_key={}, pending_count={}",
        thread_key,
        pending_permissions.read().await.len()
    );
    if let Some((options, response_tx)) = pending_permissions.write().await.remove(thread_key) {
        debug!("Found pending permission request, handling response");
        handle_permission_response(&text, options, response_tx, &slack, &channel, thread_key).await;
        return;
    }

    // Shell commands (!) - execute local commands
    if text.trim().starts_with('!') {
        handle_shell_command(
            &text,
            &channel,
            thread_ts.as_deref(),
            slack,
            config.clone(),
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
        thought_buffers,
        plan_buffers,
        plan_messages,
        notification_tx,
    )
    .await;
}
