# Contributing

Bug reports and focused pull requests are welcome. This project is in early development;
keep changes small enough to review and describe the user-visible result.

## Setup

Install Git 2.36 or newer, Rust 1.88 or newer, and Python 3.11 or newer for release tests.
Clone the repository and work from its root. No worktree creation or credentials are
needed to build and test the project.

The CI workflow checks stable Rust on Linux and macOS, plus Rust 1.88 with Git 2.36.0 on Linux.
Git 2.36 is required for NUL-delimited worktree listings, which preserve paths with newlines.

```sh
cargo build --locked
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
python3 -m unittest discover -s tests -p 'test_*.py'
shellcheck install.sh scripts/install.sh
```

Check the real terminal UI after input, layout, or terminal-lifecycle changes:

```sh
cargo build --release --locked
python3 -m venv .venv
.venv/bin/pip install -r scripts/requirements.txt
.venv/bin/python scripts/tui_smoke.py target/release/wtm
```

The smoke test uses temporary real Git repositories. It checks mouse and keyboard
input, stdout path selection, cleanup confirmations, resizing, paste, and restoration
of terminal state. It does not mutate your own repositories.

## Changes and reports

- Follow the existing Rust style; format with rustfmt and keep Clippy clean.
- Add a regression test for changed discovery, Git status parsing, or management behavior.
- Keep paths native, invoke Git with argument arrays, and preserve repository identity checks.
- Do not turn an inspection error or cached status into a claim that a worktree is clean.
- Update help, README examples, and the Unreleased changelog when commands change.
- Include the OS, architecture, Git version, Rust version, and a minimal reproduction in bug reports.
- Remove private paths, tokens, remote credentials, and proprietary repository content from reports.

Use GitHub issues for public bugs and feature discussions. Follow [SECURITY.md](SECURITY.md)
for suspected vulnerabilities. Maintainers make release and scope decisions; there is no
promised response time. Contributions are accepted under the existing [MIT license](LICENSE).

Release preparation is documented in [docs/releasing.md](docs/releasing.md).
