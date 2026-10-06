//! Git-backed metadata and management. Paths are passed as native arguments, never through a shell.

use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use walkdir::WalkDir;

use crate::model::{Repository, Worktree, WorktreeStatus};

/// Require NUL-delimited worktree listings, introduced in Git 2.36.
pub fn check_version() -> Result<()> {
    let version = read_line(Path::new("."), &["--version"])?;
    if !supported_version(&version) {
        bail!("Git 2.36 or newer is required; found {version}");
    }
    Ok(())
}

fn supported_version(value: &str) -> bool {
    let Some(version) = value.strip_prefix("git version ") else {
        return false;
    };
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
    matches!((major, minor), (Some(major), Some(minor)) if major > 2 || (major == 2 && minor >= 36))
}

/// A bounded, in-memory inspection cache shared across scans in one session.
/// Working status, registry entries, references and file mtimes are never cached.
#[derive(Debug, Clone, Default)]
pub struct InspectionCache(Arc<Mutex<CacheState>>);

#[derive(Debug, Default)]
struct CacheState {
    commits: HashMap<CommitKey, CommitEntry>,
    files: HashMap<PathBuf, FileEntry>,
    sequence: u64,
    commit_bytes: usize,
    file_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CommitKey {
    common_dir: PathBuf,
    head: String,
}

#[derive(Debug, Clone)]
struct CommitMetadata {
    subject: String,
    committed_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct CommitEntry {
    metadata: CommitMetadata,
    sequence: u64,
    bytes: usize,
}

#[derive(Debug)]
struct FileEntry {
    fingerprint: Option<IndexFingerprint>,
    paths: Arc<Vec<PathBuf>>,
    sequence: u64,
    bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexFingerprint {
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(unix)]
    inode: (u64, u64),
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

impl InspectionCache {
    fn commit(&self, key: &CommitKey) -> Option<CommitMetadata> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.sequence = state.sequence.wrapping_add(1);
        let sequence = state.sequence;
        let entry = state.commits.get_mut(key)?;
        entry.sequence = sequence;
        crate::profile::commit_hit();
        Some(entry.metadata.clone())
    }

    fn insert_commit(&self, key: CommitKey, metadata: CommitMetadata) {
        const MAX_ENTRIES: usize = 4096;
        const MAX_BYTES: usize = 16 * 1024 * 1024;
        let bytes =
            metadata.subject.len() + key.common_dir.as_os_str().len() + key.head.len() + 256;
        if bytes > MAX_BYTES {
            return;
        }
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.sequence = state.sequence.wrapping_add(1);
        let sequence = state.sequence;
        if let Some(old) = state.commits.insert(
            key,
            CommitEntry {
                metadata,
                sequence,
                bytes,
            },
        ) {
            state.commit_bytes -= old.bytes;
        }
        state.commit_bytes += bytes;
        while state.commits.len() > MAX_ENTRIES || state.commit_bytes > MAX_BYTES {
            let key = state
                .commits
                .iter()
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(key, _)| key.clone())
                .expect("nonempty bounded commit cache");
            if let Some(old) = state.commits.remove(&key) {
                state.commit_bytes -= old.bytes;
            }
        }
    }

    fn files(
        &self,
        git_dir: &Path,
        fingerprint: &Option<IndexFingerprint>,
    ) -> Option<Arc<Vec<PathBuf>>> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.sequence = state.sequence.wrapping_add(1);
        let sequence = state.sequence;
        let entry = state.files.get_mut(git_dir)?;
        if &entry.fingerprint != fingerprint {
            return None;
        }
        entry.sequence = sequence;
        Some(Arc::clone(&entry.paths))
    }

    fn forget_files(&self, git_dir: &Path) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(old) = state.files.remove(git_dir) {
            state.file_bytes -= old.bytes;
        }
    }

    fn insert_files(
        &self,
        git_dir: PathBuf,
        fingerprint: Option<IndexFingerprint>,
        paths: Arc<Vec<PathBuf>>,
    ) {
        const MAX_ENTRIES: usize = 256;
        const MAX_BYTES: usize = 32 * 1024 * 1024;
        let bytes = paths
            .iter()
            .map(|path| path.as_os_str().len() + std::mem::size_of::<PathBuf>())
            .sum::<usize>()
            + git_dir.as_os_str().len()
            + 128;
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(old) = state.files.remove(&git_dir) {
            state.file_bytes -= old.bytes;
        }
        if bytes > MAX_BYTES {
            return;
        }
        state.sequence = state.sequence.wrapping_add(1);
        let sequence = state.sequence;
        state.files.insert(
            git_dir,
            FileEntry {
                fingerprint,
                paths,
                sequence,
                bytes,
            },
        );
        state.file_bytes += bytes;
        while state.files.len() > MAX_ENTRIES || state.file_bytes > MAX_BYTES {
            let key = state
                .files
                .iter()
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(key, _)| key.clone())
                .expect("nonempty bounded file cache");
            if let Some(old) = state.files.remove(&key) {
                state.file_bytes -= old.bytes;
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AddOptions {
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub new_branch: bool,
    pub start_point: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RemoveOptions {
    /// Explicitly allow Git to discard the selected worktree's uncommitted files.
    pub force: bool,
    /// Delete the branch with `git branch -d`, which still requires Git's merge check.
    pub delete_branch: bool,
}

fn command(path: &Path, write: bool) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(path);
    command.stdin(Stdio::null());
    // A terminal launched inside another worktree can carry Git's repository overrides.
    // None may select a different repository, index, object store, or configuration here.
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command.env("GIT_TERMINAL_PROMPT", "0");
    if !write {
        command.env("GIT_OPTIONAL_LOCKS", "0");
    }
    command
}

fn output<I, S>(path: &Path, args: I, write: bool) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect();
    let started = Instant::now();
    let result = command(path, write)
        .args(&args)
        .output()
        .with_context(|| format!("could not run Git in {}", path.display()));
    let mut named = args.iter();
    let verb = loop {
        match named.next() {
            Some(arg) if arg == "-c" => {
                named.next();
            }
            Some(arg) if arg == "--no-replace-objects" => {}
            verb => break verb,
        }
    };
    crate::profile::git(
        &verb.map_or_else(|| "unknown".into(), |arg| arg.to_string_lossy()),
        started.elapsed(),
    );
    result
}

fn checked<I, S>(path: &Path, args: I, write: bool) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let result = output(path, args, write)?;
    if !result.status.success() {
        bail!("{}", git_error(&result));
    }
    Ok(result.stdout)
}

