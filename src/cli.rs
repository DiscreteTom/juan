use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "anywhere")]
#[command(about = "Chat-to-ACP Bridge", long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(long, default_value = "anywhere.toml")]
    pub config: String,

    #[arg(long, default_value = "info")]
    pub log_level: String,
}

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
