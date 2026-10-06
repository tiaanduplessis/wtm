use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn fixture() -> (TempDir, PathBuf, PathBuf) {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let repo = temp.path().join("owner");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    fs::write(repo.join("file"), "hello\n").unwrap();
    git(&repo, &["add", "file"]);
    git(&repo, &["commit", "-m", "initial"]);
    let linked = temp.path().join("feature tree");
    git(
        &repo,
        &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
    );
    (temp, repo, linked)
}
fn git(repo: &Path, args: &[&str]) {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn wtm(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wtm"))
        .args(args)
        .output()
        .unwrap()
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn json_lists_metadata_and_filters_nested_worktrees() {
    let (temp, repo, linked) = fixture();
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:example/project.git",
        ],
    );
    fs::write(linked.join("local"), "change").unwrap();
    let output = wtm(&[
        "list",
        temp.path().to_str().unwrap(),
        "--json",
        "--filter",
        "feature",
    ]);
    success(&output);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = report["worktrees"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["repo"]["path"], repo.to_str().unwrap());
    assert_eq!(rows[0]["path"], linked.to_str().unwrap());
    assert_eq!(rows[0]["branch"], "feature");
    assert_eq!(rows[0]["status"]["untracked"], 1);
    assert!(rows[0]["short_head"].as_str().unwrap().len() >= 7);
    assert!(
        rows[0]["commit_url"]
            .as_str()
            .unwrap()
            .starts_with("https://github.com/example/project/commit/")
    );
    assert!(rows[0]["updated_at"].is_string());
}
#[test]
fn non_terminal_tui_fails_with_actionable_error() {
    let (temp, _, _) = fixture();
    let output = wtm(&[temp.path().to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("wtm list --json"));
    assert!(!output.stderr.contains(&0x1b));
}
#[test]
fn removal_requires_confirmation_and_retains_branch() {
    let (_temp, repo, linked) = fixture();
    success(&wtm(&["remove", linked.to_str().unwrap(), "--dry-run"]));
    assert!(linked.is_dir());
    assert!(!wtm(&["remove", linked.to_str().unwrap()]).status.success());
    assert!(linked.is_dir());
    success(&wtm(&["remove", linked.to_str().unwrap(), "--yes"]));
    assert!(!linked.exists());
    git(&repo, &["show-ref", "--verify", "refs/heads/feature"]);
    assert!(repo.is_dir());
}
#[test]
fn dirty_worktree_is_protected_unless_force_is_explicit() {
    let (_temp, _, linked) = fixture();
    fs::write(linked.join("local.txt"), "valuable").unwrap();
    assert!(
        !wtm(&["remove", linked.to_str().unwrap(), "--yes"])
            .status
            .success()
    );
    assert!(linked.join("local.txt").exists());
    success(&wtm(&[
        "remove",
        linked.to_str().unwrap(),
        "--yes",
        "--force",
    ]));
    assert!(!linked.exists());
}
#[test]
fn cli_creates_moves_locks_and_unlocks() {
    let (temp, repo, _) = fixture();
    let created = temp.path().join("new tree");
    success(&wtm(&[
        "add",
        created.to_str().unwrap(),
        "--repo",
        repo.to_str().unwrap(),
        "--branch",
        "new-topic",
        "--new-branch",
        "--from",
        "main",
    ]));
    success(&wtm(&[
        "lock",
        created.to_str().unwrap(),
        "--reason",
        "retain fixture",
    ]));
    assert!(
        !wtm(&["remove", created.to_str().unwrap(), "--force", "--yes"])
            .status
            .success()
    );
    success(&wtm(&["unlock", created.to_str().unwrap(), "--yes"]));
    let moved = temp.path().join("moved tree");
    success(&wtm(&[
        "move",
        created.to_str().unwrap(),
        moved.to_str().unwrap(),
        "--yes",
    ]));
    assert!(!created.exists());
    assert!(moved.exists());
}
#[test]
fn path_resolution_refuses_ambiguity_and_handles_exact_paths() {
    let (temp, repo, linked) = fixture();
    let output = wtm(&["path", "feature", "--root", temp.path().to_str().unwrap()]);
    success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        linked.to_str().unwrap()
    );
    let other = temp.path().join("feature-other");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature-two",
            other.to_str().unwrap(),
        ],
    );
    let ambiguous = wtm(&["path", "feature", "--root", temp.path().to_str().unwrap()]);
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("ambiguous"));
    success(&wtm(&[
        "path",
        linked.to_str().unwrap(),
        "--root",
        temp.path().to_str().unwrap(),
    ]));
}
#[test]
fn newline_paths_use_nul_delimited_output() {
    let (temp, repo, _) = fixture();
    let path = temp.path().join("line\nbreak");
    git(
        &repo,
        &["worktree", "add", "-b", "newline", path.to_str().unwrap()],
    );
    let args = ["path", "newline", "--root", temp.path().to_str().unwrap()];
    assert!(!wtm(&args).status.success());
    let output = wtm(&[
        "path",
        "newline",
        "--root",
        temp.path().to_str().unwrap(),
        "-0",
    ]);
    success(&output);
    assert_eq!(
        output.stdout,
        [path.to_str().unwrap().as_bytes(), b"\0"].concat()
    );
}
#[test]
fn exec_runs_in_selected_worktree_and_propagates_status() {
    let (temp, _, _) = fixture();
    let output = wtm(&[
        "exec",
        "feature",
        "--root",
        temp.path().to_str().unwrap(),
        "--",
        "git",
        "rev-parse",
        "--abbrev-ref",
        "HEAD",
    ]);
    success(&output);
    assert_eq!(output.stdout, b"feature\n");
    #[cfg(unix)]
    {
        let output = wtm(&[
            "exec",
            "feature",
            "--root",
            temp.path().to_str().unwrap(),
            "--",
            "sh",
            "-c",
            "exit 7",
        ]);
        assert_eq!(output.status.code(), Some(7));
    }
}
#[cfg(unix)]
#[test]
fn bash_shell_integration_changes_directory_and_preserves_it_on_failure() {
    let (temp, _, linked) = fixture();
    let binary_dir = Path::new(env!("CARGO_BIN_EXE_wtm")).parent().unwrap();
    let path = format!(
        "{}:{}",
        binary_dir.display(),
        std::env::var("PATH").unwrap()
    );
    let script = "eval \"$(wtm shell-init bash)\"\nwtm cd feature --root \"$1\" || exit\nwtm cd nonexistent --root \"$1\" 2>/dev/null && exit 8\nprintf '%s' \"$PWD\"";
    let output = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            script,
            "wtm-test",
            temp.path().to_str().unwrap(),
        ])
        .env("PATH", path)
        .output()
        .unwrap();
    success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        linked.to_str().unwrap()
    );
}