fn git_error(result: &Output) -> String {
    let stderr = String::from_utf8_lossy(&result.stderr);
    let stdout = String::from_utf8_lossy(&result.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    redact_url_credentials(&format!("Git exited with {}: {detail}", result.status))
}

fn line(bytes: Vec<u8>) -> Result<String> {
    Ok(String::from_utf8(bytes)?
        .trim_end_matches(['\r', '\n'])
        .into())
}

fn read_line(path: &Path, args: &[&str]) -> Result<String> {
    line(checked(path, args, false)?)
}

#[cfg(unix)]
fn native_path(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn native_path(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(std::str::from_utf8(bytes)?))
}

fn path_line(bytes: Vec<u8>) -> Result<PathBuf> {
    // Git emits one terminating newline; a pathname itself may end in a newline.
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    native_path(bytes)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    // Keep `..` semantics when an ancestor is a symlink; lexical collapse could
    // otherwise create or move a worktree in a different directory than requested.
    std::path::absolute(path).context("cannot resolve the requested path")
}

fn identity(path: &Path) -> Result<PathBuf> {
    Ok(fs::canonicalize(path).unwrap_or(absolute(path)?))
}

fn common_dir(path: &Path) -> Result<PathBuf> {
    identity(&path_line(checked(
        path,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        false,
    )?)?)
}

/// Read only the repository's shared identity, before collecting expensive metadata.
pub fn repository_common_dir(path: &Path) -> Result<PathBuf> {
    common_dir(path)
}

/// Refresh mutable repository metadata without losing a verified separate checkout root.
pub fn refresh_repository(repo: &Repository) -> Result<Repository> {
    validate_repository(repo)?;
    let records = registry_for(repo)?;
    let primary = records
        .first()
        .context("Git returned no registered worktrees")?;
    let mut fresh = repo.clone();
    fresh.path = primary.path.clone();
    fresh.name = fresh
        .path
        .file_name()
        .unwrap_or(fresh.path.as_os_str())
        .to_string_lossy()
        .trim_end_matches(".git")
        .into();
    fresh.remote = remote(&fresh.common_dir)?;
    Ok(fresh)
}

/// Identify the shared repository even when called from one of its linked worktrees.
pub fn repository(path: &Path) -> Result<Repository> {
    let common_dir = common_dir(path)?;
    let mut records = registry(&common_dir)?;
    // For `init --separate-git-dir`, the registry may name the metadata directory.
    // Discovery in its primary checkout provides the authoritative working root.
    if records.first().is_some_and(|record| !record.bare) {
        let git_dir = path_line(checked(
            path,
            ["rev-parse", "--path-format=absolute", "--git-dir"],
            false,
        )?)?;
        if identity(&git_dir)? == common_dir
            && let Ok(root) =
                checked(path, ["rev-parse", "--show-toplevel"], false).and_then(path_line)
        {
            records[0].path = root;
        }
    }
    let primary = records
        .first()
        .context("Git returned no registered worktrees")?;
    let repo_path = primary.path.clone();
    let name = repo_path
        .file_name()
        .unwrap_or(repo_path.as_os_str())
        .to_string_lossy()
        .trim_end_matches(".git")
        .to_owned();
    let remote = remote(&common_dir)?;
    Ok(Repository {
        common_dir,
        path: repo_path,
        name,
        remote,
    })
}

fn remote(path: &Path) -> Result<Option<String>> {
    let names = read_line(path, &["remote"])?;
    let name = if names.lines().any(|name| name == "origin") {
        Some("origin")
    } else {
        names.lines().next()
    };
    match name {
        Some(name) => Ok(Some(redact_url_credentials(&read_line(
            path,
            &["remote", "get-url", name],
        )?))),
        None => Ok(None),
    }
}

#[derive(Debug)]
struct Record {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    bare: bool,
    locked: Option<String>,
    prunable: Option<String>,
    warnings: Vec<String>,
}

fn registry(path: &Path) -> Result<Vec<Record>> {
    let bytes = checked(path, ["worktree", "list", "--porcelain", "-z"], false)?;
    let mut records = parse_registry(&bytes)?;
    // Git's registry reports a submodule's metadata directory as its primary path.
    // core.worktree resolves to the actual checkout; preserve other registrations.
    if records.first().is_some_and(|record| !record.bare) {
        let configured = output(path, ["config", "--get", "core.worktree"], false)?;
        if configured.status.success() {
            records[0].path = path_line(checked(path, ["rev-parse", "--show-toplevel"], false)?)?;
        } else if configured.status.code() != Some(1) {
            bail!("{}", git_error(&configured));
        }
    }
    Ok(records)
}

fn registry_for(repo: &Repository) -> Result<Vec<Record>> {
    let mut records = registry(&repo.common_dir)?;
    if let Some(primary) = records.first_mut()
        && !primary.bare
        && identity(&primary.path)? == identity(&repo.common_dir)?
    {
        primary.path = repo.path.clone();
    }
    Ok(records)
}

fn parse_registry(bytes: &[u8]) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    let mut current: Option<Record> = None;
    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(record) = current.take() {
                records.push(record);
            }
            continue;
        }
        if let Some(path) = field.strip_prefix(b"worktree ") {
            if let Some(record) = current.take() {
                records.push(record);
            }
            current = Some(Record {
                path: native_path(path)?,
                head: None,
                branch: None,
                bare: false,
                locked: None,
                prunable: None,
                warnings: Vec::new(),
            });
            continue;
        }
        let record = current.as_mut().context("invalid Git worktree registry")?;
        if let Some(head) = field.strip_prefix(b"HEAD ") {
            let head = std::str::from_utf8(head)?;
            if !head.bytes().all(|byte| byte == b'0') {
                record.head = Some(head.into());
            }
        } else if let Some(branch) = field.strip_prefix(b"branch refs/heads/") {
            match std::str::from_utf8(branch) {
                Ok(branch) => record.branch = Some(branch.into()),
                Err(_) => record
                    .warnings
                    .push("branch name is not UTF-8; branch management is unavailable".into()),
            }
        } else if field == b"bare" {
            record.bare = true;
        } else if field == b"locked" || field.starts_with(b"locked ") {
            record.locked =
                Some(String::from_utf8_lossy(field.strip_prefix(b"locked ").unwrap_or(b"")).into());
        } else if field == b"prunable" || field.starts_with(b"prunable ") {
            record.prunable = Some(
                String::from_utf8_lossy(field.strip_prefix(b"prunable ").unwrap_or(b"")).into(),
            );
        }
    }
    if let Some(record) = current {
        records.push(record);
    }
    Ok(records)
}

/// Return every Git registration, including locked, bare, detached, and missing worktrees.
pub fn worktrees(repo: &Repository) -> Result<Vec<Worktree>> {
    worktrees_filtered(repo, |_| true)
}

/// Filter registration paths before inspecting their files and working status.
/// Original registration positions still determine the primary worktree.
pub fn worktrees_filtered(
    repo: &Repository,
    include: impl FnMut(&Path) -> bool,
) -> Result<Vec<Worktree>> {
    worktrees_filtered_cached(repo, include, &InspectionCache::default(), |_| {})
}

