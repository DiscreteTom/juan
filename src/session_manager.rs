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
pub struct SessionState {
    /// ACP session ID (initially a placeholder, updated after first message)
    pub session_id: SessionId,
    /// Name of the agent handling this session
    pub agent_name: String,
    /// Workspace directory path for the agent
    pub workspace: String,
    /// Whether to auto-approve tool calls for this session
    pub auto_approve: bool,
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
    pub async fn create_session(
        &self,
        thread_key: String,
        agent_name: String,
        workspace: Option<String>,
    ) -> Result<SessionState> {
        debug!(
            "Creating session: thread_key={}, agent={}, workspace={:?}",
            thread_key, agent_name, workspace
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

        // Create placeholder session ID (will be updated after first ACP session creation)
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
            .insert(thread_key.clone(), session.clone());

        trace!("Session created successfully: {:?}", session.session_id);
        Ok(session)
    }

    /// Retrieves the session for a given Slack thread.
    pub async fn get_session(&self, thread_key: &str) -> Option<SessionState> {
        self.sessions.read().await.get(thread_key).cloned()
    }

    /// Updates the ACP session ID for a thread.
    /// Called after the first message creates the actual ACP session.
    pub async fn update_session_id(&self, thread_key: &str, session_id: SessionId) -> Result<()> {
        debug!(
            "Updating session ID for thread_key={}, new_session_id={}",
            thread_key, session_id
        );
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(thread_key) {
            session.session_id = session_id;
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

    /// Lists all active sessions.
    /// Returns a vector of (thread_key, session_state) tuples.
    pub async fn list_sessions(&self) -> Vec<(String, SessionState)> {
        self.sessions
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Expands a workspace path (e.g., ~ to home directory).
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
