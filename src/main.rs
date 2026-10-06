use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
use worktree_manager::{
    git::{self, AddOptions, RemoveOptions},
    model::{ScanOptions, Worktree},
    scan, tui,
};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Directory to scan recursively. With no command, opens the TUI.
    directory: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
    /// Leave mouse input to the terminal (for native text selection).
    #[arg(long, global = true)]
    no_mouse: bool,
    /// Maximum concurrent repository inspections.
    #[arg(long, global = true)]
    jobs: Option<usize>,
    /// Follow directory symlinks within the root (cycles are reported).
    #[arg(long, global = true)]
    follow_links: bool,
    /// Skip directories with this basename; repeat for multiple names.
    #[arg(long, global = true)]
    exclude: Vec<String>,
}
#[derive(Subcommand)]
enum Commands {
    /// Open the interactive worktree manager.
    Tui {
        #[arg(default_value = ".")]
        directory: PathBuf,
    },
    /// Recursively list worktrees, including repositories nested in checkouts.
    #[command(alias = "ls", alias = "status")]
    List {
        #[arg(default_value = ".")]
        directory: PathBuf,
        #[arg(long)]
        json: bool,
        #[arg(long, value_enum, default_value = "repo")]
        sort: Sort,
        /// Match repository, branch, or path (case insensitive).
        #[arg(long)]
        filter: Option<String>,
        /// Clickable terminal commit hashes (OSC 8; TTY only).
        #[arg(long)]
        links: bool,
        /// Print scan phases and Git costs to stderr (JSON also includes them).
        #[arg(long)]
        profile: bool,
    },
    /// Select a worktree and print its path. Use shell-init for directory switching.
    #[command(alias = "switch")]
    Cd {
        query: Option<String>,
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Resolve an unambiguous worktree query without opening the TUI.
    Path {
        query: String,
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// NUL delimiter for paths containing newlines.
        #[arg(short = '0', long)]
        null: bool,
    },
    /// Create a worktree on an existing or new branch.
    #[command(alias = "create")]
    Add {
        path: PathBuf,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        branch: String,
        #[arg(short = 'b', long)]
        new_branch: bool,
        #[arg(long)]
        from: Option<String>,
    },
    /// Remove a worktree; retain its branch by default.
    #[command(alias = "rm")]
    Remove {
        path: PathBuf,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(long)]
        dry_run: bool,
        /// Discard uncommitted files. Locks and primary worktrees remain protected.
        #[arg(long)]
        force: bool,
        /// Delete the local branch only if Git considers it merged.
        #[arg(long)]
        delete_branch: bool,
    },
    /// Move a registered worktree to a new path.
    Move {
        path: PathBuf,
        target: PathBuf,
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Protect a worktree from removal or pruning.
    Lock {
        path: PathBuf,
        /// Owning repository, required when the worktree directory is missing.
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long, default_value = "Protected by worktree-manager")]
        reason: String,
    },
    /// Unlock a worktree after confirmation.
    Unlock {
        path: PathBuf,
        /// Owning repository, required when the worktree directory is missing.
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Preview and optionally prune stale worktree registration records.
    Prune {
        #[arg(default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Fetch all remotes and prune remote tracking refs.
    Fetch {
        #[arg(default_value = ".")]
        repo: PathBuf,
    },
    /// Run a command in a matching worktree without invoking a shell.
    Exec {
        query: String,
        #[arg(long, default_value = ".")]
        root: PathBuf,
        #[arg(required = true, last = true)]
        command: Vec<std::ffi::OsString>,
    },
    /// Print completions for your shell.
    Completion {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Print optional directory-switching integration (bash, zsh, fish).
    ShellInit {
        #[arg(value_enum)]
        shell: Shell,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum Sort {
    Repo,
    Activity,
    Status,
}
fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("Error: {}", safe_text(&format!("{error:#}")));
            ExitCode::FAILURE
        }
    }
}
fn run(cli: Cli) -> Result<u8> {
    if cli.directory.is_some() && cli.command.is_some() {
        bail!("put the directory after the subcommand, for example `wtm list /path --json`");
    }
    if cli.jobs == Some(0) {
        bail!("--jobs must be at least 1");
    }
    if !matches!(
        &cli.command,
        Some(Commands::Completion { .. } | Commands::ShellInit { .. })
    ) {
        git::check_version()?;
    }
    let options = ScanOptions {
        root: cli.directory.unwrap_or_else(|| PathBuf::from(".")),
        follow_links: cli.follow_links,
        jobs: cli.jobs.unwrap_or_else(|| ScanOptions::default().jobs),
        excludes: cli.exclude,
    };
    match cli.command {
        None => launch(options, false, !cli.no_mouse)?,
        Some(Commands::Tui { directory }) => {
            launch(scope(&options, directory), false, !cli.no_mouse)?
        }
        Some(Commands::List {
            directory,
            json,
            sort,
            filter,
            links,
            profile,
        }) => {
            let mut report = scan::scan(&scope(&options, directory))?;
            if profile {
                eprintln!("{}", serde_json::to_string_pretty(&report.profile)?);
            }
            if let Some(query) = filter {
                report.worktrees.retain(|row| matches_query(row, &query));
            }
            sort_rows(&mut report.worktrees, sort);
            if json {
                serde_json::to_writer_pretty(io::stdout().lock(), &report)?;
                println!();
            } else {
                print_rows(&report.worktrees, links);
                eprintln!(
                    "{} worktrees / {} repositories / {} ms",
                    report.worktrees.len(),
                    report.repositories,
                    report.elapsed_ms
                );
                for warning in &report.warnings {
                    eprintln!("Warning: {}", safe_text(warning));
                }
            }
            if !report.warnings.is_empty() {
                return Ok(2);
            }
        }
        Some(Commands::Cd { query, root }) => {
            let opts = scope(&options, root);
            if let Some(query) = query {
                print_path(&resolve(&opts, &query)?.path, false)?;
            } else {
                launch(opts, true, !cli.no_mouse)?;
            }
        }
        Some(Commands::Path { query, root, null }) => {
            print_path(&resolve(&scope(&options, root), &query)?.path, null)?
        }
        Some(Commands::Add {
            path,
            repo,
            branch,
            new_branch,
            from,
        }) => {
            let path = git::add(&AddOptions {
                repo,
                path,
                branch,
                new_branch,
                start_point: from,
            })?;
            print_path(&path, false)?;
        }
        Some(Commands::Remove {
            path,
            yes,
            dry_run,
            force,
            delete_branch,
        }) => {
            let row = registered(&path)?;
            git::validate_remove(
                &row,
                &RemoveOptions {
                    force,
                    delete_branch,
                },
            )?;
            eprintln!(
                "Remove {} ({}, {})",
                safe_text(&row.path.display().to_string()),
                safe_text(row.branch.as_deref().unwrap_or("detached HEAD")),
                row.status.summary()
            );
            eprintln!(
                "The working directory, including ignored files, will be deleted. Branches are retained unless --delete-branch is set."
            );
            if force {
                eprintln!("Uncommitted files may be discarded.");
            }
            if dry_run {
                eprintln!("Preview only; no files or branches were changed.");
            } else {
                confirm(yes, "REMOVE")?;
                println!(
                    "{}",
                    safe_text(&git::remove(
                        &row,
                        &RemoveOptions {
                            force,
                            delete_branch
                        }
                    )?)
                );
            }
        }
        Some(Commands::Move {
            path,
            target,
            yes,
            dry_run,
        }) => {
            let row = registered(&path)?;
            eprintln!(
                "Move {} to {}",
                safe_text(&row.path.display().to_string()),
                safe_text(&target.display().to_string())
            );
            if !dry_run {
                confirm(yes, "MOVE")?;
                println!("{}", safe_text(&git::move_worktree(&row, &target)?));
            }
        }
        Some(Commands::Lock { path, reason, repo }) => {
            println!(
                "{}",
                safe_text(&git::lock(
                    &registered_with_owner(&path, repo.as_deref())?,
                    &reason
                )?)
            )
        }
        Some(Commands::Unlock { path, yes, repo }) => {
            let row = registered_with_owner(&path, repo.as_deref())?;
            eprintln!("Unlock {}", safe_text(&row.path.display().to_string()));
            confirm(yes, "UNLOCK")?;
            println!("{}", safe_text(&git::unlock(&row)?));
        }
        Some(Commands::Prune { repo, dry_run, yes }) => {
            let repo = git::repository(&repo)?;
            let preview = git::prune(&repo, true)?;
            eprintln!("{}", safe_text(&preview));
            if !dry_run {
                confirm(yes, "PRUNE")?;
                println!("{}", safe_text(&git::prune(&repo, false)?));
            }
        }
        Some(Commands::Fetch { repo }) => {
            println!("{}", safe_text(&git::fetch(&git::repository(&repo)?)?))
        }
        Some(Commands::Exec {
            query,
            root,
            command,
        }) => {
            let row = resolve(&scope(&options, root), &query)?;
            let status = Command::new(&command[0])
                .args(&command[1..])
                .current_dir(&row.path)
                .status()
                .context("execute command in worktree")?;
            return Ok(status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1));
        }
        Some(Commands::Completion { shell }) => {
            clap_complete::generate(shell, &mut Cli::command(), "wtm", &mut io::stdout())
        }
        Some(Commands::ShellInit { shell }) => print!("{}", shell_init(shell)?),
    }
    Ok(0)
}
fn scope(options: &ScanOptions, root: PathBuf) -> ScanOptions {
    ScanOptions {
        root,
        ..options.clone()
    }
}
fn launch(options: ScanOptions, select_only: bool, mouse: bool) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("the TUI requires terminal input and stderr; use `wtm list --json` for scripting");
    }
    if let Some(path) = tui::run(options, select_only, mouse)? {
        print_path(&path, false)?;
    }
    Ok(())
}
fn matches_query(row: &Worktree, query: &str) -> bool {
    let query = query.to_lowercase();
    row.repo.name.to_lowercase().contains(&query)
        || row.path.to_string_lossy().to_lowercase().contains(&query)
        || row
            .branch
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains(&query)
}
fn resolve(options: &ScanOptions, query: &str) -> Result<Worktree> {
    let report = scan::scan(options)?;
    for warning in &report.warnings {
        eprintln!("Warning: {}", safe_text(warning));
    }
    let path = Path::new(query).canonicalize().ok();
    if let Some(row) = report.worktrees.iter().find(|row| {
        path.as_ref()
            .is_some_and(|path| row.path.canonicalize().as_ref().ok() == Some(path))
    }) {
        if git::repository(&row.path)?.common_dir != row.repo.common_dir {
            bail!("selected worktree changed repositories; refresh and retry");
        }
        return Ok(row.clone());
    }
    if !report.discovery_complete {
        bail!("discovery was incomplete; fix the warnings or use an exact worktree path");
    }
    let matches: Vec<_> = report
        .worktrees
        .into_iter()
        .filter(|row| matches_query(row, query))
        .collect();
    match matches.len() {
        0 => bail!(
            "no worktree matches {query:?} below {}",
            options.root.display()
        ),
        1 => Ok(matches
            .into_iter()
            .next()
            .context("missing worktree match")?),
        _ => bail!(
            "query {query:?} is ambiguous; use an exact path. Matches: {}",
            matches
                .iter()
                .map(|row| row.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
fn registered(path: &Path) -> Result<Worktree> {
    registered_with_owner(path, None)
}

fn registered_with_owner(path: &Path, owner: Option<&Path>) -> Result<Worktree> {
    let path = std::path::absolute(path).with_context(|| format!("resolve {}", path.display()))?;
    let path = path.canonicalize().unwrap_or(path);
    let repo = git::repository(owner.unwrap_or(&path))?;
    git::worktrees_filtered(&repo, |candidate| {
        candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.to_path_buf())
            == path
    })?
    .into_iter()
    .find(|row| row.path.canonicalize().unwrap_or_else(|_| row.path.clone()) == path)
    .context("path is not a registered worktree root")
}
fn confirm(yes: bool, word: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!("confirmation requires a terminal; review --dry-run and pass --yes explicitly");
    }
    eprint!("Type {word} to confirm: ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim() != word {
        bail!("cancelled; nothing changed");
    }
    Ok(())
}
fn print_path(path: &Path, null: bool) -> Result<()> {
    let mut stdout = io::stdout().lock();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        if !null && bytes.contains(&b'\n') {
            bail!("path contains a newline; use `wtm path -0` for NUL-delimited output");
        }
        stdout.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    {
        let value = path.to_string_lossy();
        if !null && value.contains('\n') {
            bail!("path contains a newline; use `wtm path -0`");
        }
        stdout.write_all(value.as_bytes())?;
    }
    stdout.write_all(if null { b"\0" } else { b"\n" })?;
    Ok(())
}
fn sort_rows(rows: &mut [Worktree], sort: Sort) {
    rows.sort_by(|a, b| match sort {
        Sort::Repo => a.repo.name.cmp(&b.repo.name).then(a.path.cmp(&b.path)),
        Sort::Activity => b.updated_at.cmp(&a.updated_at).then(a.path.cmp(&b.path)),
        Sort::Status => b
            .status
            .is_dirty()
            .cmp(&a.status.is_dirty())
            .then(a.path.cmp(&b.path)),
    });
}
fn safe_text(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
fn print_rows(rows: &[Worktree], links: bool) {
    println!(
        "{:<22} {:<26} {:<12} {:<12} {:<28} PATH",
        "REPOSITORY", "BRANCH", "COMMIT", "UPDATED", "STATE"
    );
    let links = links && io::stdout().is_terminal();
    for row in rows {
        let hash = row.short_head.as_deref().unwrap_or("unborn");
        let commit = if links {
            row.commit_url.as_ref().map_or_else(
                || format!("{hash:<12}"),
                |url| format!("\x1b]8;;{}\x1b\\{hash:<12}\x1b]8;;\x1b\\", safe_text(url)),
            )
        } else {
            format!("{hash:<12}")
        };
        let state = if row.is_bare {
            "bare".into()
        } else if row.prunable.is_some() {
            "stale".into()
        } else if row.locked.is_some() {
            format!("locked / {}", row.status.summary())
        } else {
            row.status.summary()
        };
        let age = row.updated_at.map_or_else(
            || "unknown".into(),
            |date| {
                let seconds = (Utc::now() - date).num_seconds().max(0);
                if seconds < 60 {
                    "just now".into()
                } else if seconds < 3600 {
                    format!("{}m ago", seconds / 60)
                } else if seconds < 86400 {
                    format!("{}h ago", seconds / 3600)
                } else {
                    format!("{}d ago", seconds / 86400)
                }
            },
        );
        println!(
            "{:<22} {:<26} {} {:<12} {:<28} {}",
            safe_text(&row.repo.name),
            safe_text(row.branch.as_deref().unwrap_or("detached")),
            commit,
            age,
            state,
            safe_text(&row.path.display().to_string())
        );
    }
    if rows.is_empty() {
        println!("No worktrees found below the scan directory.");
    }
}
fn shell_init(shell: Shell) -> Result<&'static str> {
    match shell {
        Shell::Bash | Shell::Zsh => Ok(r#"wtm() {
    if [ "$1" = "cd" ] || [ "$1" = "switch" ]; then
        case " $* " in
            *" --help "*|*" -h "*) command wtm "$@"; return $? ;;
        esac
        local wtm_target
        wtm_target="$(command wtm "$@")" || return $?
        if [ -n "$wtm_target" ]; then
            builtin cd -- "$wtm_target"
        fi
    else
        command wtm "$@"
    fi
}
"#),
        Shell::Fish => Ok(r#"function wtm
    if contains -- $argv[1] cd switch; and not contains -- --help $argv; and not contains -- -h $argv
        set -l wtm_target (command wtm $argv)
        or return $status
        if test -n "$wtm_target"
            builtin cd -- "$wtm_target"
        end
    else
        command wtm $argv
    end
end
"#),
        _ => bail!(
            "directory switching supports bash, zsh, and fish; completions support additional shells"
        ),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_contract() {
        Cli::command().debug_assert();
        assert!(Cli::try_parse_from(["wtm", "list", "/tmp", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["wtm", "/tmp"]).is_ok());
        assert!(Cli::try_parse_from(["wtm", "exec", "feature", "--", "git", "status"]).is_ok());
    }
    #[test]
    fn terminal_text_is_sanitized() {
        assert_eq!(safe_text("hello\x1b[31m\nworld\t"), "hello [31m world ");
    }
}
