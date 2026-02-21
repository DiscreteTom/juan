use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use toml_scaffold::TomlScaffold;
use tracing::{debug, trace};

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
        debug!("Loading config from: {}", path);
        let content = std::fs::read_to_string(path)
            .context(format!("Failed to read config file: {}", path))?;
        let config: Config = toml::from_str(&content).context("Failed to parse config file")?;
        config.validate()?;
        trace!(
            "Config loaded successfully: {} agents configured",
            config.agents.len()
        );
        Ok(config)
    }

    pub fn init(output: &str, override_existing: bool) -> Result<()> {
        use std::collections::HashMap;
        use toml_scaffold::TomlScaffold;

        debug!(
            "Initializing config file: {}, override={}",
            output, override_existing
        );
        if !override_existing && std::path::Path::new(output).exists() {
            anyhow::bail!(
                "File already exists: {}. Use --override to overwrite.",
                output
            );
        }

        let config = Config {
            slack: SlackConfig {
                bot_token: "xoxb-your-bot-token".into(),
                app_token: "xapp-your-app-token".into(),
            },
            bridge: BridgeConfig {
                default_workspace: "~".into(),
                auto_approve: false,
            },
            agents: vec![
                AgentConfig {
                    name: "kiro".into(),
                    description: "Kiro CLI - https://kiro.dev/cli/".into(),
                    command: "kiro-cli".into(),
                    args: vec!["acp".into()],
                    env: HashMap::new(),
                    auto_approve: false,
                },
                AgentConfig {
                    name: "opencode".into(),
                    description: "OpenCode - https://opencode.ai/".into(),
                    command: "opencode".into(),
                    args: vec!["acp".into()],
                    env: HashMap::new(),
                    auto_approve: false,
                },
            ],
        };

        let scaffold = config.to_scaffold()?;
        std::fs::write(output, scaffold)?;
        println!("Config scaffold written to: {}", output);
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        debug!("Validating config");
        anyhow::ensure!(
            !self.agents.is_empty(),
            "At least one agent must be configured"
        );

        for agent in &self.agents {
            anyhow::ensure!(!agent.name.is_empty(), "Agent name cannot be empty");
            anyhow::ensure!(!agent.command.is_empty(), "Agent command cannot be empty");
        }

        trace!("Config validation passed");
        Ok(())
    }
}
