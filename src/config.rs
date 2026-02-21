use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use toml_scaffold::TomlScaffold;

/// Config for [anywhere](https://github.com/DiscreteTom/anywhere)
#[derive(Debug, Deserialize, Serialize, Clone, JsonSchema, TomlScaffold)]
pub struct Config {
    /// Slack workspace connection settings
    pub slack: SlackConfig,
    /// Bridge behavior settings
    pub bridge: BridgeConfig,
    /// List of configured agents
    pub agents: Vec<AgentConfig>,
}

/// Slack connection configuration
#[derive(Debug, Deserialize, Serialize, Clone, JsonSchema, TomlScaffold)]
pub struct SlackConfig {
    /// Bot User OAuth Token (starts with xoxb-)
    pub bot_token: String,
    /// App-level token for Socket Mode (starts with xapp-)
    pub app_token: String,
}

/// Bridge behavior configuration
#[derive(Debug, Deserialize, Serialize, Clone, JsonSchema, TomlScaffold)]
pub struct BridgeConfig {
    /// Default workspace path for agents
    pub default_workspace: String,
    /// Global auto-approve setting for tool calls
    pub auto_approve: bool,
}

/// Agent configuration
#[derive(Debug, Deserialize, Serialize, Clone, JsonSchema, TomlScaffold)]
pub struct AgentConfig {
    /// Unique identifier for the agent
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Path to the agent executable
    pub command: String,
    /// Command-line arguments for the agent
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the agent
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Override global auto-approve for this agent
    #[serde(default)]
    pub auto_approve: bool,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .context(format!("Failed to read config file: {}", path))?;
        let config: Config = toml::from_str(&content).context("Failed to parse config file")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.agents.is_empty(),
            "At least one agent must be configured"
        );

        for agent in &self.agents {
            anyhow::ensure!(!agent.name.is_empty(), "Agent name cannot be empty");
            anyhow::ensure!(!agent.command.is_empty(), "Agent command cannot be empty");
        }

        Ok(())
    }

    pub fn expand_path(&self, path: &str) -> PathBuf {
        if path.starts_with('~') {
            if let Some(home) = dirs::home_dir() {
                return home.join(path.strip_prefix("~/").unwrap_or(&path[1..]));
            }
        }
        PathBuf::from(path)
    }
}
