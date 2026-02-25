mod command;
mod message;
mod permission;
mod shell;

use crate::{
    agent::AgentManager,
    bridge::{PendingPermissions, PlatformConnection, PlatformEvent},
    config::Config,
    handler::{
        command::handle_command, message::handle_message, permission::handle_permission_response,
        shell::handle_shell_command,
    },
    session::SessionManager,
};
use slack_morphism::prelude::SlackFile;
use std::sync::Arc;
use tracing::debug;

/// Main entry point for handling platform events.
/// Routes events to appropriate handlers based on message content.
pub async fn handle_event(
    event: PlatformEvent,
    connection: PlatformConnection,
    config: Arc<Config>,
    agent_manager: Arc<AgentManager>,
    session_manager: Arc<SessionManager>,
    pending_permissions: PendingPermissions,
    notification_tx: tokio::sync::mpsc::UnboundedSender<crate::bridge::NotificationWrapper>,
) {
    tracing::debug!("Received event: {:?}", event);

    let channel = event.channel().to_string();
    let ts = event.ts().to_string();
    let thread_ts = event.thread_ts().map(String::from);
    let text = event.text().to_string();

    // Extract files based on platform
    let files: Vec<SlackFile> = match &event {
        PlatformEvent::Slack(e) => e.files.clone(),
        PlatformEvent::Feishu(_) => vec![], // TODO: implement file extraction for Feishu
    };

    // Check if this is a response to a pending permission request FIRST
    let thread_key = thread_ts.as_deref().unwrap_or(&ts);
    debug!(
        "Checking for pending permission: thread_key={}, pending_count={}",
        thread_key,
        pending_permissions.read().await.len()
    );
    if let Some((options, response_tx)) = pending_permissions.write().await.remove(thread_key) {
        debug!("Found pending permission request, handling response");
        handle_permission_response(
            &text,
            options,
            response_tx,
            &connection,
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
            connection,
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
            connection.clone(),
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
        &files,
        &channel,
        thread_ts.as_deref(),
        connection,
        agent_manager,
        session_manager,
        notification_tx,
    )
    .await;
}
