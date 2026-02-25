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
    /// Map of session_id to agent handle (one agent process per session)
    agents: Arc<RwLock<HashMap<SessionId, AgentHandle>>>,
    /// Channel for receiving notifications from agents
    notification_tx: mpsc::UnboundedSender<crate::bridge::NotificationWrapper>,
    /// Map of session_id to auto_approve setting
    session_permissions: Arc<RwLock<HashMap<SessionId, bool>>>,
    /// Channel for receiving permission requests from agents
    permission_request_tx: mpsc::UnboundedSender<PermissionRequest>,
    /// Agent configurations
    agent_configs: Arc<RwLock<HashMap<String, AgentConfig>>>,
}

/// Permission request from an agent that needs user approval
pub struct PermissionRequest {
    pub session_id: SessionId,
    pub options: Vec<PermissionOption>,
    pub response_tx: oneshot::Sender<Option<String>>,
}

/// Handle for communicating with a spawned agent.
struct AgentHandle {
    /// Channel for sending commands to the agent's task
    tx: mpsc::UnboundedSender<AgentCommand>,
    /// Channel for sending cancel signal
    cancel_tx: mpsc::UnboundedSender<()>,
    /// Child process handle
    process: Arc<tokio::sync::Mutex<tokio::process::Child>>,
}

/// Commands that can be sent to an agent task.
enum AgentCommand {
    /// Send a prompt to the session
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
    /// Set session model (deprecated)
    SetModel {
        req: SetSessionModelRequest,
        resp_tx: oneshot::Sender<agent_client_protocol::Result<SetSessionModelResponse>>,
    },
}

impl AgentManager {
    /// Creates a new agent manager with the given notification channel.
    pub fn new(
        notification_tx: mpsc::UnboundedSender<crate::bridge::NotificationWrapper>,
        permission_request_tx: mpsc::UnboundedSender<PermissionRequest>,
    ) -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            notification_tx,
            session_permissions: Arc::new(RwLock::new(HashMap::new())),
            permission_request_tx,
            agent_configs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Registers agent configurations for later use.
    pub async fn register_agents(&self, configs: Vec<AgentConfig>) {
        let mut agent_configs = self.agent_configs.write().await;
        for config in configs {
            agent_configs.insert(config.name.clone(), config);
        }
    }

