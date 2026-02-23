# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Command `#sessions` to show all active sessions info

## [0.1.0] - 2026-02-21

### Added
- Slack integration via Socket Mode (no public endpoint required)
- Multi-agent support with configuration and switching
- Thread-based session management with persistent context
- Workspace context for local filesystem operations
- Per-agent tool call auto-approval configuration
- Commands: `#new`, `#agents`, `#session`, `#end`, `#read`, `#diff`
- Shell command execution with `!` prefix

[Unreleased]: https://github.com/DiscreteTom/juan/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/DiscreteTom/juan/releases/tag/v0.1.0
