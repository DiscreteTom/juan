use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(name = "test-slack-plan-block")]
#[command(about = "Send a Slack plan block test message")]
struct Args {
    /// Slack channel ID (e.g., C123...)
    #[arg(long)]
    channel: String,

    /// Optional thread timestamp
    #[arg(long)]
    thread_ts: Option<String>,

    /// Optional config path (defaults to ./juan.toml or ./joan.toml)
    #[arg(long)]
    config_path: Option<String>,
}

fn resolve_config_path(candidate: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = candidate {
        let candidate_path = PathBuf::from(path);
        if candidate_path.exists() {
            return candidate_path
                .canonicalize()
                .context("Failed to canonicalize provided config path");
        }
    }

    for default in ["juan.toml", "joan.toml"] {
        let p = PathBuf::from(default);
        if p.exists() {
            return p
                .canonicalize()
                .context("Failed to canonicalize default config path");
        }
    }

    bail!("Could not find config file. Pass --config-path, or place juan.toml/joan.toml in current directory.")
}

fn get_bot_token_from_toml(config_path: &Path) -> Result<Option<String>> {
    let content = std::fs::read_to_string(config_path).with_context(|| {
        format!(
            "Failed to read config file at {}",
            config_path.to_string_lossy()
        )
    })?;
    let parsed: toml::Value = toml::from_str(&content).context("Failed to parse TOML config")?;

    let token = parsed
        .get("slack")
        .and_then(|v| v.get("bot_token"))
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned);

    Ok(token)
}

async fn invoke_slack_api(token: &str, method: &str, body: &Value) -> Result<Value> {
    let uri = format!("https://slack.com/api/{method}");
    let client = reqwest::Client::new();

    let resp = client
        .post(uri)
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/json; charset=utf-8")
        .json(body)
        .send()
        .await
        .with_context(|| format!("Failed to call Slack API method {method}"))?;

    let status = resp.status();
    let parsed: Value = resp
        .json()
        .await
        .with_context(|| format!("Failed to parse Slack API response for {method}"))?;

    if !status.is_success() {
        bail!("Slack API {method} returned HTTP {status}: {parsed}");
    }

    if parsed.get("ok").and_then(Value::as_bool) != Some(true) {
        let err = parsed
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");

        let details = parsed
            .get("response_metadata")
            .and_then(|v| v.get("messages"))
            .and_then(Value::as_array)
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .filter(|s| !s.is_empty());

        if let Some(details) = details {
            bail!("Slack API {method} failed: {err} | {details}");
        }
        bail!("Slack API {method} failed: {err}");
    }

    Ok(parsed)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let config = resolve_config_path(args.config_path.as_deref())?;
    let token = get_bot_token_from_toml(&config)?.or_else(|| std::env::var("SLACK_BOT_TOKEN").ok());
    let token = token.with_context(|| {
        format!(
            "No bot token found in [slack].bot_token in {} and SLACK_BOT_TOKEN is empty.",
            config.to_string_lossy()
        )
    })?;

    println!("Using config: {}", config.to_string_lossy());
    println!("Channel: {}", args.channel);
    if let Some(thread_ts) = &args.thread_ts {
        println!("Thread TS: {thread_ts}");
    }

    let plan_block = json!({
        "type": "plan",
        "title": "Thinking completed",
        "tasks": [
            {
                "task_id": "call_001",
                "title": "Fetched user profile information",
                "status": "complete"
            },
            {
                "task_id": "call_002",
                "title": "Checked user permissions",
                "status": "complete"
            },
            {
                "task_id": "call_003",
                "title": "Generated comprehensive user report",
                "status": "complete"
            }
        ]
    });

    let mut body = json!({
        "channel": args.channel,
        "text": "Plan block test message",
        "blocks": [plan_block]
    });

    if let Some(thread_ts) = args.thread_ts {
        body.as_object_mut()
            .context("Failed to construct Slack request body")?
            .insert("thread_ts".to_string(), Value::String(thread_ts));
    }

    let resp = invoke_slack_api(&token, "chat.postMessage", &body).await?;
    println!("chat.postMessage ok");
    if let Some(ts) = resp.get("ts").and_then(Value::as_str) {
        println!("ts: {ts}");
    }

    Ok(())
}
