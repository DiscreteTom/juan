# Juan - Chat-to-ACP Bridge

[![GitHub Release](https://img.shields.io/github/v/release/DiscreteTom/juan)](https://github.com/DiscreteTom/juan/releases)
[![License](https://img.shields.io/github/license/DiscreteTom/juan)](https://github.com/DiscreteTom/juan/blob/main/LICENSE)

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
juan init

# Run with default config file (juan.toml)
juan run
```

## Interacting with Agents

In your Slack, talk to the Slack APP. Use `#help` to see help.

## [CHANGELOG](./CHANGELOG.md)
