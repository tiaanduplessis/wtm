//! Regression fixtures for marker traversal and repository changes during discovery.

#[cfg(unix)]
mod unix {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::{Path, PathBuf},
        process::{Command, Output},
    };

    use tempfile::TempDir;
    use worktree_manager::{model::ScanOptions, scan::scan};

    fn git(path: &Path, args: &[&str]) -> Output {
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
        output
    }

    fn commit(path: &Path) {
        git(path, &["config", "user.name", "Fixture"]);
        git(path, &["config", "user.email", "fixture@example.invalid"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        fs::write(path.join("tracked.txt"), "initial\n").unwrap();
        git(path, &["add", "tracked.txt"]);
        git(path, &["commit", "-m", "initial"]);
    }

    fn repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "--initial-branch=main"]);
        commit(path);
    }

    fn separate_repo(path: &Path, metadata: &Path) {
        fs::create_dir_all(path).unwrap();
        git(
            path,
            &[
                "init",
                "--initial-branch=main",
                "--separate-git-dir",
                metadata.to_str().unwrap(),
            ],
        );
        // Explicitly identify the primary checkout when a linked alias is used
        // after the primary checkout's .git pointer has changed repositories.
        git(path, &["config", "core.worktree", path.to_str().unwrap()]);
        commit(path);
    }

    fn temp() -> TempDir {
        TempDir::new_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
    }

    #[test]
    fn followed_file_symlink_head_identifies_bare_repo_without_following_external_directories() {
        let temp = temp();
        let source = temp.path().join("source");
        repo(&source);
        let root = temp.path().join("scan");
        fs::create_dir(&root).unwrap();
        let bare = root.join("nested.git");
        git(
            &root,
            &[
                "clone",
                "--bare",
                source.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let head = git(&bare, &["rev-parse", "HEAD"]).stdout;
        fs::write(bare.join("refs/heads/main"), &head).unwrap();
        let external_refs = temp.path().join("external-refs");
        fs::rename(bare.join("refs"), &external_refs).unwrap();
        symlink(&external_refs, bare.join("refs")).unwrap();
        fs::remove_file(bare.join("HEAD")).unwrap();
        // This legacy HEAD form is accepted by Git. A direct absolute HEAD
        // symlink is deliberately not used because Git rejects that form.
        symlink("refs/heads/main", bare.join("HEAD")).unwrap();
        assert_eq!(
            git(&bare, &["rev-parse", "--is-bare-repository"]).stdout,
            b"true\n"
        );
        assert!(!bare.join("HEAD").canonicalize().unwrap().starts_with(&root));
        let external_repo = temp.path().join("external-repository");
        repo(&external_repo);
        symlink(&external_repo, root.join("escape")).unwrap();

        for follow_links in [false, true] {
            let report = scan(&ScanOptions {
                root: root.clone(),
                follow_links,
                jobs: 1,
                ..ScanOptions::default()
            })
            .unwrap();
            assert!(report.discovery_complete, "{:?}", report.warnings);
            assert_eq!(report.repositories, 1);
            assert_eq!(report.worktrees.len(), 1);
            assert_eq!(report.worktrees[0].path, bare);
            assert!(report.worktrees[0].is_bare);
        }
    }

    fn real_git() -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("git"))
            .find(|candidate| candidate.is_file())
            .expect("Git fixture executable available on PATH")
            .canonicalize()
            .unwrap()
    }

    #[test]
    fn changed_primary_pointer_does_not_reserve_away_healthy_linked_owner() {
        let temp = temp();
        let owner = temp.path().join("scan");
        let metadata_a = temp.path().join("metadata-a");
        separate_repo(&owner, &metadata_a);
        let alias = owner.join("linked");
        git(
            &owner,
            &["worktree", "add", "-b", "linked", alias.to_str().unwrap()],
        );
        let replacement = temp.path().join("replacement");
        let metadata_b = temp.path().join("metadata-b");
        separate_repo(&replacement, &metadata_b);
        let wrappers = temp.path().join("bin");
        fs::create_dir(&wrappers).unwrap();
        let wrapper = wrappers.join("git");
        fs::write(
            &wrapper,
            r#"#!/bin/sh
if [ "$#" -eq 5 ] && [ "$1" = "-C" ] && [ "$2" = "$WTM_RACE_OWNER" ] && [ "$3" = "rev-parse" ] && [ "$5" = "--git-common-dir" ] && [ ! -e "$WTM_RACE_CHANGED" ]; then
    "$WTM_RACE_REAL_GIT" "$@" || exit $?
    printf 'gitdir: %s\n' "$WTM_RACE_REPLACEMENT" > "$WTM_RACE_OWNER/.git" || exit $?
    : > "$WTM_RACE_CHANGED"
    exit 0
fi
exec "$WTM_RACE_REAL_GIT" "$@"
"#,
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![wrappers];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let marker = temp.path().join("pointer-changed");
        let output = Command::new(env!("CARGO_BIN_EXE_wtm"))
            .args(["list", owner.to_str().unwrap(), "--json", "--jobs", "1"])
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("WTM_RACE_REAL_GIT", real_git())
            .env("WTM_RACE_OWNER", &owner)
            .env("WTM_RACE_REPLACEMENT", &metadata_b)
            .env("WTM_RACE_CHANGED", &marker)
            .output()
            .unwrap();
        assert!(marker.is_file(), "wrapper injected the repository change");
        assert_eq!(
            output.status.code(),
            Some(2),
            "race must report incomplete discovery, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["discovery_complete"], false);
        assert!(
            report["worktrees"].as_array().unwrap().iter().any(|row| {
                row["path"] == alias.to_str().unwrap()
                    && row["repo"]["common_dir"] == metadata_a.to_str().unwrap()
                    && row["status"]["error"].is_null()
            }),
            "healthy linked worktree must remain discoverable: {report:#}"
        );
        assert!(
            !report["warnings"].as_array().unwrap().is_empty(),
            "repository race must remain visible"
        );
    }
}
