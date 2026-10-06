use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use tempfile::TempDir;
use worktree_manager::{
    model::ScanOptions,
    scan::{ScanEvent, ScanSession, scan},
};

fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run Git fixture command");
    assert!(
        output.status.success(),
        "Git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "--initial-branch=main"]);
    git(path, &["config", "user.name", "Fixture"]);
    git(path, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(path.join("file.txt"), "initial\n").unwrap();
    git(path, &["add", "file.txt"]);
    git(path, &["commit", "-m", "initial"]);
}

fn worktree(repo: &Path, path: &Path, branch: &str) {
    git(
        repo,
        &["worktree", "add", "-b", branch, path.to_str().unwrap()],
    );
}

fn options(root: &Path) -> ScanOptions {
    ScanOptions {
        root: root.to_path_buf(),
        jobs: 2,
        ..ScanOptions::default()
    }
}

fn paths(root: &Path) -> Vec<PathBuf> {
    scan(&options(root))
        .unwrap()
        .worktrees
        .into_iter()
        .map(|row| row.path)
        .collect()
}

#[test]
fn scans_through_umbrella_checkout_hidden_and_ignored_directories() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = temp.path().join("umbrella");
    repo(&root);
    fs::write(root.join(".gitignore"), "repos/\n.worktrees/\n.hidden/\n").unwrap();
    let nested = root.join("repos/service");
    let hidden = root.join(".hidden/library");
    repo(&nested);
    repo(&hidden);
    let linked = root.join(".worktrees/service feature");
    worktree(&nested, &linked, "feature");
    let report = scan(&options(&root)).unwrap();
    assert_eq!(report.repositories, 3);
    for expected in [&root, &nested, &hidden, &linked] {
        assert!(
            report
                .worktrees
                .iter()
                .any(|row| row.path == expected.canonicalize().unwrap()),
            "missing {}",
            expected.display()
        );
    }
    let linked_row = report
        .worktrees
        .iter()
        .find(|row| row.path == linked)
        .unwrap();
    assert_eq!(
        linked_row.repo.common_dir,
        nested.join(".git").canonicalize().unwrap()
    );
    assert!(linked_row.short_head.is_some());
}

#[test]
fn excludes_external_registered_worktrees() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = temp.path().join("scan");
    let main = root.join("repo");
    repo(&main);
    let outside = temp.path().join("outside");
    worktree(&main, &outside, "outside");
    assert_eq!(paths(&root), vec![main.canonicalize().unwrap()]);
}

#[test]
fn linked_only_root_finds_repository_owned_outside_scope() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let owner = temp.path().join("owner");
    repo(&owner);
    let linked = temp.path().join("scan/linked");
    worktree(&owner, &linked, "linked");
    let report = scan(&options(&linked)).unwrap();
    assert_eq!(report.repositories, 1);
    assert_eq!(report.worktrees.len(), 1);
    assert_eq!(report.worktrees[0].path, linked.canonicalize().unwrap());
    assert_eq!(
        report.worktrees[0].repo.common_dir,
        owner.join(".git").canonicalize().unwrap()
    );
    assert!(!report.worktrees[0].is_main);
}

#[test]
fn bare_repository_does_not_hide_sibling_or_nested_checkout() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let bare = temp.path().join("archive.git");
    fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--bare", "--initial-branch=main"]);
    let nested = bare.join("projects/service");
    repo(&nested);
    let other = temp.path().join("other");
    repo(&other);
    let report = scan(&options(temp.path())).unwrap();
    assert_eq!(report.repositories, 3);
    assert!(
        report
            .worktrees
            .iter()
            .any(|row| row.path == bare && row.is_bare)
    );
    assert!(report.worktrees.iter().any(|row| row.path == nested));
}

#[test]
fn retains_detached_worktree_and_paths_with_spaces_and_newlines() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let main = temp.path().join("main repository");
    repo(&main);
    let linked = temp.path().join("linked\nwith spaces");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    let report = scan(&options(temp.path())).unwrap();
    let row = report
        .worktrees
        .iter()
        .find(|row| row.path == linked)
        .unwrap();
    assert!(row.branch.is_none());
    assert!(row.head.is_some());
    assert_eq!(report.worktrees.len(), 2);
}

#[test]
fn explicit_excludes_apply_to_discovery_and_registered_rows() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let main = temp.path().join("repo");
    repo(&main);
    let excluded = temp.path().join("skip/repo");
    repo(&excluded);
    let linked = temp.path().join("skip/linked");
    worktree(&main, &linked, "skipped");
    let mut opts = options(temp.path());
    opts.excludes.push("skip".into());
    let report = scan(&opts).unwrap();
    assert_eq!(report.repositories, 1);
    assert_eq!(report.worktrees.len(), 1);
    assert_eq!(report.worktrees[0].path, main);
}