/// Inspect fresh mutable state, reuse only immutable metadata and index-derived paths,
/// and emit each complete row as soon as it is available.
pub fn worktrees_filtered_cached(
    repo: &Repository,
    mut include: impl FnMut(&Path) -> bool,
    cache: &InspectionCache,
    mut callback: impl FnMut(&Worktree),
) -> Result<Vec<Worktree>> {
    validate_repository(repo)?;
    let records: Vec<_> = registry_for(repo)?
        .into_iter()
        .enumerate()
        .filter(|(_, record)| include(&record.path))
        .collect();
    let context = InspectionContext {
        default: default_ref(&repo.common_dir),
        abbreviations: abbreviations(repo, records.iter().map(|(_, record)| record)),
        replacements: replacement_refs(repo),
    };
    let mut rows = Vec::with_capacity(records.len());
    for (index, record) in records {
        let row = enrich_cached(repo, record, index == 0, &context, cache)?;
        callback(&row);
        rows.push(row);
    }
    Ok(rows)
}

struct InspectionContext {
    default: Result<Option<String>>,
    abbreviations: Result<HashMap<String, String>>,
    replacements: Result<Arc<[u8]>>,
}

fn replacement_refs(repo: &Repository) -> Result<Arc<[u8]>> {
    let bytes = checked(
        &repo.common_dir,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            "refs/replace",
        ],
        false,
    )?;
    Ok(Arc::from(bytes))
}

fn abbreviations<'a>(
    repo: &Repository,
    records: impl Iterator<Item = &'a Record>,
) -> Result<HashMap<String, String>> {
    let mut seen = HashSet::new();
    let heads: Vec<_> = records
        .filter_map(|record| record.head.as_ref())
        .filter(|head| seen.insert((*head).clone()))
        .collect();
    let mut result = HashMap::with_capacity(heads.len());
    for chunk in heads.chunks(256) {
        let mut args: Vec<&str> = vec!["rev-list", "--no-walk=unsorted", "--abbrev-commit"];
        args.extend(chunk.iter().map(|head| head.as_str()));
        args.push("--");
        let short_heads = read_line(&repo.common_dir, &args)?;
        let short_heads: Vec<_> = short_heads.lines().collect();
        if short_heads.len() != chunk.len() {
            bail!("Git abbreviation output did not match the requested commits");
        }
        for (head, short) in chunk.iter().zip(short_heads) {
            if !head.starts_with(short) {
                bail!("Git abbreviation output does not match the requested commit");
            }
            result.insert((*head).clone(), short.to_owned());
        }
    }
    Ok(result)
}

fn default_ref(path: &Path) -> Result<Option<String>> {
    let remotes = read_line(path, &["remote"])?;
    let mut names: Vec<&str> = remotes.lines().collect();
    names.sort_by_key(|name| *name != "origin");
    for name in names {
        let reference = format!("refs/remotes/{name}/HEAD");
        let result = output(path, ["symbolic-ref", "--quiet", &reference], false)?;
        if result.status.success() {
            return Ok(Some(line(result.stdout)?));
        }
        if result.status.code() != Some(1) {
            bail!("{}", git_error(&result));
        }
    }
    for reference in ["refs/heads/main", "refs/heads/master"] {
        let result = output(path, ["show-ref", "--verify", "--quiet", reference], false)?;
        if result.status.success() {
            return Ok(Some(reference.into()));
        }
        if result.status.code() != Some(1) {
            bail!("{}", git_error(&result));
        }
    }
    Ok(None)
}

fn enrich(
    repo: &Repository,
    record: Record,
    is_main: bool,
    default: &Result<Option<String>>,
) -> Result<Worktree> {
    let context = InspectionContext {
        default: match default {
            Ok(value) => Ok(value.clone()),
            Err(error) => Err(anyhow::anyhow!("{error}")),
        },
        abbreviations: Ok(HashMap::new()),
        replacements: replacement_refs(repo),
    };
    enrich_cached(repo, record, is_main, &context, &InspectionCache::default())
}

fn enrich_cached(
    repo: &Repository,
    mut record: Record,
    is_main: bool,
    context: &InspectionContext,
    cache: &InspectionCache,
) -> Result<Worktree> {
    if record.bare {
        let result = output(
            &repo.common_dir,
            ["rev-parse", "--verify", "--quiet", "HEAD"],
            false,
        )?;
        if result.status.success() {
            record.head = Some(line(result.stdout)?);
        } else if result.status.code() != Some(1) {
            record
                .warnings
                .push(format!("bare HEAD: {}", git_error(&result)));
        }
        let result = output(&repo.common_dir, ["symbolic-ref", "--quiet", "HEAD"], false)?;
        if result.status.success() {
            record.branch = Some(
                line(result.stdout)?
                    .trim_start_matches("refs/heads/")
                    .into(),
            );
        } else if result.status.code() != Some(1) {
            record
                .warnings
                .push(format!("bare branch: {}", git_error(&result)));
        }
    }
    let mut worktree = Worktree {
        repo: repo.clone(),
        path: record.path,
        branch: record.branch,
        head: record.head,
        short_head: None,
        commit_subject: None,
        committed_at: None,
        updated_at: None,
        status: WorktreeStatus::default(),
        locked: record.locked,
        prunable: record.prunable,
        is_main,
        is_bare: record.bare,
        commit_url: None,
        upstream: None,
        ahead: None,
        behind: None,
        merged: None,
        warnings: record.warnings,
    };
    if !worktree.is_bare {
        match inspect_path(&worktree) {
            Ok(paths) => {
                let snapshot = match status(&worktree.path) {
                    Ok(snapshot) => {
                        if snapshot.head != worktree.head || snapshot.branch != worktree.branch {
                            worktree.warnings.push("HEAD changed during inspection; refresh before managing this worktree".into());
                        }
                        worktree.head = snapshot.head.clone();
                        worktree.branch = snapshot.branch.clone();
                        worktree.status = snapshot.status.clone();
                        inspect_upstream(&mut worktree, &paths.config, &snapshot);
                        Some(snapshot)
                    }
                    Err(error) => {
                        worktree.status.error = Some(error.to_string());
                        None
                    }
                };
                let activity_started = Instant::now();
                let activity = updated_at(
                    &worktree,
                    &paths.git_dir,
                    snapshot.as_ref().map(|status| status.untracked.as_slice()),
                    cache,
                );
                crate::profile::activity(activity_started.elapsed());
                match activity {
                    Ok(time) => worktree.updated_at = time,
                    Err(error) => worktree.warnings.push(format!("last update: {error}")),
                }
            }
            Err(error) => worktree.status.error = Some(error.to_string()),
        }
    } else {
        let mut latest = None;
        match include_git_mtimes(
            &repo.common_dir,
            &repo.common_dir,
            worktree.branch.as_deref(),
            &mut latest,
        ) {
            Ok(()) => worktree.updated_at = latest.map(DateTime::<Utc>::from),
            Err(error) => worktree.warnings.push(format!("last update: {error}")),
        }
    }
    if let Some(head) = &worktree.head {
        let cached_key = context
            .replacements
            .as_ref()
            .ok()
            .filter(|replacements| replacements.is_empty())
            .map(|_| CommitKey {
                common_dir: repo.common_dir.clone(),
                head: head.clone(),
            });
        if let Err(error) = &context.replacements {
            worktree
                .warnings
                .push(format!("commit cache unavailable: {error}"));
        }
        match commit_metadata(repo, head, cached_key, cache) {
            Ok(metadata) => {
                worktree.commit_subject = Some(metadata.subject);
                worktree.committed_at = metadata.committed_at;
                if worktree.committed_at.is_none() {
                    worktree
                        .warnings
                        .push("commit timestamp is unavailable".into());
                }
            }
            Err(error) => worktree.warnings.push(format!("commit metadata: {error}")),
        }
        let short = match &context.abbreviations {
            Ok(short) => short.get(head).cloned(),
            Err(error) => {
                worktree.warnings.push(format!("short hash batch: {error}"));
                None
            }
        };
        worktree.short_head = match short {
            Some(short) => Some(short),
            None => match read_line(&repo.common_dir, &["rev-parse", "--short", head]) {
                Ok(short) => Some(short),
                Err(error) => {
                    worktree.warnings.push(format!("short hash: {error}"));
                    None
                }
            },
        };
        worktree.commit_url = repo
            .remote
            .as_deref()
            .and_then(|remote| commit_url(remote, head));
        match &context.default {
            Ok(Some(reference)) => match output(
                &repo.common_dir,
                ["merge-base", "--is-ancestor", head, reference],
                false,
            ) {
                Ok(result) if result.status.success() => worktree.merged = Some(true),
                Ok(result) if result.status.code() == Some(1) => worktree.merged = Some(false),
                Ok(result) => worktree
                    .warnings
                    .push(format!("merge status: {}", git_error(&result))),
                Err(error) => worktree.warnings.push(format!("merge status: {error}")),
            },
            Ok(None) => {}
            Err(error) => worktree.warnings.push(format!("default branch: {error}")),
        }
    }
    Ok(worktree)
}

