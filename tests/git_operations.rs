use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

use tempfile::TempDir;
use worktree_manager::{
    git::{self, AddOptions, RemoveOptions},
    model::Worktree,
};

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    repo: PathBuf,
}

fn run(path: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test Author")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("main repo");
        fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "--initial-branch=main"]);
        run(&repo, &["config", "user.name", "Test Author"]);
        run(&repo, &["config", "user.email", "test@example.invalid"]);
        run(&repo, &["config", "commit.gpgsign", "false"]);
        run(&repo, &["config", "core.hooksPath", "/dev/null"]);
        fs::write(repo.join("tracked.txt"), "initial\n").unwrap();
        fs::write(repo.join(".gitignore"), "build/\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-m", "Initial commit"]);
        Self {
            _temp: temp,
            root,
            repo,
        }
    }

    fn add(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        git::add(&AddOptions {
            repo: self.repo.clone(),
            path: path.clone(),
            branch: name.replace(['\n', ' '], "-"),
            new_branch: true,
            start_point: None,
        })
        .unwrap();
        path
    }

    fn row(&self, path: &Path) -> Worktree {
        let repo = git::repository(&self.repo).unwrap();
        git::worktrees(&repo)
            .unwrap()
            .into_iter()
            .find(|row| row.path == path)
            .unwrap()
    }
}

#[test]
fn metadata_identifies_owner_and_counts_dirty_changes() {
    let f = Fixture::new();
    let path = f.add("feature");
    let row = f.row(&path);
    assert_eq!(row.repo.name, "main repo");
    assert_eq!(row.repo.path, f.repo);
    assert_eq!(row.repo.common_dir, f.repo.join(".git"));
    assert_eq!(row.branch.as_deref(), Some("feature"));
    assert_eq!(row.commit_subject.as_deref(), Some("Initial commit"));
    assert!(
        row.head
            .as_ref()
            .unwrap()
            .starts_with(row.short_head.as_ref().unwrap())
    );
    assert!(row.committed_at.is_some());
    assert!(row.updated_at.is_some());
    assert!(!row.is_main);
    assert!(row.status.error.is_none(), "{:?}", row.status);
    assert!(!row.status.is_dirty());
    assert_eq!(row.merged, Some(true));
    assert!(row.warnings.is_empty(), "{:?}", row.warnings);

    fs::write(path.join("tracked.txt"), "modified\n").unwrap();
    fs::write(path.join("staged.txt"), "staged\n").unwrap();
    run(&path, &["add", "staged.txt"]);
    fs::write(path.join("untracked\nfile"), "untracked\n").unwrap();
    let row = f.row(&path);
    assert_eq!(row.status.modified, 1);
    assert_eq!(row.status.staged, 1);
    assert_eq!(row.status.untracked, 1);
    assert_eq!(row.status.conflicted, 0);
}

#[test]
fn removal_preserves_branches_by_default_and_protects_dirty_worktrees() {
    let f = Fixture::new();
    let path = f.add("feature");
    fs::write(path.join("untracked"), "valuable").unwrap();
    let selected = f.row(&path);
    let error = git::remove(&selected, &RemoveOptions::default()).unwrap_err();
    assert!(error.to_string().contains("uncommitted changes"));
    assert!(path.join("untracked").exists());
    git::remove(
        &selected,
        &RemoveOptions {
            force: true,
            delete_branch: false,
        },
    )
    .unwrap();
    assert!(!path.exists());
    run(&f.repo, &["show-ref", "--verify", "refs/heads/feature"]);
}

#[test]
fn merged_branch_deletion_is_safe_and_unmerged_branch_survives() {
    let f = Fixture::new();
    let merged = f.add("merged");
    git::remove(
        &f.row(&merged),
        &RemoveOptions {
            force: false,
            delete_branch: true,
        },
    )
    .unwrap();
    assert!(!run(&f.repo, &["branch", "--list", "merged"]).contains("merged"));

    let unmerged = f.add("unmerged");
    fs::write(unmerged.join("tracked.txt"), "new commit\n").unwrap();
    run(&unmerged, &["commit", "-am", "Unmerged commit"]);
    assert_eq!(f.row(&unmerged).merged, Some(false));
    let error = git::remove(
        &f.row(&unmerged),
        &RemoveOptions {
            force: false,
            delete_branch: true,
        },
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("worktree removed, but its branch was retained")
    );
    assert!(!unmerged.exists());
    run(&f.repo, &["show-ref", "--verify", "refs/heads/unmerged"]);
}