#[test]
fn stale_metadata_does_not_block_healthy_selection() {
    let (temp, repo, linked) = fixture();
    let stale = temp.path().join("stale");
    git(
        &repo,
        &["worktree", "add", "-b", "stale", stale.to_str().unwrap()],
    );
    fs::remove_dir_all(&stale).unwrap();
    let output = wtm(&["path", "feature", "--root", temp.path().to_str().unwrap()]);
    success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        linked.to_str().unwrap()
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("Warning:"));
    let json = wtm(&["list", temp.path().to_str().unwrap(), "--json"]);
    assert_eq!(json.status.code(), Some(2));
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["discovery_complete"], true);
}

#[test]
fn incomplete_discovery_requires_exact_path_for_selection() {
    let (temp, _, linked) = fixture();
    let broken = temp.path().join("broken");
    fs::create_dir(&broken).unwrap();
    fs::write(broken.join(".git"), "gitdir: missing\n").unwrap();
    let fuzzy = wtm(&["path", "feature", "--root", temp.path().to_str().unwrap()]);
    assert!(!fuzzy.status.success());
    assert!(String::from_utf8_lossy(&fuzzy.stderr).contains("discovery was incomplete"));
    success(&wtm(&[
        "path",
        linked.to_str().unwrap(),
        "--root",
        temp.path().to_str().unwrap(),
    ]));
}

#[test]
fn leading_directory_with_subcommand_is_rejected() {
    let (temp, _, _) = fixture();
    let output = wtm(&[temp.path().to_str().unwrap(), "list", "--json"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("directory after the subcommand"));
}

#[test]
fn missing_registration_can_be_locked_and_unlocked_with_owner() {
    let (_temp, repo, linked) = fixture();
    fs::remove_dir_all(&linked).unwrap();
    success(&wtm(&[
        "lock",
        linked.to_str().unwrap(),
        "--repo",
        repo.to_str().unwrap(),
        "--reason",
        "offline",
    ]));
    success(&wtm(&[
        "unlock",
        linked.to_str().unwrap(),
        "--repo",
        repo.to_str().unwrap(),
        "--yes",
    ]));
    assert!(!linked.exists());
}

#[test]
fn terminal_errors_do_not_emit_control_sequences() {
    let (temp, _, _) = fixture();
    let missing = temp.path().join("missing\u{1b}[31m");
    let output = wtm(&["list", missing.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(!output.stderr.contains(&0x1b));
}

#[test]
fn profile_reports_costs_without_polluting_json_output() {
    let (temp, _, _) = fixture();
    let output = wtm(&["list", temp.path().to_str().unwrap(), "--json", "--profile"]);
    success(&output);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let profile: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(profile, report["profile"]);
    assert!(profile["git_commands"].as_u64().unwrap() > 0);
    assert_eq!(profile["commands"]["status"]["calls"], 2);
    assert!(profile["first_result_ms"].is_number());
    assert_eq!(report["pending_repositories"], serde_json::json!([]));
}

#[cfg(unix)]
#[test]
fn old_git_is_rejected_before_scanning_and_completions_do_not_require_git() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let executable = temp.path().join("git");
    fs::write(
        &executable,
        "#!/bin/sh\nfor argument; do\n  if [ \"$argument\" = --version ]; then\n    echo 'git version 2.35.0'\n    exit 0\n  fi\ndone\necho 'unexpected Git command' >&2\nexit 99\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wtm"))
        .args(["list", temp.path().to_str().unwrap(), "--json"])
        .env("PATH", temp.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("Git 2.36"), "{error}");
    assert!(!error.contains("unexpected Git command"));

    fs::remove_file(executable).unwrap();
    let completions = Command::new(env!("CARGO_BIN_EXE_wtm"))
        .args(["completion", "zsh"])
        .env("PATH", temp.path())
        .output()
        .unwrap();
    success(&completions);
    assert!(!completions.stdout.is_empty());
}
