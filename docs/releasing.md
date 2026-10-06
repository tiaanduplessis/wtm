# Releasing wtm

The GitHub destination is `tiaanduplessis/wtm`. Public visibility and releases are
maintainer decisions. The project uses GitHub releases and a source
fallback; it has no Homebrew formula, crates.io publication, or package-manager setup.

## First publication

1. Review the files and confirm the MIT license and copyright attribution.
2. Confirm repository visibility when ready to make the source public. Add its remote
   if needed, and commit/push the reviewed source to `main`.
   Keep credentials, `.env` files, local build output, and private repository content out of Git.
3. Set the About description to: “Recursively find, inspect, and manage Git worktrees
   with a keyboard and mouse terminal UI.” Suggested topics: `git`, `git-worktree`,
   `worktrees`, `rust`, `cli`, `tui`, `terminal`.
4. Enable GitHub Actions and confirm the first CI run passes. Review workflow permissions;
   pull requests have read-only permissions and publication is limited to the release job.
5. Enable private vulnerability reporting in Settings → Advanced Security. Verify that the
   Security tab's **Report a vulnerability** route is available. Files alone do not enable it.
6. Check the available secret-scanning and push-protection settings. Use branch/tag rules
   appropriate for a solo maintainer; require the stable Linux, macOS, and MSRV checks
   after their first successful runs. Restrict release-tag writes to maintainers.
7. Make the first draft release using the steps below. Test downloads from that draft
   before publishing it. Keep the README installation instructions aligned with the
   published assets.

GitHub settings, hosted CI, downloads, and security reporting need live verification
after repository creation. Local workflow files do not prove those features are enabled.

## Cut a release

Versions follow `major.minor.patch`; before 1.0, minor releases may change commands or
JSON fields. Record those changes and migration instructions in `CHANGELOG.md`. Tags
must exactly match `Cargo.toml`, for example `v0.1.0`. Prerelease tags are not handled
by the current installer or workflow.

Before tagging, move the Unreleased changes into a section named for the version and
include its release date. Keep an empty Unreleased section for future changes.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
python3 -m unittest discover -s tests -p 'test_*.py'
python3 scripts/release.py check --tag v0.1.0
```

Also run the release build and PTY smoke test from [CONTRIBUTING.md](../CONTRIBUTING.md).
Commit the version, changelog, and reviewed source; wait for hosted CI before pushing
the release tag. Creating and pushing the tag is a maintainer action.

The release workflow builds and tests four native targets:

| Platform | Target | GitHub runner |
| --- | --- | --- |
| macOS Apple Silicon | `aarch64-apple-darwin` | `macos-14` |
| macOS Intel | `x86_64-apple-darwin` | `macos-15-intel` |
| Linux Arm64 | `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` |
| Linux x86_64 | `x86_64-unknown-linux-musl` | `ubuntu-24.04` |

Linux binaries link musl for portability. Git and an interactive terminal remain runtime
requirements. macOS binaries are not Developer ID signed or notarized. Windows and
other architectures are not release targets; source compatibility there is unverified.

Each archive contains `wtm`, `LICENSE`, `README.md`, `CHANGELOG.md`, and
`THIRD_PARTY_NOTICES.txt`. Packaging checks the native binary's version and architecture.
The publisher requires all four archives, validates their contents, and generates
`SHA256SUMS` before creating or updating a draft release. Reruns refuse an already
published release. No persistent publishing token is needed beyond GitHub's job token.

Review the draft's release notes and assets. Download an archive on each supported
platform and verify the checksum and executable. Use the installer with a temporary
`--dir` and the draft's files in a local fixture, or publish only after the direct archive
checks pass; anonymous installer downloads cannot access a draft. Test the public
installer after publishing before announcing the release.
Also run the README's Cargo source fallback against the published tag in an isolated install root.

```sh
wtm --version
wtm list . --json
wtm .
```

Check keyboard and mouse navigation and normal terminal restoration. Wait for the release
workflow to finish before promoting the draft manually. Announcements, package registries, and repository
settings changes are separate maintainer decisions.

## Local packaging

Python 3.11 or newer and the selected Rust toolchain are required. Run packaging on the
binary's native platform; it executes the binary to check its version. Use a Rustup
toolchain so the compiler's standard-library notices are available. Keep that toolchain
selected for both building and packaging. Example for Apple Silicon:

```sh
rustup toolchain install 1.88.0 --profile minimal
export PATH="$HOME/.cargo/bin:$PATH"
export RUSTUP_TOOLCHAIN=1.88.0
rustup target add aarch64-apple-darwin
cargo build --release --locked --target aarch64-apple-darwin
python3 scripts/release.py package --tag v0.1.0 --target aarch64-apple-darwin \
  --binary target/aarch64-apple-darwin/release/wtm
```

`checksums` intentionally fails until all four platform archives are present. Do not
substitute another platform's binary, invent a checksum, or publish a partial release.

## Dependency updates

Dependabot proposes Cargo and GitHub Actions updates weekly. Review compatibility,
security advisories, and source notices; run the same checks before merging. Keep
Rust 1.88 support or explicitly document a new minimum. Workflow actions are pinned
to commit SHAs. Avoid automatic dependency merges and adding installation ecosystems
until users need them.

## Platform references

- [GitHub-hosted runner labels](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
- [GitHub Actions security guidance](https://docs.github.com/en/actions/security-for-github-actions/security-guides/security-hardening-for-github-actions)
- [Private vulnerability reporting setup](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/configuring-private-vulnerability-reporting-for-a-repository)
