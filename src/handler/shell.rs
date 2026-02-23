use crate::{session, slack};
use std::sync::Arc;
use tokio::process::Command;
use tracing::debug;

/// Handles shell commands (messages starting with !).
/// Executes the command locally and sends output back to Slack.
pub async fn handle_shell_command(
    text: &str,
    channel: &str,
    thread_ts: Option<&str>,
    slack: Arc<slack::SlackConnection>,
    session_manager: Arc<session::SessionManager>,
) {
    let cmd = text.trim().strip_prefix('!').unwrap_or("").trim();
    debug!("Executing shell command: {}", cmd);

    if cmd.is_empty() {
        let _ = slack
            .send_message(channel, thread_ts, "Usage: !<command>")
            .await;
        return;
    }

    // Get workspace from session if in a thread
    let workspace = if let Some(thread) = thread_ts {
        if let Some(session) = session_manager.get_session(thread).await {
            Some(crate::utils::expand_path(&session.workspace))
        } else {
            None
        }
    } else {
        None
    };

    // Execute command via shell
    let mut command = if cfg!(windows) {
        let mut cmd_process = Command::new("cmd");
        cmd_process.arg("/C").arg(&cmd);
        cmd_process
    } else {
        let mut sh_process = Command::new("sh");
        sh_process.arg("-c").arg(&cmd);
        sh_process
    };

    if let Some(dir) = workspace {
        command.current_dir(dir);
    }

    let output = command.output().await;

    // Format response with stdout/stderr
    let response = match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

            let mut parts = Vec::new();

            if !out.status.success() {
                parts.push(format!("Exit code: {}", out.status.code().unwrap_or(-1)));
            }

            if !stdout.is_empty() {
                let ticks = crate::utils::safe_backticks(&stdout);
                parts.push(format!("{}\n{}\n{}", ticks, stdout, ticks));
            }

            if !stderr.is_empty() {
                let ticks = crate::utils::safe_backticks(&stderr);
                parts.push(format!("Stderr:\n{}\n{}\n{}", ticks, stderr, ticks));
            }

            if parts.is_empty() {
                "Command executed successfully (no output)".to_string()
            } else {
                parts.join("\n\n")
            }
        }
        Err(e) => format!("Failed to execute command: {}", e),
    };

    let _ = slack.send_message(channel, thread_ts, &response).await;
}
