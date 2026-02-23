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
        cmd_process.arg("/C").arg(cmd);
        cmd_process
    } else {
        let mut sh_process = Command::new("sh");
        sh_process.arg("-c").arg(cmd);
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

            if out.status.success() {
                if stdout.is_empty() && stderr.is_empty() {
                    "Command executed successfully (no output)".to_string()
                } else if stderr.is_empty() {
                    let ticks = crate::utils::safe_backticks(&stdout);
                    format!("{}\n{}\n{}", ticks, stdout, ticks)
                } else {
                    let stdout_ticks = crate::utils::safe_backticks(&stdout);
                    let stderr_ticks = crate::utils::safe_backticks(&stderr);
                    format!(
                        "{}\n{}\n{}\n\nStderr:\n{}\n{}\n{}",
                        stdout_ticks, stdout, stdout_ticks, stderr_ticks, stderr, stderr_ticks
                    )
                }
            } else {
                if stderr.is_empty() {
                    let ticks = crate::utils::safe_backticks(&stdout);
                    format!(
                        "Exit code: {}\n{}\n{}\n{}",
                        out.status.code().unwrap_or(-1),
                        ticks,
                        stdout,
                        ticks
                    )
                } else {
                    let stdout_ticks = crate::utils::safe_backticks(&stdout);
                    let stderr_ticks = crate::utils::safe_backticks(&stderr);
                    format!(
                        "Exit code: {}\n{}\n{}\n{}\n\nStderr:\n{}\n{}\n{}",
                        out.status.code().unwrap_or(-1),
                        stdout_ticks,
                        stdout,
                        stdout_ticks,
                        stderr_ticks,
                        stderr,
                        stderr_ticks
                    )
                }
            }
        }
        Err(e) => format!("Failed to execute command: {}", e),
    };

    let _ = slack.send_message(channel, thread_ts, &response).await;
}