#[test]
fn missing_registrations_are_scoped_to_root() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = temp.path().join("scan");
    let main = root.join("repo");
    repo(&main);
    let inside = root.join("missing");
    let outside = temp.path().join("missing-outside");
    worktree(&main, &inside, "inside");
    worktree(&main, &outside, "outside");
    fs::remove_dir_all(&inside).unwrap();
    fs::remove_dir_all(&outside).unwrap();
    let report = scan(&options(&root)).unwrap();
    assert!(
        report
            .worktrees
            .iter()
            .any(|row| row.path == inside && row.prunable.is_some())
    );
    assert!(!report.worktrees.iter().any(|row| row.path == outside));
    assert!(report.discovery_complete);
    assert!(
        !report.warnings.is_empty(),
        "known missing registration reports metadata warning"
    );
}

#[test]
fn malformed_git_pointer_is_reported_without_losing_valid_repositories() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let broken = temp.path().join("broken");
    fs::create_dir(&broken).unwrap();
    fs::write(
        broken.join(".git"),
        "gitdir: /nonexistent/worktree-manager-fixture\n",
    )
    .unwrap();
    let valid = temp.path().join("valid");
    repo(&valid);
    let report = scan(&options(temp.path())).unwrap();
    assert_eq!(report.worktrees.len(), 1);
    assert!(!report.discovery_complete);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("broken"))
    );
}

#[test]
fn rejects_missing_root_or_file() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    assert!(scan(&options(&temp.path().join("missing"))).is_err());
    let file = temp.path().join("file");
    fs::write(&file, "text").unwrap();
    assert!(scan(&options(&file)).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_cycles_and_escapes_are_safe_and_deduplicated() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let root = temp.path().join("scan");
    let main = root.join("repo");
    repo(&main);
    let external = temp.path().join("external");
    repo(&external);
    symlink(&main, root.join("alias")).unwrap();
    symlink(&root, main.join("cycle")).unwrap();
    symlink(&external, root.join("escape")).unwrap();
    let default_report = scan(&options(&root)).unwrap();
    assert_eq!(default_report.repositories, 1);
    assert_eq!(default_report.worktrees.len(), 1);
    let mut opts = options(&root);
    opts.follow_links = true;
    let followed = scan(&opts).unwrap();
    assert_eq!(followed.repositories, 1);
    assert_eq!(followed.worktrees.len(), 1);
    assert!(
        followed
            .warnings
            .iter()
            .any(|warning| warning.contains("loop"))
    );
}

#[test]
fn discovers_submodule_git_file_and_preserves_no_origin_repositories() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let source = temp.path().join("source");
    repo(&source);
    let root = temp.path().join("scan");
    repo(&root);
    git(
        &root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source.to_str().unwrap(),
            "modules/library",
        ],
    );
    let report = scan(&options(&root)).unwrap();
    assert_eq!(report.repositories, 2);
    assert!(
        report
            .worktrees
            .iter()
            .any(|row| row.path == root && row.repo.remote.is_none())
    );
    assert!(
        report
            .worktrees
            .iter()
            .any(|row| row.path == root.join("modules/library"))
    );
}

#[test]
fn ordinary_directory_resembling_bare_metadata_does_not_hide_nested_repository() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    fs::write(temp.path().join("HEAD"), "ordinary data").unwrap();
    fs::write(temp.path().join("config"), "ordinary data").unwrap();
    fs::create_dir(temp.path().join("objects")).unwrap();
    let nested = temp.path().join("modules/repo");
    repo(&nested);
    let report = scan(&options(temp.path())).unwrap();
    assert_eq!(report.repositories, 1);
    assert_eq!(report.worktrees.len(), 1);
    assert_eq!(report.worktrees[0].path, nested);
    assert!(!report.discovery_complete);
}

#[test]
fn streaming_reports_rows_before_completion_and_profiles_git_costs() {
    use std::sync::Mutex;
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    repo(&temp.path().join("one"));
    repo(&temp.path().join("two"));
    let rows = Mutex::new(Vec::new());
    let report = ScanSession::default()
        .scan(&options(temp.path()), |event| {
            if let ScanEvent::Rows { worktrees, .. } = event {
                rows.lock()
                    .unwrap()
                    .extend(worktrees.into_iter().map(|row| row.path));
            }
        })
        .unwrap();
    let mut streamed = rows.into_inner().unwrap();
    streamed.sort();
    let mut final_paths: Vec<_> = report
        .worktrees
        .iter()
        .map(|row| row.path.clone())
        .collect();
    final_paths.sort();
    assert_eq!(streamed, final_paths);
    assert!(report.profile.first_result_ms.is_some());
    assert!(report.profile.first_result_ms.unwrap() <= report.elapsed_ms);
    assert!(report.profile.directories >= 3);
    assert_eq!(
        report.profile.git_commands,
        report
            .profile
            .commands
            .values()
            .map(|cost| cost.calls)
            .sum::<usize>()
    );
    assert_eq!(report.profile.commands["status"].calls, 2);
}