#[test]
fn locks_are_never_waived_by_force_and_can_be_explicitly_unlocked() {
    let f = Fixture::new();
    let path = f.add("locked");
    git::lock(&f.row(&path), "keep for investigation").unwrap();
    let row = f.row(&path);
    assert_eq!(row.locked.as_deref(), Some("keep for investigation"));
    assert!(
        git::remove(
            &row,
            &RemoveOptions {
                force: true,
                delete_branch: true
            }
        )
        .unwrap_err()
        .to_string()
        .contains("locked")
    );
    assert!(git::move_worktree(&row, &f.root.join("destination")).is_err());
    git::unlock(&row).unwrap();
    git::remove(&f.row(&path), &RemoveOptions::default()).unwrap();
}

#[test]
fn primary_repository_is_protected_from_every_linked_operation() {
    let f = Fixture::new();
    let row = f.row(&f.repo);
    assert!(row.is_main);
    assert!(
        git::remove(
            &row,
            &RemoveOptions {
                force: true,
                delete_branch: true
            }
        )
        .is_err()
    );
    assert!(git::move_worktree(&row, &f.root.join("new-primary")).is_err());
    assert!(git::lock(&row, "primary").is_err());
    assert!(git::unlock(&row).is_err());
    assert!(f.repo.join("tracked.txt").exists());
}

#[test]
fn stale_head_and_replaced_repository_are_refused() {
    let f = Fixture::new();
    let path = f.add("feature");
    let selected = f.row(&path);
    run(
        &path,
        &["commit", "--allow-empty", "-m", "Changed after selection"],
    );
    assert!(
        git::remove(&selected, &RemoveOptions::default())
            .unwrap_err()
            .to_string()
            .contains("HEAD or branch changed")
    );
    assert!(path.exists());
    let selected = f.row(&path);
    // Preserve the old tree elsewhere and put a different repository at its registered path.
    fs::rename(&path, f.root.join("preserved")).unwrap();
    fs::create_dir(&path).unwrap();
    run(&path, &["init", "--initial-branch=main"]);
    assert!(
        git::remove(
            &selected,
            &RemoveOptions {
                force: true,
                delete_branch: false
            }
        )
        .is_err()
    );
    assert!(path.join(".git").exists());
    assert!(f.row(&path).status.error.is_some());
}

#[test]
fn removing_nested_repositories_is_refused_even_with_force() {
    let f = Fixture::new();
    let path = f.add("feature");
    let nested = path.join("ignored nested");
    fs::create_dir(&nested).unwrap();
    run(&nested, &["init", "--initial-branch=main"]);
    let error = git::remove(
        &f.row(&path),
        &RemoveOptions {
            force: true,
            delete_branch: false,
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("nested repository"));
    assert!(nested.join(".git").exists());
    assert!(git::move_worktree(&f.row(&path), &f.root.join("new-path")).is_err());
}

#[test]
fn nested_bare_repository_is_protected() {
    let f = Fixture::new();
    let path = f.add("feature");
    let nested = path.join("bare");
    fs::create_dir(&nested).unwrap();
    run(&nested, &["init", "--bare", "--initial-branch=main"]);
    assert!(
        git::remove(
            &f.row(&path),
            &RemoveOptions {
                force: true,
                delete_branch: false
            }
        )
        .unwrap_err()
        .to_string()
        .contains("nested bare repository")
    );
    assert!(nested.join("HEAD").exists());
}

#[test]
fn move_preserves_content_and_registry_and_handles_newline_paths() {
    let f = Fixture::new();
    let path = f.add("line\nbreak");
    fs::write(path.join("untracked\nfile"), "preserved").unwrap();
    let destination = f.root.join("moved path\nwith newline");
    git::move_worktree(&f.row(&path), &destination).unwrap();
    assert!(!path.exists());
    assert_eq!(
        fs::read_to_string(destination.join("untracked\nfile")).unwrap(),
        "preserved"
    );
    let row = f.row(&destination);
    assert_eq!(row.status.untracked, 1);
    assert!(row.status.error.is_none());
    assert_eq!(
        git::repository(&destination).unwrap().common_dir,
        f.repo.join(".git")
    );
}

#[test]
fn stale_registry_prune_preview_is_read_only_and_locked_entries_survive() {
    let f = Fixture::new();
    let missing = f.add("missing");
    let locked = f.add("locked");
    git::lock(&f.row(&locked), "offline disk").unwrap();
    fs::remove_dir_all(&missing).unwrap();
    fs::remove_dir_all(&locked).unwrap();
    let repo = git::repository(&f.repo).unwrap();
    let rows = git::worktrees(&repo).unwrap();
    let missing_row = rows.iter().find(|row| row.path == missing).unwrap();
    assert!(missing_row.prunable.is_some());
    assert!(missing_row.status.error.is_some());
    assert!(git::prune(&repo, true).unwrap().contains("Removing"));
    assert_eq!(git::worktrees(&repo).unwrap().len(), 3);
    git::prune(&repo, false).unwrap();
    let rows = git::worktrees(&repo).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|row| row.path == locked && row.locked.is_some())
    );
}

