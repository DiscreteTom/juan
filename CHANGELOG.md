# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Slack plan block rendering for ACP plan updates
- Fallback plan block updates derived from thoughts and tool-call lifecycle updates
- Raw Slack Web API message post/update paths to support unsupported block types

### Changed

- Use Block Kit markdown blocks instead of plain text with mrkdwn formatting
- Removed markdown-to-mrkdwn conversion (standard markdown now works natively)

### Fixed

- Shell commands with URLs now work correctly (decode Slack's angle bracket URL formatting)

## [0.2.0] - 2026-02-23

### Added

- `#diff` command now accepts any git diff CLI parameters (e.g., `#diff --cached`, `#diff HEAD~1`)
- `#read` and `!<command>` now work outside agent threads using default workspace from config
- Text content from tool calls is now uploaded as plain text files for easier viewing
- Command `#mode` to show available modes and current mode
- Command `#mode <value>` to switch session mode via config options or deprecated modes API
- Command `#mode <value>!` to force set mode even when mode list is not available
- Command `#model` to show available models and current model
- Command `#model <value>` to switch session model via config options
- Support for ACP Session Config Options protocol (new standard API)
- Support for deprecated `session/set_mode` API for backward compatibility with agents using older ACP versions
- Agent config option `default_mode` to automatically set mode when creating new sessions (supports `!` suffix for force setting)
- Agent config option `default_model` to automatically set model when creating new sessions
- Session welcome message now shows configured default mode and model values
- Error reactions: `#new` command now adds `:x:` emoji reaction to user's message when session creation fails

### Changed

- Diff format simplified to show only changed lines with +/- prefix, no headers or context markers

### Fixed

- Messages sent to Slack now properly encode special characters (`&`, `<`, `>`) to prevent formatting issues
- Incoming messages from Slack now use minimal decoding (only `&amp;`, `&lt;`, `&gt;`) as per Slack's documentation
- Failed agent spawns are no longer marked as running, allowing retry with `#new` command
- `#new` command now validates workspace exists before creating agent session
- Shell command output now omits empty stdout/stderr blocks for cleaner display

## [0.1.1] - 2026-02-23

### Added

- Command `#sessions` to show all active sessions info
- White check mark emoji reaction on `#new` message when session ends
- Bot token scope `reactions:write` required for emoji reactions

## [0.1.0] - 2026-02-21

### Added

- Slack integration via Socket Mode (no public endpoint required)
- Multi-agent support with configuration and switching
- Thread-based session management with persistent context
- Workspace context for local filesystem operations
- Per-agent tool call auto-approval configuration
- Commands: `#new`, `#agents`, `#session`, `#end`, `#read`, `#diff`
- Shell command execution with `!` prefix

[Unreleased]: https://github.com/DiscreteTom/juan/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/DiscreteTom/juan/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/DiscreteTom/juan/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/DiscreteTom/juan/releases/tag/v0.1.0