#[test]
fn incremental_refresh_reuses_immutable_metadata_but_reads_file_changes() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let owner = temp.path().join("owner");
    repo(&owner);
    let opts = options(temp.path());
    let session = ScanSession::default();
    let original = session.scan(&opts, |_| {}).unwrap();
    let expected_update = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    fs::File::open(owner.join("file.txt"))
        .unwrap()
        .set_modified(expected_update)
        .unwrap();
    let refreshed = session.refresh(&opts, None, |_| {}).unwrap();
    assert!(!refreshed.worktrees[0].status.is_dirty());
    assert!(refreshed.worktrees[0].updated_at > original.worktrees[0].updated_at);
    assert!(refreshed.profile.commit_cache_hits > 0);
    assert!(refreshed.profile.file_list_cache_hits > 0);
    assert_eq!(refreshed.profile.directories, 0);
    fs::write(owner.join("new.txt"), "untracked").unwrap();
    let dirty = session.refresh(&opts, None, |_| {}).unwrap();
    assert_eq!(dirty.worktrees[0].status.untracked, 1);
    git(&owner, &["add", "new.txt"]);
    let staged = session.refresh(&opts, None, |_| {}).unwrap();
    assert_eq!(staged.worktrees[0].status.staged, 1);
    assert_eq!(staged.worktrees[0].status.untracked, 0);
}

#[test]
fn refresh_discovers_new_registrations_and_full_rescan_discovers_new_owners() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let owner = temp.path().join("owner");
    repo(&owner);
    let opts = options(temp.path());
    let session = ScanSession::default();
    session.scan(&opts, |_| {}).unwrap();
    let linked = temp.path().join("linked");
    worktree(&owner, &linked, "topic");
    let nested = owner.join("hidden/nested");
    repo(&nested);
    let refreshed = session.refresh(&opts, None, |_| {}).unwrap();
    assert_eq!(refreshed.repositories, 1);
    assert!(refreshed.worktrees.iter().any(|row| row.path == linked));
    let rescanned = session.scan(&opts, |_| {}).unwrap();
    assert_eq!(rescanned.repositories, 2);
    assert!(rescanned.worktrees.iter().any(|row| row.path == nested));
}

#[test]
fn targeted_refresh_preserves_other_inventory_and_failed_owners_become_unknown() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let first = temp.path().join("one");
    let second = temp.path().join("two");
    repo(&first);
    repo(&second);
    let opts = options(temp.path());
    let session = ScanSession::default();
    let original = session.scan(&opts, |_| {}).unwrap();
    fs::write(first.join("new"), "one").unwrap();
    fs::write(second.join("new"), "two").unwrap();
    let owner = original
        .worktrees
        .iter()
        .find(|row| row.path == first)
        .unwrap()
        .repo
        .common_dir
        .clone();
    let refreshed = session.refresh(&opts, Some(&owner), |_| {}).unwrap();
    assert_eq!(refreshed.repositories, 2);
    assert_eq!(
        refreshed
            .worktrees
            .iter()
            .find(|row| row.path == first)
            .unwrap()
            .status
            .untracked,
        1
    );
    assert_eq!(
        refreshed
            .worktrees
            .iter()
            .find(|row| row.path == second)
            .unwrap()
            .status
            .untracked,
        0
    );
    assert_eq!(refreshed.profile.commands["status"].calls, 1);
    fs::remove_dir_all(second.join(".git")).unwrap();
    let failed = session.refresh(&opts, None, |_| {}).unwrap();
    assert!(!failed.discovery_complete);
    assert_eq!(failed.pending_repositories, vec![second.join(".git")]);
    assert!(
        failed
            .worktrees
            .iter()
            .find(|row| row.path == second)
            .unwrap()
            .status
            .error
            .is_some()
    );
    let rescanned = session.scan(&opts, |_| {}).unwrap();
    assert!(rescanned.discovery_complete);
    assert_eq!(rescanned.repositories, 1);
}

#[test]
fn changed_scan_scope_forces_full_discovery() {
    let temp = TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    repo(&temp.path().join("one"));
    repo(&temp.path().join("skip/two"));
    let session = ScanSession::default();
    let mut opts = options(temp.path());
    let full = session.scan(&opts, |_| {}).unwrap();
    assert_eq!(full.repositories, 2);
    opts.excludes.push("skip".into());
    let scoped = session.refresh(&opts, None, |_| {}).unwrap();
    assert_eq!(scoped.repositories, 1);
    assert!(scoped.profile.directories > 0);
}
