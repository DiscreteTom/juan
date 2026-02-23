# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1] - 2026-02-23

### Added
- Slack plan block rendering for ACP plan updates with full and incremental display modes
- Fallback plan block updates derived from thoughts and tool-call lifecycle updates
- Test helpers for Slack plan blocks and streaming APIs:
  - `scripts/test-slack-plan-block.ps1`
  - `scripts/test-slack-plan-stream.ps1`
  - `src/bin/test_slack_plan_block.rs`

### Changed
- Normalize markdown formatting before sending/updating Slack messages
- Add raw Slack Web API message post/update paths to support unsupported block types
- Add `bridge.plan_display_mode` config default/validation and reset per-prompt plan UI state

## [0.1.0] - 2026-02-21

### Added
- Slack integration via Socket Mode (no public endpoint required)
- Multi-agent support with configuration and switching
- Thread-based session management with persistent context
- Workspace context for local filesystem operations
- Per-agent tool call auto-approval configuration
- Commands: `#new`, `#agents`, `#session`, `#end`, `#read`, `#diff`
- Shell command execution with `!` prefix

[Unreleased]: https://github.com/DiscreteTom/juan/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/DiscreteTom/juan/releases/tag/v0.1.1
[0.1.0]: https://github.com/DiscreteTom/juan/releases/tag/v0.1.0
