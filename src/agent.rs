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
use tracing::{debug, error, info, trace};

use crate::config::AgentConfig;

/// Manages all spawned agents and their communication channels.
pub struct AgentManager {
    /// Map of agent name to agent handle
    agents: Arc<RwLock<HashMap<String, AgentHandle>>>,
    /// Channel for receiving notifications from agents
    notification_tx: mpsc::UnboundedSender<(String, SessionNotification)>,
    /// Map of session_id to auto_approve setting
    session_permissions: Arc<RwLock<HashMap<String, bool>>>,
    /// Channel for receiving permission requests from agents
    permission_request_tx: mpsc::UnboundedSender<PermissionRequest>,
}

/// Permission request from an agent that needs user approval
pub struct PermissionRequest {
    pub agent_name: String,
    pub session_id: SessionId,
    pub options: Vec<PermissionOption>,
    pub response_tx: oneshot::Sender<Option<String>>,
}

/// Handle for communicating with a spawned agent.
struct AgentHandle {
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
    /// Set a session configuration option
    SetConfigOption {
        req: SetSessionConfigOptionRequest,
        resp_tx: oneshot::Sender<agent_client_protocol::Result<SetSessionConfigOptionResponse>>,
    },
    /// Set session mode (deprecated)
    SetMode {
        req: SetSessionModeRequest,
        resp_tx: oneshot::Sender<agent_client_protocol::Result<SetSessionModeResponse>>,
    },
}

impl AgentManager {
    /// Creates a new agent manager with the given notification channel.
    pub fn new(
        notification_tx: mpsc::UnboundedSender<(String, SessionNotification)>,
        permission_request_tx: mpsc::UnboundedSender<PermissionRequest>,
    ) -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            notification_tx,
            session_permissions: Arc::new(RwLock::new(HashMap::new())),
            permission_request_tx,
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
        debug!("Agent command: {} {:?}", config.command, config.args);
        trace!("Agent env: {:?}", config.env);

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
        let (init_tx, init_rx) = tokio::sync::oneshot::channel();
        let agent_name = config.name.clone();
        let agent_name2 = agent_name.clone();
        let notification_tx = self.notification_tx.clone();
        let session_permissions = self.session_permissions.clone();
        let permission_request_tx = self.permission_request_tx.clone();

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
                    session_permissions,
                    permission_request_tx,
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
                    .client_info(Implementation::new("juan", "0.1.1"));

