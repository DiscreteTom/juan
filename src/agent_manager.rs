/// Agent manager for spawning and communicating with ACP agents.
///
/// This module handles:
/// - Spawning agent processes with stdio communication
/// - Managing ACP protocol connections
/// - Routing requests/responses between Slack and agents
/// - Handling agent notifications
use agent_client_protocol::*;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{error, info};

use crate::config::AgentConfig;

/// Manages all spawned agents and their communication channels.
pub struct AgentManager {
    /// Map of agent name to agent handle
    agents: Arc<RwLock<HashMap<String, AgentHandle>>>,
    /// Channel for receiving notifications from agents
    notification_tx: mpsc::UnboundedSender<(String, SessionNotification)>,
}

/// Handle for communicating with a spawned agent.
struct AgentHandle {
    config: AgentConfig,
    /// Channel for sending commands to the agent's task
    tx: mpsc::UnboundedSender<AgentCommand>,
}

/// Commands that can be sent to an agent task.
enum AgentCommand {
    /// Create a new ACP session
    NewSession {
        req: NewSessionRequest,
        resp_tx: oneshot::Sender<agent_client_protocol::Result<NewSessionResponse>>,
    },
    /// Send a prompt to an existing session
    Prompt {
        req: PromptRequest,
        resp_tx: oneshot::Sender<agent_client_protocol::Result<PromptResponse>>,
    },
}

impl AgentManager {
    /// Creates a new agent manager with the given notification channel.
    pub fn new(notification_tx: mpsc::UnboundedSender<(String, SessionNotification)>) -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            notification_tx,
        }
    }

    /// Spawns multiple agents from their configurations.
    /// Logs errors but continues spawning remaining agents if one fails.
    pub async fn spawn_agents(&self, configs: Vec<AgentConfig>) -> Result<()> {
        for config in configs {
            if let Err(e) = self.spawn_agent(config.clone()).await {
                error!("Failed to spawn agent {}: {}", config.name, e);
            }
        }
        Ok(())
    }

    /// Spawns a single agent process and sets up ACP communication.
    ///
    /// Creates:
    /// - Child process with stdin/stdout pipes
    /// - Dedicated thread with local runtime for ACP protocol
    /// - Command channel for sending requests
    /// - Notification forwarding to main loop
    async fn spawn_agent(&self, config: AgentConfig) -> Result<()> {
        info!("Spawning agent: {}", config.name);

        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .envs(&config.env);

        let mut process = cmd
            .spawn()
            .context(format!("Failed to spawn agent: {}", config.name))?;

        // Get stdio handles and convert to futures_io types for ACP
        let stdin = process
            .stdin
            .take()
            .context("Failed to get stdin")?
            .compat_write();
        let stdout = process
            .stdout
            .take()
            .context("Failed to get stdout")?
            .compat();

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let agent_name = config.name.clone();
        let agent_name2 = agent_name.clone();
        let notification_tx = self.notification_tx.clone();

        // Spawn dedicated thread for this agent's ACP communication
        // Uses LocalSet to allow !Send futures from ACP library
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let local = tokio::task::LocalSet::new();
            rt.block_on(local.run_until(async move {
                let client = NotificationClient {
                    agent_name: agent_name.clone(),
                    notification_tx,
                };
                let (connection, io_task) =
                    ClientSideConnection::new(client, stdin, stdout, |fut| {
                        tokio::task::spawn_local(fut);
                    });

                let agent_name_clone = agent_name.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = io_task.await {
                        error!("Agent {} IO task error: {}", agent_name_clone, e);
                    }
                });

                // Initialize the agent with ACP protocol
                let init_req = InitializeRequest::new(ProtocolVersion::LATEST)
                    .client_info(Implementation::new("anywhere", "0.1.0"));

                match connection.initialize(init_req).await {
                    Ok(_) => info!("Agent {} initialized successfully", agent_name),
                    Err(e) => {
                        error!("Failed to initialize agent {}: {}", agent_name, e);
                        return;
                    }
                }

                // Handle commands from the main loop
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        AgentCommand::NewSession { req, resp_tx } => {
                            let result = connection.new_session(req).await;
                            let _ = resp_tx.send(result);
                        }
                        AgentCommand::Prompt { req, resp_tx } => {
                            let result = connection.prompt(req).await;
                            let _ = resp_tx.send(result);
                        }
                    }
                }
            }));
        });

        let handle = AgentHandle {
            config: config.clone(),
            tx: cmd_tx,
        };

        self.agents.write().await.insert(agent_name2, handle);

        Ok(())
    }

    /// Creates a new ACP session with the specified agent.
    pub async fn new_session(
        &self,
        agent_name: &str,
        req: NewSessionRequest,
    ) -> Result<NewSessionResponse> {
        let handle = self
            .agents
            .read()
            .await
            .get(agent_name)
            .ok_or_else(|| anyhow::anyhow!("Agent not found: {}", agent_name))?
            .tx
            .clone();

        let (resp_tx, resp_rx) = oneshot::channel();
        handle
            .send(AgentCommand::NewSession { req, resp_tx })
            .context("Failed to send command to agent")?;

        resp_rx
            .await
            .context("Agent command channel closed")?
            .map_err(|e| anyhow::anyhow!("Agent error: {}", e))
    }

    /// Sends a prompt to an agent's existing session.
    pub async fn prompt(&self, agent_name: &str, req: PromptRequest) -> Result<PromptResponse> {
        let handle = self
            .agents
            .read()
            .await
            .get(agent_name)
            .ok_or_else(|| anyhow::anyhow!("Agent not found: {}", agent_name))?
            .tx
            .clone();

        let (resp_tx, resp_rx) = oneshot::channel();
        handle
            .send(AgentCommand::Prompt { req, resp_tx })
            .context("Failed to send command to agent")?;

        resp_rx
            .await
            .context("Agent command channel closed")?
            .map_err(|e| anyhow::anyhow!("Agent error: {}", e))
    }

    /// Lists all currently running agents.
    pub async fn list_agents(&self) -> Vec<String> {
        self.agents.read().await.keys().cloned().collect()
    }
}

/// ACP client implementation for handling agent notifications and permission requests.
struct NotificationClient {
    agent_name: String,
    notification_tx: mpsc::UnboundedSender<(String, SessionNotification)>,
}

#[async_trait::async_trait(?Send)]
impl Client for NotificationClient {
    /// Handles permission requests from agents.
    /// Currently auto-approves by selecting the first option.
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        let first_option = args
            .options
            .first()
            .ok_or_else(|| Error::invalid_params())?;

        Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                first_option.option_id.clone(),
            )),
        ))
    }

    /// Handles session notifications from agents (e.g., message chunks).
    /// Forwards to main loop for processing.
    async fn session_notification(
        &self,
        args: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        let _ = self.notification_tx.send((self.agent_name.clone(), args));
        Ok(())
    }
}
