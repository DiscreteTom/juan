/// Slack client module for Socket Mode connection and message handling.
///
/// This module provides:
/// - SlackConnection: Client for sending/updating messages
/// - SlackEvent: Simplified event types for the application
/// - Socket Mode listener for receiving events from Slack
use anyhow::{Context, Result};
use slack_morphism::prelude::*;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info};

/// Simplified Slack event types used internally by the application.
/// Converts from slack_morphism's complex event types to our domain model.
#[derive(Debug, Clone)]
pub enum SlackEvent {
    /// Regular message in a channel or thread
    Message {
        channel: String,
        ts: String,
        thread_ts: Option<String>,
        text: String,
        user: String,
    },
    /// Message that mentions the bot (e.g., @botname)
    AppMention {
        channel: String,
        ts: String,
        thread_ts: Option<String>,
        text: String,
        user: String,
    },
}

/// Slack client wrapper for Socket Mode connection and API calls.
/// Handles both receiving events (via Socket Mode) and sending messages (via Web API).
pub struct SlackConnection {
    client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    bot_token: SlackApiToken,
}

impl SlackConnection {
    /// Creates a new Slack connection with the given bot token.
    /// Does not establish connection yet - call connect() to start listening.
    pub fn new(bot_token: String) -> Self {
        let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new().unwrap()));

        Self {
            client,
            bot_token: SlackApiToken::new(bot_token.into()),
        }
    }

    /// Establishes Socket Mode connection and starts listening for events.
    /// Events are sent through the provided channel.
    /// This is a long-running task that should be spawned in a separate tokio task.
    pub async fn connect(
        self: Arc<Self>,
        app_token: String,
        event_tx: mpsc::UnboundedSender<SlackEvent>,
    ) -> Result<()> {
        info!("Connecting to Slack Socket Mode");

        // Register callback for push events (messages, mentions, etc.)
        let callbacks = SlackSocketModeListenerCallbacks::new().with_push_events(handle_push_event);

        // Create listener environment with event sender in user state
        let listener_env = Arc::new(
            SlackClientEventsListenerEnvironment::new(self.client.clone())
                .with_user_state(event_tx),
        );

        let listener = SlackClientSocketModeListener::new(
            &SlackClientSocketModeConfig::new(),
            listener_env,
            callbacks,
        );

        let app_token = SlackApiToken::new(app_token.into());
        listener.listen_for(&app_token).await?;
        listener.serve().await;

        Ok(())
    }

    /// Sends a message to a Slack channel or thread.
    /// Returns the timestamp (ts) of the sent message, which can be used to update it later.
    pub async fn send_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<String> {
        let session = self.client.open_session(&self.bot_token);

        let mut req = SlackApiChatPostMessageRequest::new(
            channel.into(),
            SlackMessageContent::new().with_text(text.into()),
        );

        // If thread_ts is provided, send as a reply in that thread
        if let Some(ts) = thread_ts {
            req = req.with_thread_ts(ts.into());
        }

        let resp = session
            .chat_post_message(&req)
            .await
            .context("Failed to send Slack message")?;

        Ok(resp.ts.to_string())
    }

    /// Updates an existing message with new text.
    /// Requires the channel and timestamp (ts) of the message to update.
    pub async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<()> {
        let session = self.client.open_session(&self.bot_token);

        let req = SlackApiChatUpdateRequest::new(
            channel.into(),
            SlackMessageContent::new().with_text(text.into()),
            ts.into(),
        );

        session
            .chat_update(&req)
            .await
            .context("Failed to update Slack message")?;

        Ok(())
    }
}

/// Callback handler for Slack push events (messages, mentions, etc.).
/// Converts slack_morphism events to our simplified SlackEvent type and sends to the channel.
async fn handle_push_event(
    event: SlackPushEventCallback,
    _client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    state: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Extract the event sender channel from user state
    let tx = state
        .read()
        .await
        .get_user_state::<mpsc::UnboundedSender<SlackEvent>>()
        .cloned()
        .ok_or("No event sender in state")?;

    match event.event {
        SlackEventCallbackBody::Message(msg) => {
            // Ignore messages from bots to prevent loops
            if msg.sender.bot_id.is_some() {
                return Ok(());
            }

            if let Some(content) = msg.content {
                if let Some(text) = content.text {
                    let user = msg.sender.user.map(|u| u.to_string()).unwrap_or_default();
                    let _ = tx.send(SlackEvent::Message {
                        channel: msg
                            .origin
                            .channel
                            .map(|c| c.to_string())
                            .unwrap_or_default(),
                        ts: msg.origin.ts.to_string(),
                        thread_ts: msg.origin.thread_ts.map(|ts| ts.to_string()),
                        text,
                        user,
                    });
                }
            }
        }
        SlackEventCallbackBody::AppMention(mention) => {
            let user = mention.user.to_string();
            let text = mention.content.text.unwrap_or_default();

            let _ = tx.send(SlackEvent::AppMention {
                channel: mention.channel.to_string(),
                ts: mention.origin.ts.to_string(),
                thread_ts: mention.origin.thread_ts.map(|ts| ts.to_string()),
                text,
                user,
            });
        }
        _ => debug!("Unhandled callback event"),
    }

    Ok(())
}
