# Changelog

Changes are recorded here before releases.

## Unreleased

## 0.1.0 - 2026-10-06

- Preserve nested-repository protection when a worktree's `.git` marker is a file.
- Recursively discover nested, hidden, and ignored Git repositories and linked worktrees.
- Inspect branches, commits, activity, local changes, upstream state, locks, and stale registrations.
- Manage worktrees through a keyboard and mouse dashboard or explicit CLI commands.
- Review creation, moves, locking, unlocking, removal, pruning, and fetch operations.
- Protect primary checkouts, nested repositories, dirty files, and unknown status during cleanup.
- Stream completed repositories during discovery and cache immutable metadata for faster refresh.
- Integrate directory selection and completions with zsh, Bash, and fish.
- Provide checksummed macOS and Linux release archives and a source-install fallback.
