use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    pub slack: SlackConfig,
    pub bridge: BridgeConfig,
    pub agents: Vec<AgentConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SlackConfig {
    pub bot_token: String,
    pub app_token: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BridgeConfig {
    pub default_workspace: String,
    #[serde(default)]
    pub auto_approve: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AgentConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub description: String,
    pub auto_approve: Option<bool>,
    #[serde(default)]
    pub env: HashMap<String, String>,
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
