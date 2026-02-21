use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "anywhere")]
#[command(about = "Chat-to-ACP Bridge", long_about = None)]
pub struct Args {
    #[arg(long, default_value = "anywhere.toml")]
    pub config: String,

    #[arg(long, default_value = "info")]
    pub log_level: String,
}