fn commit_metadata(
    repo: &Repository,
    head: &str,
    key: Option<CommitKey>,
    cache: &InspectionCache,
) -> Result<CommitMetadata> {
    if let Some(key) = &key
        && let Some(metadata) = cache.commit(key)
    {
        return Ok(metadata);
    }
    let mut args = Vec::new();
    if key.is_some() {
        // Never cache replacement-dependent output under the immutable original SHA.
        // This also prevents a replacement added during inspection from poisoning it.
        args.push("--no-replace-objects");
    }
    args.extend([
        "-c",
        "i18n.logOutputEncoding=UTF-8",
        "show",
        "-s",
        "--format=%ct%x00%s",
        head,
        "--",
    ]);
    let raw = read_line(&repo.common_dir, &args)?;
    let (time, subject) = raw
        .split_once('\0')
        .context("Git omitted commit metadata fields")?;
    let metadata = CommitMetadata {
        subject: subject.into(),
        committed_at: time
            .parse()
            .ok()
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0)),
    };
    if let Some(key) = key {
        cache.insert_commit(key, metadata.clone());
    }
    Ok(metadata)
}

#[derive(Default)]
struct RepoConfig(HashMap<Vec<u8>, Vec<u8>>);

impl RepoConfig {
    fn read(path: &Path) -> Result<Self> {
        let result = output(
            path,
            [
                "config",
                "--null",
                "--get-regexp",
                r"^(core\.worktree|branch\..*\.(remote|merge))$",
            ],
            false,
        )?;
        if !result.status.success() && result.status.code() != Some(1) {
            bail!("{}", git_error(&result));
        }
        let mut values = HashMap::new();
        for record in result
            .stdout
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
        {
            let separator = record
                .iter()
                .position(|byte| *byte == b'\n')
                .context("invalid Git configuration output")?;
            values.insert(
                record[..separator].to_vec(),
                record[separator + 1..].to_vec(),
            );
        }
        Ok(Self(values))
    }

    fn value(&self, key: &str) -> Option<&[u8]> {
        self.0.get(key.as_bytes()).map(Vec::as_slice)
    }
}

struct WorktreePaths {
    git_dir: PathBuf,
    config: RepoConfig,
}

