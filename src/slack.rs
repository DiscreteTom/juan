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

fn convert_markdown_emphasis_segment(segment: &str) -> String {
    fn starts_with_at(text: &str, i: usize, pat: &str) -> bool {
        text.get(i..)
            .map(|rest| rest.starts_with(pat))
            .unwrap_or(false)
    }

    let mut out = String::with_capacity(segment.len());
    let mut i = 0;

    while i < segment.len() {
        if starts_with_at(segment, i, "**") {
            let rest = match segment.get(i + 2..) {
                Some(r) => r,
                None => "",
            };
            if let Some(end_rel) = rest.find("**") {
                let inner = &rest[..end_rel];
                if !inner.trim().is_empty() {
                    out.push('*');
                    out.push_str(inner);
                    out.push('*');
                    i += 2 + end_rel + 2;
                    continue;
                }
            }
        }

        if starts_with_at(segment, i, "__") {
            let rest = match segment.get(i + 2..) {
                Some(r) => r,
                None => "",
            };
            if let Some(end_rel) = rest.find("__") {
                let inner = &rest[..end_rel];
                if !inner.trim().is_empty() {
                    out.push('*');
                    out.push_str(inner);
                    out.push('*');
                    i += 2 + end_rel + 2;
                    continue;
                }
            }
        }

        if starts_with_at(segment, i, "*") {
            let rest = match segment.get(i + 1..) {
                Some(r) => r,
                None => "",
            };
            if let Some(first) = rest.chars().next() {
                if !first.is_whitespace() {
                    if let Some(end_rel) = rest.find('*') {
                        let inner = &rest[..end_rel];
                        if !inner.trim().is_empty() && !inner.ends_with(' ') {
                            out.push('_');
                            out.push_str(inner);
                            out.push('_');
                            i += 1 + end_rel + 1;
                            continue;
                        }
                    }
                }
            }
        }

        if let Some(ch) = segment.get(i..).and_then(|rest| rest.chars().next()) {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    out
}

fn normalize_inline_markdown_for_slack(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut buf = String::new();
    let mut in_inline_code = false;

    for ch in line.chars() {
        if ch == '`' {
            if in_inline_code {
                out.push_str(&buf);
                buf.clear();
                out.push('`');
                in_inline_code = false;
            } else {
                out.push_str(&convert_markdown_emphasis_segment(&buf));
                buf.clear();
                out.push('`');
                in_inline_code = true;
            }
        } else {
            buf.push(ch);
        }
    }

    if in_inline_code {
        out.push_str(&buf);
    } else {
        out.push_str(&convert_markdown_emphasis_segment(&buf));
    }

    out
}

fn normalize_markdown_for_slack(text: &str) -> String {
    let mut out_lines = Vec::new();
    let mut in_fenced_code = false;

    for line in text.replace('\r', "").lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fenced_code = !in_fenced_code;
            out_lines.push(line.to_string());
            continue;
        }

        if in_fenced_code {
            out_lines.push(line.to_string());
            continue;
        }

        out_lines.push(normalize_inline_markdown_for_slack(line));
    }

    out_lines.join("\n")
}

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
    bot_token_raw: String,
}

impl SlackConnection {
    /// Creates a new Slack connection with the given bot token.
    /// Does not establish connection yet - call connect() to start listening.
    pub fn new(bot_token: String) -> Self {
        let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new().unwrap()));

        Self {
            client,
            bot_token: SlackApiToken::new(bot_token.clone().into()),
            bot_token_raw: bot_token,
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
        let text = normalize_markdown_for_slack(text);
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
        )
        // Force Slack's markdown parsing mode explicitly instead of relying on defaults.
        .with_parse("none".into());

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
        let text = normalize_markdown_for_slack(text);
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
        )
        // Keep update behavior consistent with chat.postMessage formatting.
        .with_parse("none".into());

        session
            .chat_update(&req)
            .await
            .context("Failed to update Slack message")?;

        Ok(())
    }

    /// Sends a Slack message using raw Web API payload so unsupported block types
    /// (e.g. type="plan") can be passed through unchanged.
    pub async fn send_message_with_blocks(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        blocks: Vec<Value>,
    ) -> Result<String> {
        let text = normalize_markdown_for_slack(text);
        let mut body = json!({
            "channel": channel,
            "text": text,
            "blocks": blocks
        });

        if let Some(thread_ts) = thread_ts {
            body.as_object_mut()
                .context("Failed to build chat.postMessage body")?
                .insert("thread_ts".to_string(), Value::String(thread_ts.to_string()));
        }

        let resp = self.invoke_slack_api("chat.postMessage", &body).await?;
        let ts = resp
            .get("ts")
            .and_then(Value::as_str)
            .context("Slack chat.postMessage response missing ts")?;
        Ok(ts.to_string())
    }

    /// Updates a Slack message using raw Web API payload so unsupported block types
    /// (e.g. type="plan") can be passed through unchanged.
    pub async fn update_message_with_blocks(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: Vec<Value>,
    ) -> Result<()> {
        let text = normalize_markdown_for_slack(text);
        let body = json!({
            "channel": channel,
            "ts": ts,
            "text": text,
            "blocks": blocks
        });

        self.invoke_slack_api("chat.update", &body).await?;
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

    async fn invoke_slack_api(&self, method: &str, body: &Value) -> Result<Value> {
        let uri = format!("https://slack.com/api/{method}");
        let client = reqwest::Client::new();

        let resp = client
            .post(uri)
            .bearer_auth(&self.bot_token_raw)
            .header(reqwest::header::CONTENT_TYPE, "application/json; charset=utf-8")
            .json(body)
            .send()
            .await
            .with_context(|| format!("Failed to call Slack API method {method}"))?;

        let status = resp.status();
        let parsed: Value = resp
            .json()
            .await
            .with_context(|| format!("Failed to parse Slack API response for {method}"))?;

        if !status.is_success() {
            anyhow::bail!("Slack API {method} returned HTTP {status}: {parsed}");
        }

        if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error");

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
