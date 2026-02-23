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
use tracing::{debug, info, trace};

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
    },
    /// Message that mentions the bot (e.g., @botname)
    AppMention {
        channel: String,
        ts: String,
        thread_ts: Option<String>,
        text: String,
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
        debug!(
            "Sending message to channel={}, thread_ts={:?}, text_len={}",
            channel,
            thread_ts,
            text.len()
        );
        trace!("Message text: {}", text);
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
        debug!(
            "Updating message: channel={}, ts={}, text_len={}",
            channel,
            ts,
            text.len()
        );
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

    /// Adds a reaction emoji to a message.
    pub async fn add_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<()> {
        debug!(
            "Adding reaction: channel={}, ts={}, emoji={}",
            channel, ts, emoji
        );
        let session = self.client.open_session(&self.bot_token);

        let req = SlackApiReactionsAddRequest::new(channel.into(), emoji.into(), ts.into());

        session
            .reactions_add(&req)
            .await
            .context("Failed to add reaction")?;

        Ok(())
    }

    /// Uploads a file/snippet to Slack with syntax highlighting.
    /// The filetype determines the syntax highlighting (e.g., "diff", "yaml", "python").
    pub async fn upload_file(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        content: &str,
        filename: &str,
        _filetype: &str,
        title: Option<&str>,
    ) -> Result<()> {
        debug!(
            "Uploading file to channel={}, thread_ts={:?}, filename={}",
            channel, thread_ts, filename
        );
        let session = self.client.open_session(&self.bot_token);

        // Step 1: Get upload URL
        let get_url_req =
            SlackApiFilesGetUploadUrlExternalRequest::new(filename.into(), content.len());
        let url_resp = session
            .get_upload_url_external(&get_url_req)
            .await
            .context("Failed to get upload URL")?;

        // Step 2: Upload file to the URL
        let client = reqwest::Client::new();
        client
            .post(url_resp.upload_url.0.as_str())
            .body(content.to_string())
            .send()
            .await
            .context("Failed to upload file content")?;

        // Step 3: Complete the upload
        let mut file_complete = SlackApiFilesComplete::new(url_resp.file_id);
        if let Some(title) = title {
            file_complete = file_complete.with_title(title.into());
        }

        let mut complete_req = SlackApiFilesCompleteUploadExternalRequest::new(vec![file_complete]);

        if let Some(ts) = thread_ts {
            complete_req = complete_req
                .with_channel_id(channel.into())
                .with_thread_ts(ts.into());
        } else {
            complete_req = complete_req.with_channel_id(channel.into());
        }

        let resp = session
            .files_complete_upload_external(&complete_req)
            .await
            .context("Failed to complete file upload")?;

        debug!("File uploaded successfully: {:?}", resp);

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
    trace!("Received push event: {:?}", event.event);
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
                trace!("Ignoring bot message");
                return Ok(());
            }

            if let Some(content) = msg.content {
                if let Some(text) = content.text {
                    let text = html_escape::decode_html_entities(&text).to_string();
                    let user = msg.sender.user.map(|u| u.to_string()).unwrap_or_default();
                    debug!(
                        "Received message from user={}, channel={:?}",
                        user, msg.origin.channel
                    );
                    let _ = tx.send(SlackEvent::Message {
                        channel: msg
                            .origin
                            .channel
                            .map(|c| c.to_string())
                            .unwrap_or_default(),
                        ts: msg.origin.ts.to_string(),
                        thread_ts: msg.origin.thread_ts.map(|ts| ts.to_string()),
                        text,
                    });
                }
            }
        }
        SlackEventCallbackBody::AppMention(mention) => {
            let user = mention.user.to_string();
            let text = mention.content.text.unwrap_or_default();
            let text = html_escape::decode_html_entities(&text).to_string();
            debug!(
                "Received app mention from user={}, channel={}",
                user, mention.channel
            );

            let _ = tx.send(SlackEvent::AppMention {
                channel: mention.channel.to_string(),
                ts: mention.origin.ts.to_string(),
                thread_ts: mention.origin.thread_ts.map(|ts| ts.to_string()),
                text,
            });
        }
        _ => debug!("Unhandled callback event"),
    }

    Ok(())
}
