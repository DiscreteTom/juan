# Anywhere - Chat-to-ACP Bridge

A self-hosted bridge that allows you to interact with ACP-compatible coding agents through Slack. Run it on your PC to connect your Slack workspace with local or remote AI coding agents.

## Features

- **Slack Integration**: Connect via Socket Mode (no public endpoint required)
- **Multi-Agent Support**: Configure and switch between multiple agents
- **Session Management**: Thread-based sessions with persistent context
- **Workspace Context**: Agents work in your local filesystem
- **Auto-Approval**: Configure per-agent tool call approval settings

## Prerequisites

- Rust 1.70 or later
- A Slack workspace where you can install apps
- One or more ACP-compatible agents installed locally

## Installation

```bash
# Clone the repository
git clone https://github.com/yourusername/anywhere.git
cd anywhere

# Build the project
cargo build --release

# The binary will be at target/release/anywhere
```

## Slack App Setup

1. Go to [https://api.slack.com/apps](https://api.slack.com/apps) and create a new app
2. Enable Socket Mode:
   - Go to "Socket Mode" in the sidebar
   - Enable Socket Mode
   - Generate an app-level token with `connections:write` scope (starts with `xapp-`)
3. Add Bot Token Scopes:
   - Go to "OAuth & Permissions"
   - Add these scopes:
     - `app_mentions:read` - Read messages that mention your app
     - `chat:write` - Send messages
     - `channels:history` - View messages in public channels
     - `groups:history` - View messages in private channels
     - `im:history` - View messages in direct messages
   - Install the app to your workspace
   - Copy the Bot User OAuth Token (starts with `xoxb-`)
4. Enable Events:
   - Go to "Event Subscriptions"
   - Subscribe to bot events:
     - `app_mention` - When your app is mentioned
     - `message.channels` - Messages in channels
     - `message.groups` - Messages in private channels
     - `message.im` - Direct messages
5. Configure App Home:
   - Go to "App Home"
   - Under "Show Tabs", check "Allow users to send Slash commands and messages from the messages tab"

## Configuration

Create a `anywhere.toml` file (see `anywhere.toml.example`):

```toml
[slack]
bot_token = "xoxb-your-bot-token-here"
app_token = "xapp-your-app-token-here"

[bridge]
default_workspace = "~/projects"
auto_approve = false

[[agents]]
name = "coder"
command = "/usr/local/bin/your-agent"
args = ["--mode", "acp"]
description = "General purpose coding agent"
auto_approve = true
```

### Configuration Options

- `slack.bot_token`: Your Slack bot token (required)
- `slack.app_token`: Your Slack app token for Socket Mode (required)
- `bridge.default_workspace`: Default workspace path for agents (default: `~`)
- `bridge.auto_approve`: Global auto-approve setting for tool calls (default: `false`)
- `agents`: Array of agent configurations
  - `name`: Unique identifier for the agent
  - `command`: Path to the agent executable
  - `args`: Command-line arguments for the agent
  - `description`: Human-readable description
  - `auto_approve`: Override global auto-approve for this agent (optional)
  - `env`: Environment variables for the agent (optional)

## Usage

```bash
# Run with default config file (anywhere.toml)
./anywhere

# Specify a custom config file
./anywhere --config /path/to/config.toml

# Set log level
./anywhere --log-level debug
```

### Log Levels

- `error`: Only errors
- `warn`: Warnings and errors
- `info`: General information (default)
- `debug`: Detailed debugging information
- `trace`: Very verbose tracing

## Interacting with Agents

### In Slack

1. **Start a conversation**: Mention your bot in a channel or send a direct message
2. **Ask questions**: Type your message and the agent will respond
3. **Thread-based sessions**: Each thread maintains its own session with the agent
4. **Use slash commands**: Control agent behavior and sessions

### Commands

Commands start with `#` and can be used to control the bot:

- `#agent <name> [workspace]` - Start session with specific agent (creates a new thread). Can only be used in main channel, not in threads.
- `#agents` - List all available agents with descriptions
- `#session` - Show current session info (agent, workspace, auto-approve setting). Must be used in a thread.
- `#end` - End current session and cleanup. Must be used in a thread.
- `#read <file_path>` - Read local file content and send to agent. Must be used in a thread.
- `#diff [file_path]` - Show git diff (whole repo if no file specified). Must be used in a thread.

### Example Conversation

```
You: @anywhere-bot Can you help me write a Python function to calculate fibonacci numbers?

Bot: Thinking...

Bot: Here's a Python function to calculate Fibonacci numbers:

[Agent response with code and explanation]
```

## Architecture

```
┌─────────────┐         ┌──────────────┐         ┌─────────────┐
│   Slack     │◄───────►│   Anywhere   │◄───────►│  ACP Agent  │
│  Workspace  │ Socket  │    Bridge    │  stdio  │  (Process)  │
└─────────────┘  Mode   └──────────────┘  JSON   └─────────────┘
                                          RPC
```

- **Slack Connection**: WebSocket via Socket Mode (no public endpoint)
- **Session Management**: Maps Slack threads to ACP sessions
- **Agent Communication**: JSON-RPC over stdio with each agent process
- **Multi-Agent**: Each agent runs in its own thread with dedicated runtime

## Deployment

### Running as a Service (Linux)

Create `/etc/systemd/system/anywhere.service`:

```ini
[Unit]
Description=Anywhere Chat-to-ACP Bridge
After=network.target

[Service]
Type=simple
User=youruser
WorkingDirectory=/home/youruser/anywhere
ExecStart=/home/youruser/anywhere/target/release/anywhere --config /home/youruser/anywhere/anywhere.toml
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

Enable and start:

```bash
sudo systemctl daemon-reload
sudo systemctl enable anywhere
sudo systemctl start anywhere
sudo systemctl status anywhere
```

### Running in Background

```bash
# Using nohup
nohup ./anywhere --config anywhere.toml > anywhere.log 2>&1 &

# Using screen
screen -S anywhere
./anywhere --config anywhere.toml
# Press Ctrl+A, then D to detach

# Using tmux
tmux new -s anywhere
./anywhere --config anywhere.toml
# Press Ctrl+B, then D to detach
```

## Troubleshooting

### Agent fails to start

- Check that the agent command path is correct
- Verify the agent supports ACP protocol
- Check agent logs for initialization errors
- Ensure the agent has necessary permissions

### Slack connection fails

- Verify bot token and app token are correct
- Check that Socket Mode is enabled
- Ensure the app is installed in your workspace
- Check network connectivity

### Sessions not persisting

- Sessions are stored in memory only
- Restarting the bridge will clear all sessions
- Each thread maintains its own session

### No response from agent

- Check agent logs for errors
- Verify the workspace path exists and is accessible
- Ensure the agent process is still running
- Check the log level for more details

## Development

```bash
# Run in development mode
cargo run -- --config anywhere.toml --log-level debug

# Run tests
cargo test

# Check code
cargo check

# Format code
cargo fmt

# Lint code
cargo clippy
```

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

[Your chosen license]

## Acknowledgments

- Built with the [Agent Client Protocol](https://agentclientprotocol.com/)
- Uses [slack-morphism](https://github.com/abdolence/slack-morphism-rust) for Slack integration