/// One unambiguous Git path plus native markers avoids newline-delimited multi-path output.
fn inspect_path(worktree: &Worktree) -> Result<WorktreePaths> {
    if !worktree.path.is_dir() {
        bail!("worktree directory is missing: {}", worktree.path.display());
    }
    let git_dir = identity(&path_line(checked(
        &worktree.path,
        ["rev-parse", "--path-format=absolute", "--git-dir"],
        false,
    )?)?)?;
    let marker = worktree.path.join(".git");
    let declared = if marker.is_dir() {
        identity(&marker)?
    } else {
        let bytes = fs::read(&marker)
            .with_context(|| format!("cannot read worktree marker {}", marker.display()))?;
        let path = path_line(
            bytes
                .strip_prefix(b"gitdir: ")
                .context("invalid worktree .git marker")?
                .to_vec(),
        )?;
        identity(&if path.is_absolute() {
            path
        } else {
            worktree.path.join(path)
        })?
    };
    if declared != git_dir {
        bail!("worktree metadata identity changed; refresh before continuing");
    }
    let common = match fs::read(git_dir.join("commondir")) {
        Ok(bytes) => {
            let path = path_line(bytes)?;
            identity(&if path.is_absolute() {
                path
            } else {
                git_dir.join(path)
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
        Err(error) => return Err(error).context("cannot read shared Git directory marker"),
    };
    if common != identity(&worktree.repo.common_dir)? {
        bail!("worktree path now belongs to another repository; refresh before continuing");
    }
    if common != git_dir {
        let pointer = path_line(
            fs::read(git_dir.join("gitdir")).context("cannot read linked worktree backpointer")?,
        )?;
        let pointer = if pointer.is_absolute() {
            pointer
        } else {
            git_dir.join(pointer)
        };
        if identity(&pointer)? != identity(&marker)? {
            bail!("worktree registration points to another directory; refresh before continuing");
        }
        if pointer.parent().map(identity).transpose()? != Some(identity(&worktree.path)?) {
            bail!(
                "worktree metadata belongs to another registered root; refresh before continuing"
            );
        }
    }
    let config = RepoConfig::read(&worktree.path)?;
    if config.value("core.worktree").is_some() {
        let root = path_line(checked(
            &worktree.path,
            ["rev-parse", "--show-toplevel"],
            false,
        )?)?;
        if identity(&root)? != identity(&worktree.path)? {
            bail!("worktree path is no longer a worktree root; refresh before continuing");
        }
    }
    Ok(WorktreePaths { git_dir, config })
}

#[derive(Default)]
struct StatusSnapshot {
    status: WorktreeStatus,
    untracked: Vec<PathBuf>,
    head: Option<String>,
    branch: Option<String>,
    upstream: Option<String>,
    divergence: Option<(usize, usize)>,
}

fn status(path: &Path) -> Result<StatusSnapshot> {
    parse_status(&checked(
        path,
        [
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        false,
    )?)
}

fn parse_status(bytes: &[u8]) -> Result<StatusSnapshot> {
    let mut result = StatusSnapshot::default();
    let mut head_seen = false;
    let mut branch_seen = false;
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        if let Some(head) = field.strip_prefix(b"# branch.oid ") {
            head_seen = true;
            if head != b"(initial)" {
                result.head = Some(std::str::from_utf8(head)?.into());
            }
        } else if let Some(branch) = field.strip_prefix(b"# branch.head ") {
            branch_seen = true;
            if branch != b"(detached)" {
                result.branch = std::str::from_utf8(branch).ok().map(str::to_owned);
            }
        } else if let Some(upstream) = field.strip_prefix(b"# branch.upstream ") {
            result.upstream = Some(std::str::from_utf8(upstream)?.into());
        } else if let Some(counts) = field.strip_prefix(b"# branch.ab ") {
            let counts = std::str::from_utf8(counts)?;
            let (ahead, behind) = counts
                .split_once(' ')
                .context("invalid Git branch divergence")?;
            result.divergence = Some((
                ahead
                    .strip_prefix('+')
                    .context("invalid ahead count")?
                    .parse()?,
                behind
                    .strip_prefix('-')
                    .context("invalid behind count")?
                    .parse()?,
            ));
        } else if field.starts_with(b"# ") || field.starts_with(b"! ") {
            continue;
        } else if let Some(path) = field.strip_prefix(b"? ") {
            result.status.untracked += 1;
            result.untracked.push(native_path(path)?);
        } else {
            let (xy, submodule, renamed, conflict) = match field.first() {
                Some(b'1') => {
                    let parts: Vec<_> = field.splitn(9, |byte| *byte == b' ').collect();
                    if parts.len() != 9 {
                        bail!("invalid ordinary Git status record");
                    }
                    (parts[1], parts[2], false, false)
                }
                Some(b'2') => {
                    let parts: Vec<_> = field.splitn(10, |byte| *byte == b' ').collect();
                    if parts.len() != 10 {
                        bail!("invalid rename Git status record");
                    }
                    (parts[1], parts[2], true, false)
                }
                Some(b'u') => {
                    let parts: Vec<_> = field.splitn(11, |byte| *byte == b' ').collect();
                    if parts.len() != 11 {
                        bail!("invalid unmerged Git status record");
                    }
                    (parts[1], parts[2], false, true)
                }
                _ => bail!("invalid Git status record"),
            };
            if xy.len() != 2 || submodule.len() != 4 {
                bail!("invalid Git status change flags");
            }
            if conflict {
                result.status.conflicted += 1;
            } else {
                if xy[0] != b'.' {
                    result.status.staged += 1;
                }
                if xy[1] != b'.' || submodule[2] == b'M' || submodule[3] == b'U' {
                    result.status.modified += 1;
                }
            }
            if renamed {
                fields
                    .next()
                    .context("missing rename source in Git status")?;
            }
        }
    }
    if !head_seen || !branch_seen {
        bail!("Git status omitted branch metadata");
    }
    Ok(result)
}

fn inspect_upstream(worktree: &mut Worktree, config: &RepoConfig, snapshot: &StatusSnapshot) {
    let Some(branch) = &worktree.branch else {
        return;
    };
    let remote = config.value(&format!("branch.{branch}.remote"));
    let merge = config.value(&format!("branch.{branch}.merge"));
    match (remote, merge) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => worktree
            .warnings
            .push("upstream: branch has incomplete upstream configuration".into()),
        (Some(remote), Some(merge)) => {
            if let (Some(upstream), Some((ahead, behind))) =
                (&snapshot.upstream, snapshot.divergence)
            {
                worktree.upstream = Some(if remote == b"." {
                    String::from_utf8_lossy(merge).into()
                } else {
                    upstream.clone()
                });
                worktree.ahead = Some(ahead);
                worktree.behind = Some(behind);
            } else {
                worktree
                    .warnings
                    .push("upstream: configured reference or divergence is unavailable".into());
            }
        }
    }
}

fn index_fingerprint(path: &Path) -> Result<Option<IndexFingerprint>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot inspect Git index"),
    };
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Some(IndexFingerprint {
        modified: metadata.modified()?,
        len: metadata.len(),
        #[cfg(unix)]
        changed: (metadata.ctime(), metadata.ctime_nsec()),
        #[cfg(unix)]
        inode: (metadata.dev(), metadata.ino()),
        #[cfg(not(unix))]
        created: metadata.created().ok(),
    }))
}

fn tracked_paths(
    path: &Path,
    git_dir: &Path,
    cache: &InspectionCache,
) -> Result<Arc<Vec<PathBuf>>> {
    let index = git_dir.join("index");
    let mut before = match index_fingerprint(&index) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            cache.forget_files(git_dir);
            return Err(error);
        }
    };
    if let Some(paths) = cache.files(git_dir, &before) {
        let after = match index_fingerprint(&index) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                cache.forget_files(git_dir);
                return Err(error);
            }
        };
        if before == after {
            crate::profile::file_list_hit();
            return Ok(paths);
        }
        cache.forget_files(git_dir);
        before = after;
    }
    let result = checked(path, ["ls-files", "-z", "--cached"], false).and_then(|bytes| {
        let mut paths = bytes
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .map(native_path)
            .collect::<Result<Vec<_>>>()?;
        paths.dedup();
        Ok(Arc::new(paths))
    });
    let paths = match result {
        Ok(paths) => paths,
        Err(error) => {
            cache.forget_files(git_dir);
            return Err(error);
        }
    };
    let after = match index_fingerprint(&index) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            cache.forget_files(git_dir);
            return Err(error);
        }
    };
    if before == after {
        cache.insert_files(git_dir.to_owned(), after, Arc::clone(&paths));
    } else {
        cache.forget_files(git_dir);
    }
    Ok(paths)
}

