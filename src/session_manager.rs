use agent_client_protocol::*;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::Config;

#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    config: Arc<Config>,
}

pub struct SessionState {
    pub session_id: SessionId,
    pub agent_name: String,
    pub workspace: String,
    pub auto_approve: bool,
}

impl SessionManager {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    pub async fn create_session(
        &self,
        thread_key: String,
        agent_name: String,
        workspace: Option<String>,
    ) -> Result<SessionState> {
        let workspace = workspace.unwrap_or_else(|| self.config.bridge.default_workspace.clone());

        let agent_config = self
            .config
            .agents
            .iter()
            .find(|a| a.name == agent_name)
            .ok_or_else(|| anyhow::anyhow!("Agent not found: {}", agent_name))?;

        let auto_approve = agent_config
            .auto_approve
            .unwrap_or(self.config.bridge.auto_approve);

        let session_id = SessionId::from(format!("session-{}", thread_key));

        let session = SessionState {
            session_id,
            agent_name,
            workspace,
            auto_approve,
        };

        self.sessions
            .write()
            .await
            .insert(thread_key, session.clone());

        Ok(session)
    }

    pub async fn get_session(&self, thread_key: &str) -> Option<SessionState> {
        self.sessions.read().await.get(thread_key).cloned()
    }

    pub async fn update_session_id(&self, thread_key: &str, session_id: SessionId) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(thread_key) {
            session.session_id = session_id;
            Ok(())
        } else {
            Err(anyhow::anyhow!("Session not found: {}", thread_key))
        }
    }

    pub async fn end_session(&self, thread_key: &str) -> Result<()> {
        self.sessions
            .write()
            .await
            .remove(thread_key)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", thread_key))?;
        Ok(())
    }

    pub async fn list_sessions(&self) -> Vec<(String, SessionState)> {
        self.sessions
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn expand_workspace_path(&self, path: &str) -> String {
        self.config.expand_path(path).to_string_lossy().to_string()
    }
}

impl Clone for SessionState {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            agent_name: self.agent_name.clone(),
            workspace: self.workspace.clone(),
            auto_approve: self.auto_approve,
        }
    }
}
