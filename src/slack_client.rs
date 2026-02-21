use anyhow::{Context, Result};
use slack_morphism::prelude::*;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info};

#[derive(Debug, Clone)]
pub enum SlackEvent {
    Message {
        channel: String,
        thread_ts: Option<String>,
        text: String,
        user: String,
    },
    AppMention {
        channel: String,
        thread_ts: Option<String>,
        text: String,
        user: String,
    },
}

pub struct SlackConnection {
    client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    bot_token: SlackApiToken,
}

impl SlackConnection {
    pub fn new(bot_token: String) -> Self {
        let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new().unwrap()));

        Self {
            client,
            bot_token: SlackApiToken::new(bot_token.into()),
        }
    }

    pub async fn connect(
        self: Arc<Self>,
        app_token: String,
        event_tx: mpsc::UnboundedSender<SlackEvent>,
    ) -> Result<()> {
        info!("Connecting to Slack Socket Mode");

        let callbacks = SlackSocketModeListenerCallbacks::new().with_push_events(handle_push_event);

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

        if let Some(ts) = thread_ts {
            req = req.with_thread_ts(ts.into());
        }

        let resp = session
            .chat_post_message(&req)
            .await
            .context("Failed to send Slack message")?;

        Ok(resp.ts.to_string())
    }

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

async fn handle_push_event(
    event: SlackPushEventCallback,
    _client: Arc<SlackClient<SlackClientHyperHttpsConnector>>,
    state: SlackClientEventsUserState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tx = state
        .read()
        .await
        .get_user_state::<mpsc::UnboundedSender<SlackEvent>>()
        .cloned()
        .ok_or("No event sender in state")?;

    match event.event {
        SlackEventCallbackBody::Message(msg) => {
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
                thread_ts: mention.origin.thread_ts.map(|ts| ts.to_string()),
                text,
                user,
            });
        }
        _ => debug!("Unhandled callback event"),
    }

    Ok(())
}
