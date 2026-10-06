# Repository instructions

`wtm` is a Rust CLI and terminal UI for Git worktrees. Keep changes small, preserve
unrelated work, and do not publish releases, push tags, or change GitHub settings unless
the user asks. There is no requirement to create a worktree or delegate work.

## Structure

- `src/main.rs`: CLI, completions, and shell integration.
- `src/scan.rs`: full recursive discovery and session refresh.
- `src/git.rs`: native paths, status parsing, inspection caches, and guarded Git writes.
- `src/tui.rs`, `src/tui/interaction.rs`: dashboard, hit regions, keyboard/mouse input, terminal lifecycle.
- `src/model.rs`, `src/profile.rs`: shared data and per-scan measurements.
- `tests/`: real-Git regressions and installer/release tests.
- `install.sh`: release-binary installer; `scripts/install.sh`: source installer.
- `scripts/release.py`: tag checks, notices, deterministic archives, checksums, release notes.

## Checks

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
python3 -m unittest discover -s tests -p 'test_*.py'
shellcheck install.sh scripts/install.sh
```

For terminal changes, also build release mode and run the PTY test using
[CONTRIBUTING.md](CONTRIBUTING.md). Check Rust 1.88 compatibility when changing dependencies
or Rust syntax. Review dependency licenses and notices before changing release contents.

## Invariants

- Default discovery includes nested, hidden, and ignored directories. Exclusions are explicit.
- Discovery failures remain visible; unknown or pending status must not be treated as clean.
- Never fetch during scans. Revalidate identity, HEAD, registry, and protection state before writes.
- Preserve primary and bare repositories, locks, nested repositories, and dirty worktrees.
- Keep branches during removal unless deletion is explicitly requested and Git accepts it.
- Terminal UI and control sequences go to stderr; selected paths alone go to stdout.
- Restore raw mode, alternate screen, mouse capture, and paste mode on every exit path.
- Mouse actions use the same guards as keyboard actions. Paste never submits or confirms.
- Pass native arguments to subprocesses; do not evaluate shell text built from repository input.
- Do not print or retain tokens, credential-bearing URLs, or private repository content.
- Never replace an installed binary after a failed checksum or version check.

Keep public documentation generic. The GitHub destination is `tiaanduplessis/wtm`.
The project is MIT licensed and distributes through GitHub releases, not a package registry.
