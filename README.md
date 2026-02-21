# Anywhere - Chat-to-ACP Bridge

A self-hosted bridge that allows you to interact with ACP-compatible coding agents through Slack. Run it on your PC to connect your Slack workspace with local or remote AI coding agents.

## Features

- **Slack Integration**: Connect via Socket Mode (no public endpoint required)
- **Multi-Agent Support**: Configure and switch between multiple agents
- **Session Management**: Thread-based sessions with persistent context
- **Workspace Context**: Agents work in your local filesystem
- **Auto-Approval**: Configure per-agent tool call approval settings

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
     - `files:write` - Upload files and share them
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

## Usage

```bash
# Create a config file
anywhere init

# Run with default config file (anywhere.toml)
anywhere run
```

## Interacting with Agents

### Commands

Commands start with `#` and can be used to control the bot:

- `#new <name> [workspace]` - Start session with specific agent (creates a new thread). Can only be used in main channel, not in threads.
- `#agents` - List all available agents with descriptions
- `#session` - Show current session info (agent, workspace, auto-approve setting). Must be used in an active agent thread.
- `#end` - End current session and cleanup. Must be used in an active agent thread.
- `#read <file_path>` - Read local file content and send to agent. Must be used in an active agent thread.
- `#diff [file_path]` - Show git diff (whole repo if no file specified). Must be used in an active agent thread.

### Shell Commands

Messages starting with `!` will execute shell commands:

- `!ls -la` - List files in current directory
- `!pwd` - Show current working directory
- `!git status` - Run any shell command

The output (stdout and stderr) will be sent back to Slack in a code block.
