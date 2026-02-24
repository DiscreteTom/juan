/// Slack client module for Socket Mode connection and message handling.
///
/// This module provides:
/// - SlackConnection: Client for sending/updating messages
/// - SlackEvent: Simplified event types for the application
/// - Socket Mode listener for receiving events from Slack
use anyhow::{Context, Result};
use serde_json::{Value, json};
use slack_morphism::prelude::*;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, trace};

/// Decode special characters from Slack messages.
/// Only decodes &amp;, &lt;, and &gt; as per Slack's documentation.
/// Also removes angle brackets around URLs that Slack adds.
/// https://docs.slack.dev/messaging/formatting-message-text/
fn decode_slack_text(text: &str) -> String {
    let mut result = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&vert;", "|");

    // Remove angle brackets around URLs (Slack wraps URLs in <http://...> or <http://...|label>)
    loop {
        if let Some(start) = result.find("<http") {
            if let Some(end) = result[start..].find('>') {
                let url_part = result[start + 1..start + end].to_string();
                // Remove label if present (e.g., "http://example.com|label" -> "http://example.com")
                let url = url_part.split('|').next().unwrap_or(&url_part).to_string();
                result.replace_range(start..start + end + 1, &url);
            } else {
                break;
            }
        } else {
            break;
        }
    }

    result
}

/// Simplified Slack event type used internally by the application.
/// Converts from slack_morphism's complex event types to our domain model.
#[derive(Debug, Clone)]
pub struct SlackEvent {
    pub channel: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub text: String,
}

/// Slack client wrapper for Socket Mode connection and API calls.
/// Handles both receiving events (via Socket Mode) and sending messages (via Web API).
pub struct SlackConnection {
    client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    bot_token: SlackApiToken,
    bot_token_str: String,
    rate_limit_tx: mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>,
}

