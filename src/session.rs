/// Session manager for tracking Slack thread to agent session mappings.
///
/// Each Slack thread can have an active session with an agent.
/// Sessions track:
/// - Which agent is being used
/// - The workspace directory for the agent
/// - The ACP session ID
/// - Auto-approve settings
use agent_client_protocol::*;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, trace};

use crate::config::Config;

/// Manages all active sessions between Slack threads and agents.
/// Thread-safe via RwLock for concurrent access.
#[derive(Clone)]
pub struct SessionManager {
    /// Map from Slack thread key (thread_ts or channel) to session state
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    config: Arc<Config>,
}

/// State for a single active session.
/// Tracks the connection between a Slack thread and an ACP agent session.
#[derive(Clone)]
pub struct SessionState {
    /// ACP session ID (initially a placeholder, updated after first message)
    pub session_id: SessionId,
    /// Name of the agent handling this session
    pub agent_name: String,
    /// Workspace directory path for the agent
    pub workspace: String,
    /// Whether to auto-approve tool calls for this session
    pub auto_approve: bool,
    /// Slack channel ID where this session is active
    pub channel: String,
    /// Whether the session is currently processing a prompt
    pub busy: bool,
    /// Timestamp of the user's #new message
    pub initial_ts: String,
    /// Session configuration options (modes, models, etc.)
    pub config_options: Option<Vec<SessionConfigOption>>,
    /// Deprecated: Session modes (for backward compatibility)
    pub modes: Option<SessionModeState>,
    /// Deprecated: Session models (for backward compatibility)
    pub models: Option<SessionModelState>,
}

impl SessionManager {
    /// Creates a new session manager with the given configuration.
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Creates a new session for a Slack thread.
    ///
    /// # Arguments
    /// * `thread_key` - Slack thread_ts (or channel if not in thread)
    /// * `agent_name` - Name of the agent to use
    /// * `workspace` - Optional workspace path (uses default if not provided)
    /// * `channel` - Slack channel ID
    /// * `session_id` - ACP session ID
    pub async fn create_session(
        &self,
        thread_key: String,
        agent_name: String,
        workspace: Option<String>,
        channel: String,
        session_id: SessionId,
    ) -> Result<SessionState> {
        debug!(
            "Creating session: thread_key={}, agent={}, workspace={:?}, session_id={}",
            thread_key, agent_name, workspace, session_id
        );
        let workspace = workspace.unwrap_or_else(|| self.config.bridge.default_workspace.clone());

        // Look up agent configuration to get auto-approve setting
        let agent_config = self
            .config
            .agents
            .iter()
            .find(|a| a.name == agent_name)
            .ok_or_else(|| anyhow::anyhow!("Agent not found: {}", agent_name))?;

        let auto_approve = agent_config.auto_approve;

        let session = SessionState {
            session_id,
            agent_name,
            workspace,
            auto_approve,
            channel,
            busy: false,
            initial_ts: thread_key.clone(),
            config_options: None,
            modes: None,
            models: None,
        };

        self.sessions
            .write()
            .await
            .insert(thread_key.clone(), session.clone());

        trace!("Session created successfully: {:?}", session.session_id);
        Ok(session)
    }

    /// Retrieves the session for a given Slack thread.
    pub async fn get_session(&self, thread_key: &str) -> Option<SessionState> {
        self.sessions.read().await.get(thread_key).cloned()
    }

    /// Sets the busy flag for a session.
    pub async fn set_busy(&self, thread_key: &str, busy: bool) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(thread_key) {
            session.busy = busy;
            Ok(())
        } else {
            Err(anyhow::anyhow!("Session not found: {}", thread_key))
        }
    }

    /// Ends a session and removes it from tracking.
    pub async fn end_session(&self, thread_key: &str) -> Result<()> {
        debug!("Ending session for thread_key={}", thread_key);
        self.sessions
            .write()
            .await
            .remove(thread_key)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", thread_key))?;
        Ok(())
    }

    /// Returns readonly access to all sessions.
    pub fn sessions(&self) -> Arc<RwLock<HashMap<String, SessionState>>> {
        self.sessions.clone()
    }

    /// Finds a session by session_id.
    /// Returns (thread_key, session_state) if found.
    pub async fn find_by_session_id(
        &self,
        session_id: &SessionId,
    ) -> Option<(String, SessionState)> {
        self.sessions
            .read()
            .await
            .iter()
            .find(|(_, session)| &session.session_id == session_id)
            .map(|(k, v)| (k.clone(), v.clone()))
    }

    /// Updates the config_options for a session.
    pub async fn update_config_options(
        &self,
        thread_key: &str,
        config_options: Vec<SessionConfigOption>,
    ) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(thread_key) {
            session.config_options = Some(config_options);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Session not found: {}", thread_key))
        }
    }

    /// Updates the modes for a session (deprecated API).
    pub async fn update_modes(&self, thread_key: &str, modes: SessionModeState) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(thread_key) {
            session.modes = Some(modes);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Session not found: {}", thread_key))
        }
    }

    /// Updates the models for a session (deprecated API).
    pub async fn update_models(&self, thread_key: &str, models: SessionModelState) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(thread_key) {
            session.models = Some(models);
            Ok(())
        } else {
            Err(anyhow::anyhow!("Session not found: {}", thread_key))
        }
    }
}
