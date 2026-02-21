/// CLI argument parsing using clap.
/// Defines the command-line interface for the anywhere bridge application.
use clap::{Parser, Subcommand};

/// Main CLI arguments structure.
/// Handles global options and subcommands for the application.
#[derive(Parser, Debug)]
#[command(name = "anywhere")]
#[command(about = "Chat-to-ACP Bridge", long_about = None)]
pub struct Args {
    /// Optional subcommand (e.g., init)
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Path to the configuration file
    #[arg(long, default_value = "anywhere.toml")]
    pub config: String,

    /// Logging level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    pub log_level: String,
}

/// Available subcommands for the CLI
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Generate a scaffold config file
    Init {
        /// Config file path
        #[arg(short, long, default_value = "anywhere.toml")]
        config: String,
        /// Override existing file
        #[arg(long)]
        r#override: bool,
    },
}