impl SlackConnection {
    /// Creates a new Slack connection with the given bot token.
    /// Does not establish connection yet - call connect() to start listening.
    pub fn new(bot_token: String) -> Self {
        // Create Slack HTTP client
        let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new().unwrap()));
        // Create rate limit channel
        let (rate_limit_tx, rate_limit_rx) = mpsc::unbounded_channel();

        // Spawn rate limit worker task
        tokio::spawn(async move {
            Self::rate_limit_worker(rate_limit_rx).await;
        });

        // Initialize connection struct
        Self {
            client,
            bot_token: SlackApiToken::new(bot_token.clone().into()),
            bot_token_str: bot_token,
            rate_limit_tx,
        }
    }

    /// Rate limiting worker that ensures API requests are spaced out by at least MIN_INTERVAL.
    /// Receives permit requests and sends back permits after enforcing the minimum interval.
    async fn rate_limit_worker(
        mut rate_limit_rx: mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<()>>,
    ) {
        const MIN_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_millis(800);

        while let Some(permit_tx) = rate_limit_rx.recv().await {
            // Immediately send permit back (allow request to proceed)
            let _ = permit_tx.send(());
            // Sleep for MIN_INTERVAL to enforce rate limit
            tokio::time::sleep(MIN_INTERVAL).await;
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
        debug!("Connecting to Slack Socket Mode");

        // Register push event callback handler
        let callbacks = SlackSocketModeListenerCallbacks::new().with_push_events(handle_push_event);

        // Create listener environment with event sender in user state
        let listener_env = Arc::new(
            SlackClientEventsListenerEnvironment::new(self.client.clone())
                .with_user_state(event_tx),
        );

        // Create Socket Mode listener
        let listener = SlackClientSocketModeListener::new(
            &SlackClientSocketModeConfig::new(),
            listener_env,
            callbacks,
        );

        // Connect with app token
        let app_token = SlackApiToken::new(app_token.into());
        listener.listen_for(&app_token).await?;
        info!("Slack Socket Mode connected");
        // Start serving events (blocks until disconnected)
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

        let blocks = vec![json!({
            "type": "markdown",
            "text": text
        })];

        self.send_message_with_blocks(channel, thread_ts, text, blocks)
            .await
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

        let blocks = vec![json!({
            "type": "markdown",
            "text": text
        })];

        self.update_message_with_blocks(channel, ts, text, blocks)
            .await
    }

    /// Adds a reaction emoji to a message.
    pub async fn add_reaction(&self, channel: &str, ts: &str, emoji: &str) -> Result<()> {
        debug!(
            "Adding reaction: channel={}, ts={}, emoji={}",
            channel, ts, emoji
        );

        // Request rate limit permit
        let (permit_tx, permit_rx) = tokio::sync::oneshot::channel();
        self.rate_limit_tx
            .send(permit_tx)
            .context("Failed to send API request to rate limit worker")?;

        permit_rx
            .await
            .context("Rate limit worker dropped response")?;

        // Call Slack API reactions.add
        let session = self.client.open_session(&self.bot_token);

        let req = SlackApiReactionsAddRequest::new(channel.into(), emoji.into(), ts.into());

        session
            .reactions_add(&req)
            .await
            .context("Failed to add reaction")?;

        Ok(())
    }

    /// Uploads a file/snippet to Slack with syntax highlighting.
    pub async fn upload_file(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        content: &str,
        filename: &str,
        title: Option<&str>,
    ) -> Result<()> {
        // Request rate limit permit
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.rate_limit_tx
            .send(resp_tx)
            .context("Failed to send upload file request")?;

        resp_rx
            .await
            .context("Rate limit worker dropped response")?;

        debug!(
            "Uploading file to channel={}, thread_ts={:?}, filename={}",
            channel, thread_ts, filename
        );
        let session = self.client.open_session(&self.bot_token);

        // Get upload URL from Slack (files.getUploadUrlExternal)
        let get_url_req =
            SlackApiFilesGetUploadUrlExternalRequest::new(filename.into(), content.len());
        let url_resp = session
            .get_upload_url_external(&get_url_req)
            .await
            .context("Failed to get upload URL")?;

        // Upload file content to the URL via HTTP POST
        let http_client = reqwest::Client::new();
        http_client
            .post(url_resp.upload_url.0.as_str())
            .body(content.to_string())
            .send()
            .await
            .context("Failed to upload file content")?;

        // Complete the upload (files.completeUploadExternal) with channel/thread info
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

    /// Sends a message with custom blocks to a Slack channel or thread.
    /// Returns the timestamp (ts) of the sent message.
    pub async fn send_message_with_blocks(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        blocks: Vec<Value>,
    ) -> Result<String> {
        // Build request body with channel, text, and blocks
        let mut body = json!({
            "channel": channel,
            "text": text,
            "blocks": blocks
        });

        // Add thread_ts if replying in a thread
        if let Some(thread_ts) = thread_ts {
            body.as_object_mut()
                .context("Failed to build chat.postMessage body")?
                .insert(
                    "thread_ts".to_string(),
                    Value::String(thread_ts.to_string()),
                );
        }

        // Request rate limit permit
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.rate_limit_tx
            .send(resp_tx)
            .context("Failed to send API request to rate limit worker")?;

        resp_rx
            .await
            .context("Rate limit worker dropped response")?;

        // Call Slack API chat.postMessage
        let resp =
            Self::invoke_slack_api_static("chat.postMessage", &body, &self.bot_token_str).await?;
        // Extract and return message timestamp
        let ts = resp
            .get("ts")
            .and_then(Value::as_str)
            .context("Slack chat.postMessage response missing ts")?;
        Ok(ts.to_string())
    }

    /// Updates an existing message with custom blocks.
    pub async fn update_message_with_blocks(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: Vec<Value>,
    ) -> Result<()> {
        // Build request body with channel, ts, text, and blocks
        let body = json!({
            "channel": channel,
            "ts": ts,
            "text": text,
            "blocks": blocks
        });

        // Request rate limit permit
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        self.rate_limit_tx
            .send(resp_tx)
            .context("Failed to send API request to rate limit worker")?;

        resp_rx
            .await
            .context("Rate limit worker dropped response")?;

        // Call Slack API chat.update
        Self::invoke_slack_api_static("chat.update", &body, &self.bot_token_str).await?;
        Ok(())
    }

    /// Low-level method to invoke Slack Web API endpoints.
    /// Handles authentication, error checking, and response parsing.
    async fn invoke_slack_api_static(method: &str, body: &Value, bot_token: &str) -> Result<Value> {
        // Build API URL from method name
        let uri = format!("https://slack.com/api/{method}");
        let client = reqwest::Client::new();

        // Send POST request with bearer token auth
        let resp = client
            .post(uri)
            .bearer_auth(bot_token)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/json; charset=utf-8",
            )
            .json(body)
            .send()
            .await
            .with_context(|| format!("Failed to call Slack API method {method}"))?;

        // Parse JSON response
        let status = resp.status();
        let parsed: Value = resp
            .json()
            .await
            .with_context(|| format!("Failed to parse Slack API response for {method}"))?;

        // Check HTTP status code
        if !status.is_success() {
            anyhow::bail!("Slack API {method} returned HTTP {status}: {parsed}");
        }

        // Check Slack API 'ok' field
        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");

            // Extract error details if present
            let details = parsed
                .get("response_metadata")
                .and_then(|v| v.get("messages"))
                .and_then(Value::as_array)
                .map(|messages| {
                    messages
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .filter(|s| !s.is_empty());

            if let Some(details) = details {
                anyhow::bail!("Slack API {method} failed: {err} | {details}");
            }

            anyhow::bail!("Slack API {method} failed: {err}");
        }

        Ok(parsed)
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
    // Extract event sender channel from user state
    let tx = state
        .read()
        .await
        .get_user_state::<mpsc::UnboundedSender<SlackEvent>>()
        .cloned()
        .ok_or("No event sender in state")?;

    // Match on event type (Message or AppMention)
    match event.event {
        SlackEventCallbackBody::Message(msg) => {
            // Ignore bot messages to prevent loops
            if msg.sender.bot_id.is_some() {
                trace!("Ignoring bot message");
                return Ok(());
            }

            if let Some(content) = msg.content {
                if let Some(text) = content.text {
                    // Decode Slack text formatting
                    let text = decode_slack_text(&text);
                    let user = msg.sender.user.map(|u| u.to_string()).unwrap_or_default();
                    debug!(
                        "Received message from user={}, channel={:?}",
                        user, msg.origin.channel
                    );
                    // Convert to SlackEvent and send to channel
                    let _ = tx.send(SlackEvent {
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
            // Decode Slack text formatting
            let text = decode_slack_text(&text);
            debug!(
                "Received app mention from user={}, channel={}",
                user, mention.channel
            );

            // Convert to SlackEvent and send to channel
            let _ = tx.send(SlackEvent {
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
