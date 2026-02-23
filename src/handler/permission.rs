use crate::slack;
use tracing::debug;

pub async fn handle_permission_response(
    text: &str,
    options: Vec<agent_client_protocol::PermissionOption>,
    response_tx: tokio::sync::oneshot::Sender<Option<String>>,
    slack: &slack::SlackConnection,
    channel: &str,
    thread_key: &str,
) {
    debug!("Handling permission response: text={}", text);
    let text = text.trim();

    if text.eq_ignore_ascii_case("deny") {
        let _ = slack
            .send_message(channel, Some(thread_key), "❌ Permission denied")
            .await;
        let _ = response_tx.send(None);
        return;
    }

    // Try to parse as a number
    if let Ok(choice) = text.parse::<usize>() {
        if choice > 0 && choice <= options.len() {
            let selected = &options[choice - 1];
            let _ = slack
                .send_message(
                    channel,
                    Some(thread_key),
                    &format!("✅ Approved: {}", selected.name),
                )
                .await;
            let _ = response_tx.send(Some(selected.option_id.to_string()));
            return;
        }
    }

    let _ = slack
        .send_message(
            channel,
            Some(thread_key),
            "❌ Invalid response. Permission denied.",
        )
        .await;
    let _ = response_tx.send(None);
}