#[test]
fn detached_unborn_and_bare_repositories_are_represented() {
    let f = Fixture::new();
    let detached = f.root.join("detached");
    run(
        &f.repo,
        &[
            "worktree",
            "add",
            "--detach",
            detached.to_str().unwrap(),
            "HEAD",
        ],
    );
    let row = f.row(&detached);
    assert_eq!(row.branch, None);
    assert!(row.head.is_some());
    assert!(row.status.error.is_none());

    let unborn = f.root.join("unborn");
    fs::create_dir(&unborn).unwrap();
    run(&unborn, &["init", "--initial-branch=main"]);
    let rows = git::worktrees(&git::repository(&unborn).unwrap()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].head, None);
    assert_eq!(rows[0].branch.as_deref(), Some("main"));
    assert!(rows[0].status.error.is_none());

    let bare = f.root.join("bare.git");
    fs::create_dir(&bare).unwrap();
    run(&bare, &["init", "--bare", "--initial-branch=main"]);
    let rows = git::worktrees(&git::repository(&bare).unwrap()).unwrap();
    assert!(rows[0].is_bare && rows[0].is_main);
    assert!(git::remove(&rows[0], &RemoveOptions::default()).is_err());
}

#[test]
fn ignored_build_activity_does_not_change_timestamp_or_refresh_index() {
    let f = Fixture::new();
    let path = f.add("feature");
    let index = PathBuf::from(run(
        &path,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    ));
    let original_index = fs::metadata(&index).unwrap().modified().unwrap();
    let before = f.row(&path).updated_at.unwrap();
    fs::create_dir(path.join("build")).unwrap();
    fs::write(path.join("build/generated"), "ignored").unwrap();
    let file = fs::File::options()
        .write(true)
        .open(path.join("build/generated"))
        .unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(3600))
        .unwrap();
    assert_eq!(f.row(&path).updated_at.unwrap(), before);
    assert_eq!(
        fs::metadata(&index).unwrap().modified().unwrap(),
        original_index
    );
    let tracked = fs::File::options()
        .write(true)
        .open(path.join("tracked.txt"))
        .unwrap();
    tracked
        .set_modified(SystemTime::now() + Duration::from_secs(7200))
        .unwrap();
    assert!(f.row(&path).updated_at.unwrap() > before);
}

