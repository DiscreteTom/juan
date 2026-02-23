use crate::{agent, bridge, session, slack};
use std::sync::Arc;
use tracing::{debug, trace};

/// Handles regular messages to agents (not commands or shell commands).
///
/// Flow:
/// 1. Check if thread has an active session
/// 2. Send prompt to agent via ACP
/// 3. Update Slack with response
pub async fn handle_message(
    text: &str,
    channel: &str,
    thread_ts: Option<&str>,
    slack: Arc<slack::SlackConnection>,
    agent_manager: Arc<agent::AgentManager>,
    session_manager: Arc<session::SessionManager>,
    message_buffers: bridge::MessageBuffers,
    plan_buffers: bridge::PlanBuffers,
    plan_messages: bridge::PlanMessages,
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

    // Reset plan UI state for new user message
    message_buffers.write().await.remove(&session.session_id);
    plan_buffers.write().await.remove(&session.session_id);
    plan_messages.write().await.remove(&session.session_id);

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
