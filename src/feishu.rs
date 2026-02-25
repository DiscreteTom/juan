/// Feishu/Lark client module for WebSocket long-connection and message handling.
///
/// This module provides:
/// - FeishuConnection: Client for sending/updating messages
/// - FeishuEvent: Simplified event types for the application
/// - WebSocket long-connection listener for receiving events from Feishu
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, trace, warn};

/// Simplified Feishu event type used internally by the application.
#[derive(Debug, Clone)]
pub struct FeishuEvent {
    pub chat_id: String,
    pub message_id: String,
    pub parent_id: Option<String>,
    pub text: String,
    pub files: Vec<FeishuFile>,
}

/// Feishu file attachment
#[derive(Debug, Clone)]
pub struct FeishuFile {
    pub file_key: String,
    pub file_name: String,
    pub file_type: String,
}

/// Feishu client wrapper for WebSocket long-connection and API calls.
pub struct FeishuConnection {
    app_id: String,
    app_secret: String,
    tenant_access_token: Arc<tokio::sync::RwLock<Option<String>>>,
    rate_limit_tx: mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>,
}

impl FeishuConnection {
    /// Creates a new Feishu connection with the given app credentials.
    pub fn new(app_id: String, app_secret: String) -> Self {
        let (rate_limit_tx, rate_limit_rx) = mpsc::unbounded_channel();

        // Spawn rate limit worker task
        tokio::spawn(async move {
            Self::rate_limit_worker(rate_limit_rx).await;
        });

        Self {
            app_id,
            app_secret,
            tenant_access_token: Arc::new(tokio::sync::RwLock::new(None)),
            rate_limit_tx,
        }
    }

    /// Rate limiting worker that ensures API requests are spaced out.
    async fn rate_limit_worker(
        mut rate_limit_rx: mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<()>>,
    ) {
        const MIN_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_millis(800);

        while let Some(permit_tx) = rate_limit_rx.recv().await {
            let _ = permit_tx.send(());
            tokio::time::sleep(MIN_INTERVAL).await;
        }
    }