/// Last activity re-stats every tracked and freshly enumerated nonignored untracked file.
/// Only the index-derived tracked path list is cached, never timestamps or dirty state.
fn updated_at(
    worktree: &Worktree,
    git_dir: &Path,
    untracked: Option<&[PathBuf]>,
    cache: &InspectionCache,
) -> Result<Option<DateTime<Utc>>> {
    let tracked = tracked_paths(&worktree.path, git_dir, cache)?;
    let fallback;
    let untracked = match untracked {
        Some(paths) => paths,
        None => {
            let bytes = checked(
                &worktree.path,
                ["ls-files", "-z", "--others", "--exclude-standard"],
                false,
            )?;
            fallback = bytes
                .split(|byte| *byte == 0)
                .filter(|field| !field.is_empty())
                .map(native_path)
                .collect::<Result<Vec<_>>>()?;
            &fallback
        }
    };
    let mut latest = None;
    for file in tracked.iter().chain(untracked) {
        let file = worktree.path.join(file);
        match fs::symlink_metadata(&file) {
            Ok(metadata) => {
                let modified = metadata.modified().with_context(|| {
                    format!("cannot read modification time: {}", file.display())
                })?;
                latest =
                    Some(latest.map_or(modified, |existing: SystemTime| existing.max(modified)));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = file.parent() {
                    include_mtime(parent, &mut latest)?;
                }
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot inspect {}", file.display()));
            }
        }
    }
    include_git_mtimes(
        git_dir,
        &worktree.repo.common_dir,
        worktree.branch.as_deref(),
        &mut latest,
    )?;
    Ok(latest.map(DateTime::<Utc>::from))
}

fn include_git_mtimes(
    git_dir: &Path,
    common_dir: &Path,
    branch: Option<&str>,
    latest: &mut Option<SystemTime>,
) -> Result<()> {
    for file in ["HEAD", "index", "logs/HEAD"] {
        include_mtime(&git_dir.join(file), latest)?;
    }
    if let Some(branch) = branch {
        include_mtime(&common_dir.join("refs/heads").join(branch), latest)?;
    }
    Ok(())
}

fn include_mtime(path: &Path, latest: &mut Option<SystemTime>) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .with_context(|| format!("cannot read modification time: {}", path.display()))?;
            *latest = Some(latest.map_or(modified, |existing| existing.max(modified)));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
        }
    }
    Ok(())
}

fn validate_repository(repo: &Repository) -> Result<()> {
    let actual = common_dir(&repo.common_dir)?;
    if actual != identity(&repo.common_dir)? {
        bail!("repository identity changed; refresh before continuing");
    }
    Ok(())
}

fn validate_path(worktree: &Worktree) -> Result<()> {
    if worktree.is_bare {
        if common_dir(&worktree.path)? != identity(&worktree.repo.common_dir)? {
            bail!("bare repository identity changed; refresh before continuing");
        }
        return Ok(());
    }
    inspect_path(worktree).map(|_| ())
}

/// Refresh the registration and reject stale selections before any management write.
fn current(worktree: &Worktree) -> Result<Worktree> {
    validate_repository(&worktree.repo)?;
    let selected = identity(&worktree.path)?;
    let records = registry_for(&worktree.repo)?;
    let (index, record) = records
        .into_iter()
        .enumerate()
        .find(|(_, record)| identity(&record.path).is_ok_and(|path| path == selected))
        .context("worktree is no longer registered; refresh before continuing")?;
    if record.head != worktree.head
        || record.branch != worktree.branch
        || record.bare != worktree.is_bare
    {
        bail!("worktree HEAD or branch changed; refresh before continuing");
    }
    let fresh = enrich(&worktree.repo, record, index == 0, &Ok(None))?;
    if fresh.head != worktree.head || fresh.branch != worktree.branch {
        bail!("worktree HEAD or branch changed during inspection; refresh before continuing");
    }
    if fresh.path.try_exists()? {
        validate_path(&fresh)?;
    }
    Ok(fresh)
}

fn require_linked(worktree: &Worktree) -> Result<()> {
    if worktree.is_main || worktree.is_bare {
        bail!("the primary or bare repository cannot be managed as a linked worktree");
    }
    Ok(())
}

fn require_unlocked(worktree: &Worktree) -> Result<()> {
    if worktree.locked.is_some() {
        bail!("worktree is locked; unlock it explicitly before continuing");
    }
    Ok(())
}