    /// Spawns a single agent process and sets up ACP communication.
    ///
    /// Creates a new ACP session by spawning an agent process.
    /// Each session gets its own dedicated agent process.
    pub async fn new_session(
        &self,
        agent_name: &str,
        req: NewSessionRequest,
        auto_approve: bool,
    ) -> Result<NewSessionResponse> {
        info!("Creating new session with agent: {}", agent_name);

        // Get agent config
        let config = self
            .agent_configs
            .read()
            .await
            .get(agent_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Agent config not found: {}", agent_name))?;

        // Spawn agent process
        debug!(
            "Spawning agent process: {} {:?}",
            config.command, config.args
        );
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .envs(&config.env);

        let mut process = cmd
            .spawn()
            .context(format!("Failed to spawn agent: {}", agent_name))?;

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

        let process_handle = Arc::new(tokio::sync::Mutex::new(process));

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (cancel_tx, mut cancel_rx) = mpsc::unbounded_channel();
        let (init_tx, init_rx) = oneshot::channel();
        let (session_tx, session_rx) = oneshot::channel();

        let notification_tx = self.notification_tx.clone();
        let session_permissions = self.session_permissions.clone();
        let permission_request_tx = self.permission_request_tx.clone();

        // Spawn dedicated thread for ACP communication
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let local = tokio::task::LocalSet::new();
            rt.block_on(local.run_until(async move {
                let client = NotificationClient {
                    notification_tx,
                    session_permissions,
                    permission_request_tx,
                };
                let (connection, io_task) =
                    ClientSideConnection::new(client, stdin, stdout, |fut| {
                        tokio::task::spawn_local(fut);
                    });

                tokio::task::spawn_local(async move {
                    if let Err(e) = io_task.await {
                        error!("Agent IO task error: {}", e);
                    }
                });

                // Initialize agent
                let init_req = InitializeRequest::new(ProtocolVersion::LATEST)
                    .client_info(Implementation::new("juan", env!("CARGO_PKG_VERSION")));

                match connection.initialize(init_req).await {
                    Ok(_) => {
                        info!("Agent initialized successfully");
                        let _ = init_tx.send(Ok(()));
                    }
                    Err(e) => {
                        error!("Failed to initialize agent: {}", e);
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                }

                // Create session
                let session_result = connection.new_session(req).await;
                let session_id = match &session_result {
                    Ok(resp) => resp.session_id.clone(),
                    Err(e) => {
                        error!("Failed to create session: {}", e);
                        let _ = session_tx.send(session_result);
                        return;
                    }
                };
                let _ = session_tx.send(session_result);

                // Handle commands
                while let Some(cmd) = cmd_rx.recv().await {
                    match cmd {
                        AgentCommand::Prompt { req, resp_tx } => {
                            debug!("Processing Prompt request for session {}", req.session_id);
                            let prompt_fut = connection.prompt(req);
                            tokio::pin!(prompt_fut);

                            loop {
                                tokio::select! {
                                    result = &mut prompt_fut => {
                                        trace!("Prompt result: {:?}", result);
                                        let _ = resp_tx.send(result);
                                        break;
                                    }
                                    Some(_) = cancel_rx.recv() => {
                                        debug!("Cancelling prompt for session {}", session_id);
                                        let notification = CancelNotification::new(session_id.clone());
                                        if let Err(e) = connection.cancel(notification).await {
                                            error!("Failed to send cancel notification: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                        AgentCommand::SetConfigOption { req, resp_tx } => {
                            debug!("Processing SetConfigOption for session {}", req.session_id);
                            let result = connection.set_session_config_option(req).await;
                            let _ = resp_tx.send(result);
                        }
                        AgentCommand::SetMode { req, resp_tx } => {
                            debug!("Processing SetMode for session {}", req.session_id);
                            let result = connection.set_session_mode(req).await;
                            let _ = resp_tx.send(result);
                        }
                        AgentCommand::SetModel { req, resp_tx } => {
                            debug!("Processing SetModel for session {}", req.session_id);
                            let result = connection.set_session_model(req).await;
                            let _ = resp_tx.send(result);
                        }
                    }
                }
            }));
        });

        // Wait for initialization
        init_rx
            .await
            .context("Agent initialization channel closed")??;

        // Wait for session creation
        let response = session_rx
            .await
            .context("Session creation channel closed")?
            .map_err(|e| anyhow::anyhow!("Failed to create session: {}", e))?;

        // Store handle and permissions
        let handle = AgentHandle {
            tx: cmd_tx,
            cancel_tx,
            process: process_handle,
        };
        self.agents
            .write()
            .await
            .insert(response.session_id.clone(), handle);
        self.session_permissions
            .write()
            .await
            .insert(response.session_id.clone(), auto_approve);

        Ok(response)
    }

    /// Sends a prompt to a session.
    pub async fn prompt(
        &self,
        session_id: &SessionId,
        req: PromptRequest,
    ) -> Result<PromptResponse> {
        debug!("Sending prompt to session: {}", session_id);
        let handle = self
            .agents
            .read()
            .await
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?
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
        session_id: &SessionId,
        req: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse> {
        debug!("Setting config option for session: {}", session_id);
        let handle = self
            .agents
            .read()
            .await
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?
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
        session_id: &SessionId,
        req: SetSessionModeRequest,
    ) -> Result<SetSessionModeResponse> {
        debug!("Setting mode for session: {}", session_id);
        let handle = self
            .agents
            .read()
            .await
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?
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

    /// Sets session model (deprecated API).
    pub async fn set_model(
        &self,
        session_id: &SessionId,
        req: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse> {
        debug!("Setting model for session: {}", session_id);
        let handle = self
            .agents
            .read()
            .await
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?
            .tx
            .clone();

        let (resp_tx, resp_rx) = oneshot::channel();
        handle
            .send(AgentCommand::SetModel { req, resp_tx })
            .context("Failed to send command to agent")?;

        resp_rx
            .await
            .context("Agent command channel closed")?
            .map_err(|e| anyhow::anyhow!("Agent error: {}", e))
    }

    /// Cancels an ongoing session operation.
    pub async fn cancel(&self, session_id: &SessionId) -> Result<()> {
        debug!("Cancelling session: {}", session_id);
        let cancel_tx = self
            .agents
            .read()
            .await
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?
            .cancel_tx
            .clone();

        cancel_tx
            .send(())
            .context("Failed to send cancel signal to agent")?;

        Ok(())
    }

    /// Ends a session and kills the agent process.
    pub async fn end_session(&self, session_id: &SessionId) -> Result<()> {
        info!("Ending session: {}", session_id);

        // Get handle and kill process
        if let Some(handle) = self.agents.write().await.remove(session_id) {
            let mut process = handle.process.lock().await;
            if let Err(e) = process.kill().await {
                error!("Failed to kill agent process: {}", e);
            }
        }

        self.session_permissions.write().await.remove(session_id);
        Ok(())
    }
}

/// ACP client implementation for handling agent notifications and permission requests.
struct NotificationClient {
    notification_tx: mpsc::UnboundedSender<crate::bridge::NotificationWrapper>,
    session_permissions: Arc<RwLock<HashMap<SessionId, bool>>>,
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
            "Permission request for session {}: {:?}",
            args.session_id, args.options
        );

        let auto_approve = self
            .session_permissions
            .read()
            .await
            .get(&args.session_id)
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
            "Session notification: session={}, update={:?}",
            args.session_id, args.update
        );
        if let Err(e) = self
            .notification_tx
            .send(crate::bridge::NotificationWrapper::Agent(args))
        {
            error!("Failed to send notification: {}", e);
        }
        Ok(())
    }
}