    /// Get or refresh tenant access token
    async fn get_tenant_access_token(&self) -> Result<String> {
        // Check if we have a valid token
        {
            let token = self.tenant_access_token.read().await;
            if let Some(t) = token.as_ref() {
                return Ok(t.clone());
            }
        }

        // Fetch new token
        debug!("Fetching new tenant access token");
        let client = reqwest::Client::new();
        let resp = client
            .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret
            }))
            .send()
            .await
            .context("Failed to request tenant access token")?;

        let data: Value = resp
            .json()
            .await
            .context("Failed to parse token response")?;

        if data.get("code").and_then(Value::as_i64) != Some(0) {
            anyhow::bail!("Failed to get tenant access token: {:?}", data);
        }

        let token = data
            .get("tenant_access_token")
            .and_then(Value::as_str)
            .context("Missing tenant_access_token in response")?
            .to_string();

        // Store token
        {
            let mut t = self.tenant_access_token.write().await;
            *t = Some(token.clone());
        }

        Ok(token)
    }

    /// Establishes WebSocket long-connection and starts listening for events.
    pub async fn connect(
        self: Arc<Self>,
        event_tx: mpsc::UnboundedSender<FeishuEvent>,
    ) -> Result<()> {
        debug!("Connecting to Feishu WebSocket long-connection");

        // Get tenant access token first
        let token = self.get_tenant_access_token().await?;

        // Connect to WebSocket endpoint
        let ws_url = format!(
            "wss://open.feishu.cn/open-apis/ws/v1/connect?app_id={}&token={}",
            self.app_id, token
        );

        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .context("Failed to connect to Feishu WebSocket")?;

        info!("Feishu WebSocket long-connection established");

        let (mut write, mut read) = ws_stream.split();

        // Spawn heartbeat task
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                if write.send(Message::Ping(vec![])).await.is_err() {
                    break;
                }
            }
        });

        // Process incoming messages
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    trace!("Received WebSocket message: {}", text);
                    if let Err(e) = self.handle_ws_message(&text, &event_tx).await {
                        warn!("Failed to handle WebSocket message: {}", e);
                    }
                }
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                    trace!("Received ping/pong");
                }
                Ok(Message::Close(_)) => {
                    info!("WebSocket connection closed");
                    break;
                }
                Err(e) => {
                    warn!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Handle incoming WebSocket message
    async fn handle_ws_message(
        &self,
        text: &str,
        event_tx: &mpsc::UnboundedSender<FeishuEvent>,
    ) -> Result<()> {
        let data: Value = serde_json::from_str(text)?;

        // Check event type
        let event_type = data.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            "im.message.receive_v1" => {
                self.handle_message_event(&data, event_tx).await?;
            }
            "url_verification" => {
                // Handle verification challenge (if needed)
                debug!("Received URL verification challenge");
            }
            _ => {
                trace!("Unhandled event type: {}", event_type);
            }
        }

        Ok(())
    }

    /// Handle message receive event
    async fn handle_message_event(
        &self,
        data: &Value,
        event_tx: &mpsc::UnboundedSender<FeishuEvent>,
    ) -> Result<()> {
        let event = data.get("event").context("Missing event field")?;

        let message = event.get("message").context("Missing message field")?;
        let chat_id = message
            .get("chat_id")
            .and_then(Value::as_str)
            .context("Missing chat_id")?
            .to_string();
        let message_id = message
            .get("message_id")
            .and_then(Value::as_str)
            .context("Missing message_id")?
            .to_string();
        let parent_id = message
            .get("parent_id")
            .and_then(Value::as_str)
            .map(String::from);

        // Extract text content
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let content_json: Value = serde_json::from_str(content).unwrap_or(json!({}));
        let text = content_json
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // Extract files (if any)
        let files = vec![]; // TODO: implement file extraction

        let feishu_event = FeishuEvent {
            chat_id,
            message_id,
            parent_id,
            text,
            files,
        };

        let _ = event_tx.send(feishu_event);
        Ok(())
    }

    /// Sends a message to a Feishu chat.
    pub async fn send_message(
        &self,
        chat_id: &str,
        parent_id: Option<&str>,
        text: &str,
    ) -> Result<String> {
        debug!(
            "Sending message to chat={}, parent_id={:?}, text_len={}",
            chat_id,
            parent_id,
            text.len()
        );

        let content = json!({
            "text": text
        });

        self.send_message_with_content(chat_id, parent_id, "text", content)
            .await
    }

    /// Sends a message with custom content type
    async fn send_message_with_content(
        &self,
        chat_id: &str,
        parent_id: Option<&str>,
        msg_type: &str,
        content: Value,
    ) -> Result<String> {
        let token = self.get_tenant_access_token().await?;

        // Request rate limit permit
        let (permit_tx, permit_rx) = tokio::sync::oneshot::channel();
        self.rate_limit_tx.send(permit_tx)?;
        permit_rx.await?;

        let mut body = json!({
            "receive_id": chat_id,
            "msg_type": msg_type,
            "content": serde_json::to_string(&content)?
        });

        if let Some(pid) = parent_id {
            body.as_object_mut()
                .unwrap()
                .insert("reply_in_thread".to_string(), json!(true));
            body.as_object_mut()
                .unwrap()
                .insert("root_id".to_string(), json!(pid));
        }

        let client = reqwest::Client::new();
        let resp = client
            .post("https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id")
            .header("Authorization", format!("Bearer {}", token))
            .json(&body)
            .send()
            .await
            .context("Failed to send message")?;

        let data: Value = resp.json().await.context("Failed to parse response")?;

        if data.get("code").and_then(Value::as_i64) != Some(0) {
            anyhow::bail!("Failed to send message: {:?}", data);
        }

        let message_id = data
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(Value::as_str)
            .context("Missing message_id in response")?
            .to_string();

        Ok(message_id)
    }

    /// Updates an existing message
    pub async fn update_message(&self, message_id: &str, text: &str) -> Result<()> {
        debug!(
            "Updating message: message_id={}, text_len={}",
            message_id,
            text.len()
        );

        let token = self.get_tenant_access_token().await?;

        // Request rate limit permit
        let (permit_tx, permit_rx) = tokio::sync::oneshot::channel();
        self.rate_limit_tx.send(permit_tx)?;
        permit_rx.await?;

        let content = json!({
            "text": text
        });

        let body = json!({
            "msg_type": "text",
            "content": serde_json::to_string(&content)?
        });

        let client = reqwest::Client::new();
        let resp = client
            .patch(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages/{}",
                message_id
            ))
            .header("Authorization", format!("Bearer {}", token))
            .json(&body)
            .send()
            .await
            .context("Failed to update message")?;

        let data: Value = resp.json().await.context("Failed to parse response")?;

        if data.get("code").and_then(Value::as_i64) != Some(0) {
            anyhow::bail!("Failed to update message: {:?}", data);
        }

        Ok(())
    }

    /// Adds a reaction emoji to a message
    pub async fn add_reaction(&self, message_id: &str, emoji: &str) -> Result<()> {
        debug!(
            "Adding reaction: message_id={}, emoji={}",
            message_id, emoji
        );

        let token = self.get_tenant_access_token().await?;

        // Request rate limit permit
        let (permit_tx, permit_rx) = tokio::sync::oneshot::channel();
        self.rate_limit_tx.send(permit_tx)?;
        permit_rx.await?;

        let body = json!({
            "reaction_type": {
                "emoji_type": emoji
            }
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages/{}/reactions",
                message_id
            ))
            .header("Authorization", format!("Bearer {}", token))
            .json(&body)
            .send()
            .await
            .context("Failed to add reaction")?;

        let data: Value = resp.json().await.context("Failed to parse response")?;

        if data.get("code").and_then(Value::as_i64) != Some(0) {
            anyhow::bail!("Failed to add reaction: {:?}", data);
        }

        Ok(())
    }

    /// Uploads a file to Feishu
    pub async fn upload_file(
        &self,
        chat_id: &str,
        parent_id: Option<&str>,
        content: &str,
        filename: &str,
        _title: Option<&str>,
    ) -> Result<()> {
        debug!(
            "Uploading file to chat={}, parent_id={:?}, filename={}",
            chat_id, parent_id, filename
        );

        // For now, send as text message with code block
        let formatted = format!("```\n{}\n```\nFile: {}", content, filename);
        self.send_message(chat_id, parent_id, &formatted).await?;
        Ok(())
    }

    /// Downloads a file from Feishu
    pub async fn download_file(&self, file_key: &str) -> Result<Vec<u8>> {
        debug!("Downloading file: file_key={}", file_key);

        let token = self.get_tenant_access_token().await?;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "https://open.feishu.cn/open-apis/im/v1/messages/{}/resources/{}",
                file_key, file_key
            ))
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await
            .context("Failed to download file")?;

        if !resp.status().is_success() {
            anyhow::bail!("Failed to download file: HTTP {}", resp.status());
        }

        let bytes = resp.bytes().await.context("Failed to read file bytes")?;
        Ok(bytes.to_vec())
    }

    /// Uploads binary data to Feishu
    pub async fn upload_binary_file(
        &self,
        chat_id: &str,
        parent_id: Option<&str>,
        _content: &[u8],
        filename: &str,
        _title: Option<&str>,
    ) -> Result<()> {
        debug!(
            "Uploading binary file to chat={}, parent_id={:?}, filename={}",
            chat_id, parent_id, filename
        );

        // For now, send as text message
        let msg = format!("Binary file: {}", filename);
        self.send_message(chat_id, parent_id, &msg).await?;
        Ok(())
    }

    /// Sends a message with interactive card (buttons)
    pub async fn send_message_with_blocks(
        &self,
        chat_id: &str,
        parent_id: Option<&str>,
        text: &str,
        _blocks: Vec<Value>,
    ) -> Result<String> {
        debug!(
            "Sending message with blocks to chat={}, parent_id={:?}",
            chat_id, parent_id
        );

        // For now, just send as text
        // TODO: implement proper message card format
        self.send_message(chat_id, parent_id, text).await
    }

    /// Updates a message with interactive card
    pub async fn update_message_with_blocks(
        &self,
        message_id: &str,
        text: &str,
        _blocks: Vec<Value>,
    ) -> Result<()> {
        debug!("Updating message with blocks: message_id={}", message_id);

        // For now, just update as text
        // TODO: implement proper message card format
        self.update_message(message_id, text).await
    }
}