                match connection.initialize(init_req).await {
                    Ok(_) => {
                        info!("Agent {} initialized successfully", agent_name);
                        let _ = init_tx.send(Ok(()));
                    }
                    Err(e) => {
                        error!("Failed to initialize agent {}: {}", agent_name, e);
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                }

                // Handle commands from the main loop
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        AgentCommand::NewSession { req, resp_tx } => {
                            debug!("Agent {} processing NewSession request", agent_name);
                            let result = connection.new_session(req).await;
                            trace!("NewSession result: {:?}", result);
                            let _ = resp_tx.send(result);
                        }
                        AgentCommand::Prompt { req, resp_tx } => {
                            debug!(
                                "Agent {} processing Prompt request for session {}",
                                agent_name, req.session_id
                            );
                            let result = connection.prompt(req).await;
                            trace!("Prompt result: {:?}", result);
                            let _ = resp_tx.send(result);
                        }
                        AgentCommand::SetConfigOption { req, resp_tx } => {
                            debug!(
                                "Agent {} processing SetConfigOption request for session {}",
                                agent_name, req.session_id
                            );
                            let result = connection.set_session_config_option(req).await;
                            trace!("SetConfigOption result: {:?}", result);
                            let _ = resp_tx.send(result);
                        }
                        AgentCommand::SetMode { req, resp_tx } => {
                            debug!(
                                "Agent {} processing SetMode request for session {}",
                                agent_name, req.session_id
                            );
                            let result = connection.set_session_mode(req).await;
                            trace!("SetMode result: {:?}", result);
                            let _ = resp_tx.send(result);
                        }
                    }
                }
            }));
        });

        // Wait for initialization to complete
        init_rx
            .await
            .context("Agent initialization channel closed")??;

        let handle = AgentHandle { tx: cmd_tx };
        self.agents.write().await.insert(agent_name2, handle);

        Ok(())
    }

    /// Creates a new ACP session with the specified agent.
    pub async fn new_session(
        &self,
        agent_name: &str,
        req: NewSessionRequest,
        auto_approve: bool,
    ) -> Result<NewSessionResponse> {
        debug!("Creating new session with agent: {}", agent_name);
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

        let response = resp_rx
            .await
            .context("Agent command channel closed")?
            .map_err(|e| anyhow::anyhow!("Agent error: {}", e))?;

        // Store auto_approve setting for this session
        self.session_permissions
            .write()
            .await
            .insert(response.session_id.to_string(), auto_approve);

        Ok(response)
    }

    /// Sends a prompt to an agent's existing session.
    pub async fn prompt(&self, agent_name: &str, req: PromptRequest) -> Result<PromptResponse> {
        debug!(
            "Sending prompt to agent: {}, session: {}",
            agent_name, req.session_id
        );
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

    /// Sets a session configuration option.
    pub async fn set_config_option(
        &self,
        agent_name: &str,
        req: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse> {
        debug!(
            "Setting config option for agent: {}, session: {}, config_id: {}",
            agent_name, req.session_id, req.config_id
        );
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
            .send(AgentCommand::SetConfigOption { req, resp_tx })
            .context("Failed to send command to agent")?;

        resp_rx
            .await
            .context("Agent command channel closed")?
            .map_err(|e| anyhow::anyhow!("Agent error: {}", e))
    }

    /// Sets session mode (deprecated API).
    pub async fn set_mode(
        &self,
        agent_name: &str,
        req: SetSessionModeRequest,
    ) -> Result<SetSessionModeResponse> {
        debug!(
            "Setting mode for agent: {}, session: {}, mode_id: {}",
            agent_name, req.session_id, req.mode_id
        );
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
            .send(AgentCommand::SetMode { req, resp_tx })
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
    session_permissions: Arc<RwLock<HashMap<String, bool>>>,
    permission_request_tx: mpsc::UnboundedSender<PermissionRequest>,
}

#[async_trait::async_trait(?Send)]
impl Client for NotificationClient {
    /// Handles permission requests from agents.
    /// Checks the session's auto_approve setting and either approves automatically
    /// or requests user approval via Slack.
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        debug!(
            "Agent {} requesting permission for session {}: {:?}",
            self.agent_name, args.session_id, args.options
        );

        let auto_approve = self
            .session_permissions
            .read()
            .await
            .get(&args.session_id.to_string())
            .copied()
            .unwrap_or(false);

        if auto_approve {
            let first_option = args
                .options
                .first()
                .ok_or_else(|| Error::invalid_params())?;

            debug!(
                "Auto-approving permission for session {}: {}",
                args.session_id, first_option.option_id
            );

            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    first_option.option_id.clone(),
                )),
            ))
        } else {
            // Request user approval via Slack
            info!(
                "Requesting user approval for session {} (auto_approve=false)",
                args.session_id
            );

            let (response_tx, response_rx) = oneshot::channel();
            let permission_req = PermissionRequest {
                agent_name: self.agent_name.clone(),
                session_id: args.session_id.clone(),
                options: args.options.clone(),
                response_tx,
            };

            if let Err(e) = self.permission_request_tx.send(permission_req) {
                error!("Failed to send permission request: {}", e);
                return Ok(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ));
            }

            // Wait for user response
            match response_rx.await {
                Ok(Some(option_id)) => {
                    debug!("User approved permission: {}", option_id);
                    Ok(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            option_id,
                        )),
                    ))
                }
                Ok(None) | Err(_) => {
                    debug!("User denied or cancelled permission request");
                    Ok(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ))
                }
            }
        }
    }

    /// Handles session notifications from agents (e.g., message chunks).
    /// Forwards to main loop for processing.
    async fn session_notification(
        &self,
        args: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        trace!(
            "Agent {} notification: session={}, update={:?}",
            self.agent_name, args.session_id, args.update
        );
        if let Err(e) = self.notification_tx.send((self.agent_name.clone(), args)) {
            error!(
                "Failed to send notification from agent {}: {}",
                self.agent_name, e
            );
        }
        Ok(())
    }
}