fn require_no_nested_repositories(worktree: &Worktree) -> Result<()> {
    let selected = identity(&worktree.path)?;
    for record in registry_for(&worktree.repo)? {
        let registered = identity(&record.path)?;
        if registered != selected && registered.starts_with(&selected) {
            bail!(
                "worktree contains another registered worktree: {}",
                record.path.display()
            );
        }
    }
    let mut entries = WalkDir::new(&worktree.path).follow_links(false).into_iter();
    while let Some(entry) = entries.next() {
        let entry = entry.with_context(|| {
            format!(
                "cannot verify nested repositories in {}",
                worktree.path.display()
            )
        })?;
        if entry.depth() == 0 {
            continue;
        }
        if entry.file_name() == ".git" {
            if entry.depth() == 1 {
                entries.skip_current_dir();
                continue;
            }
            bail!(
                "worktree contains a nested repository: {}",
                entry.path().display()
            );
        }
        if entry.file_type().is_dir()
            && entry.path().join("HEAD").is_file()
            && entry.path().join("objects").is_dir()
            && entry.path().join("refs").is_dir()
        {
            bail!(
                "worktree contains a nested bare repository: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn mutation<I, S>(path: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let result = output(path, args, true)?;
    mutation_output(result)
}

fn mutation_output(result: Output) -> Result<String> {
    if !result.status.success() {
        bail!("{}", git_error(&result));
    }
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    Ok(redact_url_credentials(
        &[stdout.trim(), stderr.trim()]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
    ))
}

pub fn add(options: &AddOptions) -> Result<PathBuf> {
    let repo = repository(&options.repo)?;
    if !options.new_branch && options.start_point.is_some() {
        bail!("a start point applies only when creating a new branch");
    }
    if options.branch.is_empty() {
        bail!("branch name must not be empty");
    }
    checked(
        &repo.common_dir,
        ["check-ref-format", "--branch", &options.branch],
        false,
    )?;
    let path = absolute(&options.path)?;
    let mut args: Vec<OsString> = vec!["worktree".into(), "add".into()];
    if options.new_branch {
        args.extend(["-b".into(), options.branch.clone().into()]);
    }
    args.extend(["--".into(), path.as_os_str().to_owned()]);
    if options.new_branch {
        if let Some(start) = &options.start_point {
            args.push(start.into());
        }
    } else {
        args.push(options.branch.clone().into());
    }
    mutation(&repo.common_dir, args)?;
    Ok(path)
}

fn removable(worktree: &Worktree, options: &RemoveOptions) -> Result<Worktree> {
    let fresh = current(worktree)?;
    require_linked(&fresh)?;
    require_unlocked(&fresh)?;
    validate_path(&fresh)?;
    if !options.force {
        if let Some(error) = &fresh.status.error {
            bail!("cannot verify uncommitted changes: {error}");
        }
        if fresh.status.is_dirty() {
            bail!("worktree has uncommitted changes; explicit force is required to discard them");
        }
    }
    require_no_nested_repositories(&fresh)?;
    Ok(fresh)
}

/// Check exactly the same eligibility as removal, without modifying anything.
pub fn validate_remove(worktree: &Worktree, options: &RemoveOptions) -> Result<()> {
    removable(worktree, options).map(|_| ())
}

pub fn remove(worktree: &Worktree, options: &RemoveOptions) -> Result<String> {
    let fresh = removable(worktree, options)?;
    let mut args: Vec<OsString> = vec!["worktree".into(), "remove".into()];
    if options.force {
        args.push("--force".into());
    }
    args.extend(["--".into(), fresh.path.as_os_str().to_owned()]);
    let result = mutation(&fresh.repo.common_dir, args)?;
    if options.delete_branch
        && let Some(branch) = &fresh.branch
    {
        match mutation(&fresh.repo.common_dir, ["branch", "-d", "--", branch]) {
            Ok(deletion) => {
                return Ok([result, deletion]
                    .into_iter()
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"));
            }
            Err(error) => bail!("worktree removed, but its branch was retained: {error}"),
        }
    }
    Ok(result)
}

pub fn lock(worktree: &Worktree, reason: &str) -> Result<String> {
    let fresh = current(worktree)?;
    require_linked(&fresh)?;
    require_unlocked(&fresh)?;
    mutation(
        &fresh.repo.common_dir,
        [
            OsStr::new("worktree"),
            OsStr::new("lock"),
            OsStr::new("--reason"),
            OsStr::new(reason),
            OsStr::new("--"),
            fresh.path.as_os_str(),
        ],
    )
}

pub fn unlock(worktree: &Worktree) -> Result<String> {
    let fresh = current(worktree)?;
    require_linked(&fresh)?;
    if fresh.locked.is_none() {
        bail!("worktree is not locked");
    }
    if fresh.locked != worktree.locked {
        bail!("worktree lock changed; refresh before continuing");
    }
    mutation(
        &fresh.repo.common_dir,
        [
            OsStr::new("worktree"),
            OsStr::new("unlock"),
            OsStr::new("--"),
            fresh.path.as_os_str(),
        ],
    )
}

pub fn prune(repo: &Repository, dry_run: bool) -> Result<String> {
    validate_repository(repo)?;
    let mut args = vec!["worktree", "prune", "--verbose", "--expire", "now"];
    if dry_run {
        args.push("--dry-run");
    }
    mutation(&repo.common_dir, args)
}

pub fn fetch(repo: &Repository) -> Result<String> {
    validate_repository(repo)?;
    let mut uses_ssh = false;
    for name in read_line(&repo.common_dir, &["remote"])?.lines() {
        let urls = read_line(&repo.common_dir, &["remote", "get-url", "--all", name])?;
        uses_ssh |= urls.lines().any(ssh_remote);
    }
    let mut fetch = command(&repo.common_dir, true);
    fetch.args(["fetch", "--all", "--prune"]);
    fetch.env("GIT_ASKPASS", "false");
    fetch.env("SSH_ASKPASS", "false");
    fetch.env("SSH_ASKPASS_REQUIRE", "never");
    if uses_ssh {
        let configured = output(
            &repo.common_dir,
            ["config", "--get", "core.sshCommand"],
            false,
        )?;
        let custom = configured.status.success()
            || std::env::var_os("GIT_SSH_COMMAND").is_some()
            || std::env::var_os("GIT_SSH").is_some();
        if !configured.status.success() && configured.status.code() != Some(1) {
            bail!("{}", git_error(&configured));
        }
        if custom {
            bail!(
                "cannot fetch an SSH remote with a custom SSH command noninteractively; run `git fetch --all --prune` in the repository and refresh, or configure keys and proxies in ~/.ssh/config"
            );
        }
        // A constant native transport preserves ~/.ssh/config while preventing
        // password/passphrase prompts from taking over the raw terminal.
        fetch.env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=15",
        );
        fetch.env("GIT_SSH_VARIANT", "ssh");
    }
    mutation_output(fetch.output().context("could not run Git fetch")?)
}

fn ssh_remote(url: &str) -> bool {
    if ["ssh://", "git+ssh://", "ssh+git://"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
    {
        return true;
    }
    if url.contains("://") {
        return false;
    }
    let Some((host, path)) = url.split_once(':') else {
        return false;
    };
    // Git treats a colon before any directory separator as SCP-style SSH,
    // except native Windows drive paths.
    !(host.is_empty()
        || host.contains(['/', '\\'])
        || (host.len() == 1
            && host.as_bytes()[0].is_ascii_alphabetic()
            && path.starts_with(['/', '\\'])))
}

pub fn move_worktree(worktree: &Worktree, target: &Path) -> Result<String> {
    let fresh = current(worktree)?;
    require_linked(&fresh)?;
    require_unlocked(&fresh)?;
    validate_path(&fresh)?;
    require_no_nested_repositories(&fresh)?;
    let target = absolute(target)?;
    if target.starts_with(identity(&fresh.path)?) {
        bail!("destination cannot be inside the selected worktree");
    }
    mutation(
        &fresh.repo.common_dir,
        [
            OsStr::new("worktree"),
            OsStr::new("move"),
            OsStr::new("--"),
            fresh.path.as_os_str(),
            target.as_os_str(),
        ],
    )
}

/// Convert common hosted Git remote forms into a commit browser URL.
/// Unsupported/self-hosted remotes intentionally have no guessed link.
pub fn commit_url(remote: &str, head: &str) -> Option<String> {
    if head.is_empty() || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let remote = redact_url_credentials(remote);
    let (host, path) = if let Some(rest) = remote
        .strip_prefix("https://")
        .or_else(|| remote.strip_prefix("http://"))
        .or_else(|| remote.strip_prefix("ssh://"))
    {
        let (host, path) = rest.split_once('/')?;
        let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
        (host.split(':').next()?, path)
    } else {
        let (host, path) = remote.split_once(':')?;
        let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
        (host, path)
    };
    let path = path.trim_matches('/').trim_end_matches(".git");
    if path.split('/').count() < 2 || path.contains(['?', '#', '\\']) {
        return None;
    }
    let path = path
        .split('/')
        .map(url_component)
        .collect::<Vec<_>>()
        .join("/");
    match host.to_ascii_lowercase().as_str() {
        "github.com" => Some(format!("https://github.com/{path}/commit/{head}")),
        "gitlab.com" => Some(format!("https://gitlab.com/{path}/-/commit/{head}")),
        "bitbucket.org" => Some(format!("https://bitbucket.org/{path}/commits/{head}")),
        _ => None,
    }
}

fn url_component(component: &str) -> String {
    let mut result = String::new();
    for byte in component.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(char::from(byte));
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}

fn redact_url_credentials(text: &str) -> String {
    let mut result = text.to_owned();
    for scheme in ["https://", "http://", "ssh://"] {
        let mut offset = 0;
        while let Some(found) = result[offset..].find(scheme) {
            let start = offset + found + scheme.len();
            let end = result[start..]
                .find(['/', ' ', '\n', '\r', '\t', '\'', '"'])
                .map_or(result.len(), |end| start + end);
            if let Some(at) = result[start..end].rfind('@') {
                result.replace_range(start..start + at + 1, "");
            }
            // Authentication may also be carried in a URL query or fragment.
            // Drop these from diagnostics and display metadata; fetch still uses
            // the original repository configuration, never this display value.
            let url_end = result[start..]
                .find([' ', '\n', '\r', '\t', '\'', '"', '<', '>'])
                .map_or(result.len(), |end| start + end);
            if let Some(query) = result[start..url_end].find(['?', '#']) {
                result.replace_range(start + query..url_end, "");
            }
            offset = start;
            if offset >= result.len() {
                break;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_requires_nul_delimited_worktree_support() {
        for version in [
            "git version 2.36.0",
            "git version 2.56.0",
            "git version 2.39.5 (Apple Git-154)",
            "git version 3.0.0",
        ] {
            assert!(supported_version(version));
        }
        for version in [
            "git version 2.35.9",
            "git version 1.99.0",
            "git version bad",
            "git version 2",
            "garbage",
        ] {
            assert!(!supported_version(version));
        }
    }

    #[test]
    fn parses_newline_paths_and_lock_reasons() {
        let rows = parse_registry(b"worktree /tmp/a\nb\0HEAD 012abc\0branch refs/heads/main\0\0worktree /tmp/c\0HEAD 000000\0detached\0locked reason\nwith newline\0prunable gone\0\0").unwrap();
        assert_eq!(rows[0].path, Path::new("/tmp/a\nb"));
        assert_eq!(rows[1].head, None);
        assert_eq!(rows[1].locked.as_deref(), Some("reason\nwith newline"));
    }

    #[cfg(unix)]
    #[test]
    fn parser_preserves_non_utf8_path_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let rows = parse_registry(b"worktree /tmp/native-\xff\0HEAD abc123\0detached\0\0").unwrap();
        assert_eq!(rows[0].path.as_os_str().as_bytes(), b"/tmp/native-\xff");
    }

    #[test]
    fn hosted_commit_links_and_credentials() {
        assert_eq!(
            commit_url("git@github.com:owner/repo.git", "abc123").as_deref(),
            Some("https://github.com/owner/repo/commit/abc123")
        );
        assert_eq!(
            commit_url("ssh://git@gitlab.com/team/sub/repo.git", "abc123").as_deref(),
            Some("https://gitlab.com/team/sub/repo/-/commit/abc123")
        );
        assert_eq!(
            commit_url("https://user:secret@bitbucket.org/team/repo.git", "abc123").as_deref(),
            Some("https://bitbucket.org/team/repo/commits/abc123")
        );
        assert_eq!(
            commit_url("https://example.org/team/repo.git", "abc123"),
            None
        );
        assert_eq!(
            redact_url_credentials(
                "error https://token@github.com/a/b https://user:secret@gitlab.com/x/y"
            ),
            "error https://github.com/a/b https://gitlab.com/x/y"
        );
        assert_eq!(
            redact_url_credentials(
                "https://token@github.com/a/b.git?access_token=secret ssh://password@gitlab.com/a/b#secret"
            ),
            "https://github.com/a/b.git ssh://gitlab.com/a/b"
        );
    }

    #[test]
    fn ssh_transport_detection_distinguishes_local_and_http_paths() {
        for remote in [
            "git@github.com:team/repo.git",
            "host:repo",
            "ssh://git@host/repo",
            "git+ssh://host/repo",
            "ssh+git://host/repo",
        ] {
            assert!(ssh_remote(remote), "{remote}");
        }
        for remote in [
            "https://host/repo",
            "http://host/repo",
            "git://host/repo",
            "file:///tmp/repo",
            "/tmp/repo:local",
            "./repo:local",
            "C:\\repo",
            "C:/repo",
            "relative/repo",
        ] {
            assert!(!ssh_remote(remote), "{remote}");
        }
    }

    #[test]
    fn porcelain_v2_preserves_native_paths_and_skips_rename_source() {
        let status = parse_status(b"# branch.oid abc123\0# branch.head feature\0# branch.upstream origin/main\0# branch.ab +2 -3\x002 R. N... 100644 100644 100644 abc abc R100 renamed\nfile\0original\nfile\0? fresh\nfile\0").unwrap();
        assert_eq!(status.head.as_deref(), Some("abc123"));
        assert_eq!(status.divergence, Some((2, 3)));
        assert_eq!(status.status.staged, 1);
        assert_eq!(status.status.untracked, 1);
        assert_eq!(status.untracked, vec![PathBuf::from("fresh\nfile")]);
        assert!(parse_status(b"# branch.oid abc123\0# branch.head feature\x002 R. N... 100644 100644 100644 abc abc R100 dest\0").is_err());
        assert!(parse_status(b"? file\0").is_err());
    }

    #[test]
    fn commit_cache_evicts_old_entries_and_rejects_oversized_metadata() {
        let cache = InspectionCache::default();
        let key = |head: String| CommitKey {
            common_dir: PathBuf::from("repo/.git"),
            head,
        };
        for index in 0..4097 {
            cache.insert_commit(
                key(index.to_string()),
                CommitMetadata {
                    subject: "subject".into(),
                    committed_at: None,
                },
            );
        }
        assert!(cache.commit(&key("0".into())).is_none());
        assert!(cache.commit(&key("4096".into())).is_some());
        let state = cache.0.lock().unwrap();
        assert_eq!(state.commits.len(), 4096);
        assert!(state.commit_bytes <= 16 * 1024 * 1024);
        drop(state);
        cache.insert_commit(
            key("large".into()),
            CommitMetadata {
                subject: "x".repeat(16 * 1024 * 1024),
                committed_at: None,
            },
        );
        assert!(cache.commit(&key("large".into())).is_none());
    }

    #[test]
    fn tracked_path_cache_bounds_and_index_identity_control_reuse() {
        let cache = InspectionCache::default();
        for index in 0..257 {
            cache.insert_files(
                PathBuf::from(format!("repo/{index}")),
                None,
                Arc::new(vec![PathBuf::from("file")]),
            );
        }
        assert!(cache.files(Path::new("repo/0"), &None).is_none());
        assert!(cache.files(Path::new("repo/256"), &None).is_some());
        assert_eq!(cache.0.lock().unwrap().files.len(), 256);
        cache.forget_files(Path::new("repo/256"));
        assert!(cache.files(Path::new("repo/256"), &None).is_none());
    }
}
