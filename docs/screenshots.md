# README screenshots

The images in [images/](images/) capture real `wtm` sessions through the PTY harness.
`pyte` interprets the application's terminal output; Pillow renders the resulting cell
grid as PNGs. These are terminal-screen captures, not native Terminal.app screenshots
or drawn UI mockups. The renderer supplies a dark terminal palette and monospace font.

The fixture creates three disposable Git repositories, including a nested repository,
and seven worktrees with clean, dirty, locked, and primary states. All remote URLs use
`example` repositories. The cleanup capture opens a real confirmation and cancels it
without removing a worktree. Temporary repositories are removed when capture finishes.

From the repository root:

```sh
cargo build --release --locked
uv run scripts/screenshots.py target/release/wtm
```

The capture script has its own pinned Python dependencies and does not add runtime
dependencies to `wtm`. It uses Menlo on macOS or DejaVu Sans Mono on Linux. Supply
`--font /path/to/monospace.ttf` if neither font is available. `--output DIRECTORY`
changes the image destination.

Review both images before committing: labels and confirmation text must remain readable,
the scan must have finished, and no personal paths, credentials, or private data may appear.