#[test]
fn staged_rename_and_conflict_status_are_counted_without_path_confusion() {
    let f = Fixture::new();
    let path = f.add("feature");
    run(&path, &["mv", "tracked.txt", "renamed\nfile.txt"]);
    assert_eq!(f.row(&path).status.staged, 1);
    run(&path, &["reset", "--hard", "HEAD"]);
    fs::write(path.join("tracked.txt"), "feature\n").unwrap();
    run(&path, &["commit", "-am", "Feature change"]);
    fs::write(f.repo.join("tracked.txt"), "main\n").unwrap();
    run(&f.repo, &["commit", "-am", "Main change"]);
    let result = Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["merge", "main"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let row = f.row(&path);
    assert_eq!(row.status.conflicted, 1);
    assert_eq!(row.status.staged, 0);
    assert_eq!(row.status.modified, 0);
    assert!(git::remove(&row, &RemoveOptions::default()).is_err());
}

#[test]
fn local_fetch_reports_upstream_and_remote_default_merge_status() {
    let f = Fixture::new();
    let remote = f.root.join("remote.git");
    run(
        &f.repo,
        &[
            "clone",
            "--bare",
            f.repo.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    run(
        &f.repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let repo = git::repository(&f.repo).unwrap();
    git::fetch(&repo).unwrap();
    run(&f.repo, &["remote", "set-head", "origin", "main"]);
    run(
        &f.repo,
        &["branch", "--set-upstream-to=origin/main", "main"],
    );
    let row = f.row(&f.repo);
    assert_eq!(row.upstream.as_deref(), Some("origin/main"));
    assert_eq!((row.ahead, row.behind), (Some(0), Some(0)));
    run(&f.repo, &["commit", "--allow-empty", "-m", "Ahead"]);
    let row = f.row(&f.repo);
    assert_eq!((row.ahead, row.behind), (Some(1), Some(0)));
    assert_eq!(row.merged, Some(false));
}

#[test]
fn invalid_repository_and_failed_fetch_are_errors_not_fake_success() {
    let f = Fixture::new();
    assert!(git::repository(&f.root).is_err());
    run(
        &f.repo,
        &[
            "remote",
            "add",
            "origin",
            "/nonexistent/worktree-manager-test-remote",
        ],
    );
    assert!(git::fetch(&git::repository(&f.repo).unwrap()).is_err());
    let path = f.add("feature");
    fs::remove_file(path.join(".git")).unwrap();
    let row = f.row(&path);
    assert!(row.status.error.is_some());
    assert_eq!(row.status.summary(), "unknown");
    assert!(
        git::remove(
            &row,
            &RemoveOptions {
                force: true,
                delete_branch: false
            }
        )
        .is_err()
    );
}

#[test]
fn remote_links_never_expose_http_credentials() {
    let f = Fixture::new();
    run(
        &f.repo,
        &[
            "remote",
            "add",
            "origin",
            "https://user:secret@github.com/team/repo.git",
        ],
    );
    let row = f.row(&f.repo);
    assert_eq!(
        row.repo.remote.as_deref(),
        Some("https://github.com/team/repo.git")
    );
    assert_eq!(
        row.commit_url,
        Some(format!(
            "https://github.com/team/repo/commit/{}",
            row.head.unwrap()
        ))
    );
}

#[test]
fn existing_branch_add_and_invalid_inputs_use_git_validation() {
    let f = Fixture::new();
    run(&f.repo, &["branch", "existing"]);
    let options = AddOptions {
        repo: f.repo.clone(),
        path: f.root.join("existing tree"),
        branch: "existing".into(),
        new_branch: false,
        start_point: None,
    };
    let path = git::add(&options).unwrap();
    assert_eq!(f.row(&path).branch.as_deref(), Some("existing"));
    assert!(
        git::add(&AddOptions {
            path: f.root.join("duplicate"),
            ..options.clone()
        })
        .is_err()
    );
    assert!(
        git::add(&AddOptions {
            path: f.root.join("invalid"),
            branch: "--orphan".into(),
            new_branch: true,
            ..options.clone()
        })
        .is_err()
    );
    assert!(
        git::add(&AddOptions {
            start_point: Some("main".into()),
            ..options
        })
        .is_err()
    );
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn native_non_utf8_paths_are_preserved() {
    use std::os::unix::ffi::OsStringExt;
    let f = Fixture::new();
    let path = f
        .root
        .join(std::ffi::OsString::from_vec(b"tree-\xff".to_vec()));
    git::add(&AddOptions {
        repo: f.repo.clone(),
        path: path.clone(),
        branch: "native".into(),
        new_branch: true,
        start_point: None,
    })
    .unwrap();
    fs::write(
        path.join(std::ffi::OsString::from_vec(b"file-\xff".to_vec())),
        "native",
    )
    .unwrap();
    let row = f.row(&path);
    assert_eq!(row.status.untracked, 1);
    assert!(row.status.error.is_none());
    git::remove(
        &row,
        &RemoveOptions {
            force: true,
            delete_branch: false,
        },
    )
    .unwrap();
    assert!(!path.exists());
}

#[test]
fn changed_lock_reason_requires_refresh_before_unlock() {
    let f = Fixture::new();
    let path = f.add("feature");
    git::lock(&f.row(&path), "old reason").unwrap();
    let selected = f.row(&path);
    run(&f.repo, &["worktree", "unlock", path.to_str().unwrap()]);
    run(
        &f.repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "new reason",
            path.to_str().unwrap(),
        ],
    );
    assert!(
        git::unlock(&selected)
            .unwrap_err()
            .to_string()
            .contains("lock changed")
    );
    assert_eq!(f.row(&path).locked.as_deref(), Some("new reason"));
}

#[test]
fn git_environment_overrides_cannot_redirect_repository() {
    // Spawn an isolated test process: Rust 2024 deliberately forbids unsynchronized
    // environment writes while the rest of the test suite runs in parallel.
    if let Some(path) = std::env::var_os("WTM_GIT_ISOLATION_ROOT") {
        let path = PathBuf::from(path);
        let repo = git::repository(&path).unwrap();
        assert_eq!(repo.common_dir, path.join(".git"));
        let row = git::worktrees(&repo).unwrap().remove(0);
        assert_eq!(row.commit_subject.as_deref(), Some("Initial commit"));
        assert!(row.status.error.is_none());
        assert!(!row.status.is_dirty());
        return;
    }
    let f = Fixture::new();
    let other = Fixture::new();
    let index = other.root.join("poisoned-index");
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "git_environment_overrides_cannot_redirect_repository",
            "--nocapture",
        ])
        .env("WTM_GIT_ISOLATION_ROOT", &f.repo)
        .env("GIT_DIR", other.repo.join(".git"))
        .env("GIT_COMMON_DIR", other.repo.join(".git"))
        .env("GIT_WORK_TREE", &other.repo)
        .env("GIT_INDEX_FILE", &index)
        .env("GIT_OBJECT_DIRECTORY", other.repo.join(".git/objects"))
        .env("GIT_CONFIG_PARAMETERS", "invalid injected config")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.bare")
        .env("GIT_CONFIG_VALUE_0", "true")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!index.exists());
}

#[test]
fn corrupted_index_is_unknown_and_requires_explicit_force() {
    let f = Fixture::new();
    let path = f.add("feature");
    let index = PathBuf::from(run(
        &path,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    ));
    fs::write(index, b"not a valid Git index").unwrap();
    let row = f.row(&path);
    assert!(row.status.error.is_some());
    assert_eq!(row.status.summary(), "unknown");
    assert!(git::validate_remove(&row, &RemoveOptions::default()).is_err());
    assert!(path.exists());
    let force = RemoveOptions {
        force: true,
        delete_branch: false,
    };
    git::validate_remove(&row, &force).unwrap();
    assert!(path.exists(), "validation must not remove files");
    git::remove(&row, &force).unwrap();
    assert!(!path.exists());
}

#[test]
fn missing_locked_registration_can_be_unlocked_then_pruned() {
    let f = Fixture::new();
    let path = f.add("feature");
    git::lock(&f.row(&path), "offline").unwrap();
    fs::remove_dir_all(&path).unwrap();
    let row = f.row(&path);
    assert!(row.status.error.is_some());
    git::unlock(&row).unwrap();
    let repo = git::repository(&f.repo).unwrap();
    git::prune(&repo, false).unwrap();
    assert_eq!(git::worktrees(&repo).unwrap().len(), 1);
}

#[test]
fn bare_repository_with_history_has_commit_metadata_and_linked_rows() {
    let f = Fixture::new();
    let bare = f.root.join("clone.git");
    run(
        &f.repo,
        &[
            "clone",
            "--bare",
            f.repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let repo = git::repository(&bare).unwrap();
    let row = git::worktrees(&repo).unwrap().remove(0);
    assert!(row.is_bare && row.is_main);
    assert_eq!(row.branch.as_deref(), Some("main"));
    assert_eq!(row.commit_subject.as_deref(), Some("Initial commit"));
    assert!(row.head.is_some() && row.short_head.is_some() && row.committed_at.is_some());
    assert!(row.updated_at.is_some());
    assert!(row.warnings.is_empty(), "{:?}", row.warnings);
    let path = f.root.join("bare linked");
    git::add(&AddOptions {
        repo: bare,
        path: path.clone(),
        branch: "linked".into(),
        new_branch: true,
        start_point: Some("main".into()),
    })
    .unwrap();
    let rows = git::worktrees(&repo).unwrap();
    assert_eq!(rows.len(), 2);
    let linked = rows.iter().find(|row| row.path == path).unwrap();
    assert!(!linked.is_bare && !linked.is_main);
    git::remove(linked, &RemoveOptions::default()).unwrap();
}

#[test]
fn separate_git_directory_checkout_has_correct_primary_root() {
    let f = Fixture::new();
    let path = f.root.join("separate");
    let metadata = f.root.join("metadata.git");
    fs::create_dir(&path).unwrap();
    run(
        &path,
        &[
            "init",
            "--initial-branch=main",
            "--separate-git-dir",
            metadata.to_str().unwrap(),
        ],
    );
    let repo = git::repository(&path).unwrap();
    assert_eq!(repo.path, path);
    assert_eq!(repo.common_dir, metadata);
    let rows = git::worktrees(&repo).unwrap();
    assert_eq!(rows[0].path, path);
    assert!(rows[0].is_main && rows[0].status.error.is_none());
}

#[test]
fn unstaged_deletion_uses_parent_directory_activity() {
    let f = Fixture::new();
    let path = f.add("feature");
    let before = f.row(&path).updated_at.unwrap();
    fs::remove_file(path.join("tracked.txt")).unwrap();
    let future = SystemTime::now() + Duration::from_secs(7200);
    fs::File::open(&path).unwrap().set_modified(future).unwrap();
    let row = f.row(&path);
    assert_eq!(row.status.modified, 1);
    assert!(row.updated_at.unwrap() > before);
}

#[test]
fn configured_but_missing_upstream_is_reported_as_warning() {
    let f = Fixture::new();
    run(
        &f.repo,
        &["remote", "add", "origin", "/not-needed-for-this-test"],
    );
    run(&f.repo, &["config", "branch.main.remote", "origin"]);
    run(
        &f.repo,
        &["config", "branch.main.merge", "refs/heads/missing"],
    );
    let row = f.row(&f.repo);
    assert!(row.upstream.is_none());
    assert!(
        row.warnings
            .iter()
            .any(|warning| warning.starts_with("upstream:"))
    );
    assert!(
        row.status.error.is_none(),
        "metadata failure must not invent a dirty-status failure"
    );
}

#[test]
fn custom_ssh_fetch_is_rejected_before_network_access() {
    let f = Fixture::new();
    run(
        &f.repo,
        &[
            "remote",
            "add",
            "origin",
            "git@example.invalid:team/repo.git",
        ],
    );
    run(&f.repo, &["config", "core.sshCommand", "false"]);
    let error = git::fetch(&git::repository(&f.repo).unwrap()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("custom SSH command noninteractively")
    );
    assert!(error.to_string().contains("refresh"));
}

#[test]
fn file_fetch_is_unaffected_by_custom_ssh_configuration() {
    let f = Fixture::new();
    let remote = f.root.join("remote.git");
    run(
        &f.repo,
        &[
            "clone",
            "--bare",
            f.repo.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    run(
        &f.repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    run(&f.repo, &["config", "core.sshCommand", "false"]);
    git::fetch(&git::repository(&f.repo).unwrap()).unwrap();
    run(
        &f.repo,
        &["show-ref", "--verify", "refs/remotes/origin/main"],
    );
}

#[cfg(unix)]
#[test]
fn filtered_inventory_never_inspects_excluded_sibling_worktrees() {
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    };
    if let Some(repo) = std::env::var_os("WTM_FILTERED_TEST_REPO") {
        let included = PathBuf::from(std::env::var_os("WTM_FILTERED_TEST_INCLUDED").unwrap());
        let repo = git::repository(&PathBuf::from(repo)).unwrap();
        let rows = git::worktrees_filtered(&repo, |path| path == included).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, included);
        assert!(
            !rows[0].is_main,
            "filtering out the primary must preserve its original registration position"
        );
        assert!(rows[0].head.is_some() && rows[0].committed_at.is_some());
        assert!(rows[0].status.error.is_none() && rows[0].warnings.is_empty());
        return;
    }
    let f = Fixture::new();
    let included = f.add("included");
    let corrupt = f.add("corrupt");
    let replaced = f.add("replaced");
    let index = PathBuf::from(run(
        &corrupt,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    ));
    fs::write(index, "corrupted index").unwrap();
    fs::rename(&replaced, f.root.join("preserved")).unwrap();
    fs::create_dir(&replaced).unwrap();
    run(&replaced, &["init", "--initial-branch=main"]);
    assert!(f.row(&corrupt).status.error.is_some());
    assert!(f.row(&replaced).status.error.is_some());

    // Log command directories in a separate process without mutating the parallel
    // suite's environment. Arguments retain their native bytes through "$@".
    let original_path = std::env::var_os("PATH").unwrap();
    let real_git = std::env::split_paths(&original_path)
        .map(|path| path.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    let bin = f.root.join("bin");
    fs::create_dir(&bin).unwrap();
    let wrapper = bin.join("git");
    fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\0' \"$2\" >> \"$WTM_GIT_LOG\"\nexec \"$WTM_REAL_GIT\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let log = f.root.join("command-paths");
    let mut path: OsString = bin.as_os_str().to_owned();
    path.push(":");
    path.push(original_path);
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "filtered_inventory_never_inspects_excluded_sibling_worktrees",
            "--nocapture",
        ])
        .env("PATH", path)
        .env("WTM_REAL_GIT", real_git)
        .env("WTM_GIT_LOG", &log)
        .env("WTM_FILTERED_TEST_REPO", &f.repo)
        .env("WTM_FILTERED_TEST_INCLUDED", &included)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let commands = fs::read(log).unwrap();
    let directories: Vec<&[u8]> = commands.split(|byte| *byte == 0).collect();
    assert!(!directories.contains(&corrupt.as_os_str().as_bytes()));
    assert!(!directories.contains(&replaced.as_os_str().as_bytes()));
    assert!(directories.contains(&included.as_os_str().as_bytes()));
}

fn cached_row(f: &Fixture, path: &Path, cache: &git::InspectionCache) -> Worktree {
    let repo = git::repository(&f.repo).unwrap();
    git::worktrees_filtered_cached(&repo, |candidate| candidate == path, cache, |_| {})
        .unwrap()
        .remove(0)
}

#[test]
fn warm_cache_refreshes_dirty_status_and_stats_existing_and_new_files() {
    let f = Fixture::new();
    let path = f.add("feature");
    let cache = git::InspectionCache::default();
    let first = cached_row(&f, &path, &cache);
    let before = first.updated_at.unwrap();
    fs::write(path.join("tracked.txt"), "modified\n").unwrap();
    let tracked_time = SystemTime::now() + Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(path.join("tracked.txt"))
        .unwrap()
        .set_modified(tracked_time)
        .unwrap();
    let row = cached_row(&f, &path, &cache);
    assert_eq!(row.status.modified, 1);
    assert!(row.updated_at.unwrap() > before);
    assert_eq!(
        row.updated_at.unwrap(),
        chrono::DateTime::<chrono::Utc>::from(tracked_time)
    );
    assert!(
        git::remove(&first, &RemoveOptions::default())
            .unwrap_err()
            .to_string()
            .contains("uncommitted")
    );

    let untracked = path.join("new\nfile");
    fs::write(&untracked, "fresh").unwrap();
    let new_time = SystemTime::now() + Duration::from_secs(7200);
    fs::File::options()
        .write(true)
        .open(&untracked)
        .unwrap()
        .set_modified(new_time)
        .unwrap();
    let row = cached_row(&f, &path, &cache);
    assert_eq!(row.status.untracked, 1);
    assert_eq!(
        row.updated_at.unwrap(),
        chrono::DateTime::<chrono::Utc>::from(new_time)
    );
    fs::remove_file(untracked).unwrap();
    assert_eq!(cached_row(&f, &path, &cache).status.untracked, 0);
}

#[cfg(unix)]
#[test]
fn cached_file_list_invalidates_when_equal_size_index_mtime_is_restored() {
    let f = Fixture::new();
    let path = f.add("feature");
    run(&path, &["config", "core.fsmonitor", "false"]);
    run(&path, &["config", "core.splitIndex", "false"]);
    run(&path, &["mv", "tracked.txt", "aaaaaaa.txt"]);
    let cache = git::InspectionCache::default();
    cached_row(&f, &path, &cache);
    let index = PathBuf::from(run(
        &path,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    ));
    let before = fs::metadata(&index).unwrap();
    let before_modified = before.modified().unwrap();
    run(&path, &["mv", "aaaaaaa.txt", "bbbbbbb.txt"]);
    assert_eq!(
        fs::metadata(&index).unwrap().len(),
        before.len(),
        "the fixture must exercise equal-sized index versions"
    );
    fs::File::options()
        .write(true)
        .open(&index)
        .unwrap()
        .set_modified(before_modified)
        .unwrap();
    let new_time = SystemTime::now() + Duration::from_secs(7200);
    fs::File::options()
        .write(true)
        .open(path.join("bbbbbbb.txt"))
        .unwrap()
        .set_modified(new_time)
        .unwrap();
    let row = cached_row(&f, &path, &cache);
    assert_eq!(row.status.staged, 1);
    assert!(row.warnings.is_empty(), "{:?}", row.warnings);
    assert_eq!(
        row.updated_at.unwrap(),
        chrono::DateTime::<chrono::Utc>::from(new_time)
    );
}

#[test]
fn warm_commit_cache_refreshes_head_abbreviation_and_replacement_refs() {
    let f = Fixture::new();
    let path = f.add("feature");
    let cache = git::InspectionCache::default();
    let initial = cached_row(&f, &path, &cache);
    run(&path, &["config", "core.abbrev", "11"]);
    let row = cached_row(&f, &path, &cache);
    assert_eq!(row.short_head.unwrap().len(), 11);
    assert_eq!(row.head, initial.head);
    run(&path, &["config", "core.abbrev", "13"]);
    assert_eq!(cached_row(&f, &path, &cache).short_head.unwrap().len(), 13);

    run(
        &path,
        &["commit", "--allow-empty", "-m", "Replacement commit"],
    );
    let replacement = run(&path, &["rev-parse", "HEAD"]);
    let changed = cached_row(&f, &path, &cache);
    assert_ne!(changed.head, initial.head);
    assert_eq!(
        changed.commit_subject.as_deref(),
        Some("Replacement commit")
    );
    let original = initial.head.unwrap();
    run(&path, &["reset", "--hard", &original]);
    assert_eq!(
        cached_row(&f, &path, &cache).commit_subject.as_deref(),
        Some("Initial commit")
    );
    run(&path, &["replace", &original, &replacement]);
    let replaced = cached_row(&f, &path, &cache);
    assert_eq!(replaced.head.as_deref(), Some(original.as_str()));
    assert_eq!(
        replaced.commit_subject.as_deref(),
        Some("Replacement commit")
    );
    run(&path, &["replace", "-d", &original]);
    assert_eq!(
        cached_row(&f, &path, &cache).commit_subject.as_deref(),
        Some("Initial commit")
    );
}

#[test]
fn warm_index_cache_never_reuses_clean_status_after_corruption() {
    let f = Fixture::new();
    let path = f.add("feature");
    let cache = git::InspectionCache::default();
    let clean = cached_row(&f, &path, &cache);
    let index = PathBuf::from(run(
        &path,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    ));
    let bytes = fs::read(&index).unwrap();
    fs::write(&index, "invalid index").unwrap();
    let unknown = cached_row(&f, &path, &cache);
    assert_eq!(unknown.status.summary(), "unknown");
    assert!(
        unknown
            .warnings
            .iter()
            .any(|warning| warning.starts_with("last update:"))
    );
    assert!(git::remove(&clean, &RemoveOptions::default()).is_err());
    assert!(path.exists());
    fs::write(&index, bytes).unwrap();
    let recovered = cached_row(&f, &path, &cache);
    assert_eq!(recovered.status.summary(), "clean");
    assert!(recovered.warnings.is_empty(), "{:?}", recovered.warnings);
}

#[test]
fn fresh_status_preserves_incomplete_local_and_missing_upstream_diagnostics() {
    let f = Fixture::new();
    let path = f.add("feature");
    let cache = git::InspectionCache::default();
    assert!(cached_row(&f, &path, &cache).warnings.is_empty());
    run(&path, &["config", "branch.feature.remote", "."]);
    let partial = cached_row(&f, &path, &cache);
    assert!(
        partial
            .warnings
            .iter()
            .any(|warning| warning.contains("incomplete upstream"))
    );
    run(
        &path,
        &["config", "branch.feature.merge", "refs/heads/main"],
    );
    let local = cached_row(&f, &path, &cache);
    assert_eq!(local.upstream.as_deref(), Some("refs/heads/main"));
    assert_eq!((local.ahead, local.behind), (Some(0), Some(0)));
    run(
        &path,
        &["config", "branch.feature.merge", "refs/heads/missing"],
    );
    assert!(
        cached_row(&f, &path, &cache)
            .warnings
            .iter()
            .any(|warning| warning.starts_with("upstream:"))
    );
}

#[test]
fn refresh_repository_keeps_checkout_identity_and_updates_sanitized_remote() {
    let f = Fixture::new();
    let repo = git::repository(&f.repo).unwrap();
    assert_eq!(
        git::repository_common_dir(&f.repo).unwrap(),
        repo.common_dir
    );
    run(
        &f.repo,
        &[
            "remote",
            "add",
            "origin",
            "https://token@github.com/team/repo.git",
        ],
    );
    let refreshed = git::refresh_repository(&repo).unwrap();
    assert_eq!(refreshed.path, repo.path);
    assert_eq!(refreshed.common_dir, repo.common_dir);
    assert_eq!(
        refreshed.remote.as_deref(),
        Some("https://github.com/team/repo.git")
    );

    let path = f.root.join("separate-cache");
    let metadata = f.root.join("metadata-cache.git");
    fs::create_dir(&path).unwrap();
    run(
        &path,
        &[
            "init",
            "--initial-branch=main",
            "--separate-git-dir",
            metadata.to_str().unwrap(),
        ],
    );
    let repo = git::repository(&path).unwrap();
    assert_eq!(git::refresh_repository(&repo).unwrap().path, path);
}

#[test]
fn streaming_callback_receives_complete_rows_in_registration_order() {
    let f = Fixture::new();
    let path = f.add("feature");
    let repo = git::repository(&f.repo).unwrap();
    let cache = git::InspectionCache::default();
    let mut emitted = Vec::new();
    let rows = git::worktrees_filtered_cached(
        &repo,
        |_| true,
        &cache,
        |row| {
            assert!(row.commit_subject.is_some() && row.short_head.is_some());
            assert!(row.status.error.is_none());
            emitted.push((row.path.clone(), row.head.clone(), row.is_main));
        },
    )
    .unwrap();
    assert_eq!(emitted.len(), rows.len());
    assert_eq!(emitted[0].0, f.repo);
    assert!(emitted[0].2);
    assert_eq!(emitted[1].0, path);
    assert!(!emitted[1].2);
}
