//! Recursive discovery and session-local, incremental worktree inspection.

use crate::{
    git,
    model::{Repository, ScanOptions, ScanReport},
    profile::{Collector, millis},
};
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use walkdir::{DirEntry, WalkDir};

#[derive(Debug, Clone)]
pub enum ScanEvent {
    Progress {
        directories: usize,
        repositories: usize,
    },
    Rows {
        repository: Repository,
        worktrees: Vec<crate::model::Worktree>,
        refreshed: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ScanSession {
    cache: git::InspectionCache,
    inventory: Arc<Mutex<Option<Inventory>>>,
    gate: Arc<Mutex<()>>,
}

#[derive(Debug, Clone)]
struct Inventory {
    root: PathBuf,
    follow_links: bool,
    excludes: Vec<String>,
    chunks: HashMap<PathBuf, Chunk>,
    warnings: Vec<String>,
    discovery_complete: bool,
}
#[derive(Debug, Clone)]
struct Chunk {
    repository: Repository,
    worktrees: Vec<crate::model::Worktree>,
    warnings: Vec<String>,
    complete: bool,
    refreshed: bool,
}

enum Entry {
    Candidate(PathBuf),
    Warning(String),
}

pub fn scan(options: &ScanOptions) -> Result<ScanReport> {
    ScanSession::default().scan(options, |_| {})
}

impl ScanSession {
    /// Discover nested repositories without implicit directory exclusions.
    pub fn scan(
        &self,
        options: &ScanOptions,
        emit: impl Fn(ScanEvent) + Send + Sync,
    ) -> Result<ScanReport> {
        let _gate = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        self.full_scan(options, &emit)
    }

    /// Refresh registry and metadata for known owners; full discovery is explicit.
    /// Scoped refreshes preserve the last observed rows of other repositories.
    pub fn refresh(
        &self,
        options: &ScanOptions,
        only: Option<&Path>,
        emit: impl Fn(ScanEvent) + Send + Sync,
    ) -> Result<ScanReport> {
        let _gate = self.gate.lock().unwrap_or_else(|error| error.into_inner());
        let root = scan_root(options)?;
        let previous = self
            .inventory
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(mut inventory) = previous.filter(|old| {
            old.root == root
                && old.follow_links == options.follow_links
                && old.excludes == options.excludes
        }) else {
            return self.full_scan(options, &emit);
        };
        if inventory.chunks.is_empty() {
            return self.full_scan(options, &emit);
        }
        let started = Instant::now();
        let collector = Collector::default();
        let first_result = Mutex::new(None);
        let repositories: Vec<_> = inventory
            .chunks
            .values()
            .filter(|chunk| only.is_none_or(|path| chunk.repository.common_dir == path))
            .cloned()
            .collect();
        let pool = worker_pool(options)?;
        let chunks = pool.install(|| {
            repositories
                .par_iter()
                .map(|previous| {
                    let _scope = collector.enter();
                    let result = git::refresh_repository(&previous.repository)
                        .and_then(|repo| self.inspect(repo, &root, options));
                    let chunk = match result {
                        Ok(chunk) => chunk,
                        Err(error) => failed_chunk(previous, &format!("{error:#}")),
                    };
                    publish(&chunk, &emit, &first_result, started);
                    chunk
                })
                .collect::<Vec<_>>()
        });
        for chunk in chunks {
            inventory
                .chunks
                .insert(chunk.repository.common_dir.clone(), chunk);
        }
        let mut profile = collector.snapshot();
        profile.inspection_ms = millis(started.elapsed());
        profile.first_result_ms = *first_result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.finish(inventory, started, profile)
    }

    fn full_scan(
        &self,
        options: &ScanOptions,
        emit: &(impl Fn(ScanEvent) + Send + Sync),
    ) -> Result<ScanReport> {
        let started = Instant::now();
        let root = scan_root(options)?;
        let collector = Collector::default();
        let directories = AtomicUsize::new(0);
        let repositories = AtomicUsize::new(0);
        let identities = Mutex::new(HashSet::new());
        let aliases = Mutex::new(HashMap::<PathBuf, Vec<PathBuf>>::new());
        let warnings = Mutex::new(Vec::new());
        let first_result = Mutex::new(None);
        let inspection_started = Mutex::new(None);
        let discovery_finished = Mutex::new(None);
        let mut metadata_roots = HashMap::new();
        let mut candidates = HashSet::new();
        let mut walker = WalkDir::new(&root)
            .follow_links(options.follow_links)
            .into_iter();
        // A marker entry identifies its parent without probing .git and HEAD in every directory.
        let entries = std::iter::from_fn(|| {
            loop {
                let _scope = collector.enter();
                let Some(entry) = walker.next() else {
                    *discovery_finished
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        Some(millis(started.elapsed()));
                    return None;
                };
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => return Some(Entry::Warning(format!("directory scan: {error}"))),
                };
                let marker = entry.file_name() == ".git";
                if !descend(&entry, &root, options, &mut metadata_roots) {
                    if entry.file_type().is_dir() {
                        walker.skip_current_dir();
                    }
                    if marker
                        && let Some(parent) = entry.path().parent()
                        && candidates.insert(parent.to_path_buf())
                    {
                        return Some(Entry::Candidate(parent.to_path_buf()));
                    }
                    continue;
                }
                if entry.file_type().is_dir() {
                    let count = directories.fetch_add(1, Ordering::Relaxed) + 1;
                    if count == 1 || count.is_multiple_of(128) {
                        emit(ScanEvent::Progress {
                            directories: count,
                            repositories: repositories.load(Ordering::Relaxed),
                        });
                    }
                    if entry.depth() == 0
                        && (entry.path().join(".git").symlink_metadata().is_ok()
                            || is_bare_candidate(entry.path()))
                        && candidates.insert(entry.path().to_path_buf())
                    {
                        return Some(Entry::Candidate(entry.path().to_path_buf()));
                    }
                } else if entry.file_name() == "HEAD"
                    && let Some(parent) = entry.path().parent()
                    && is_bare_candidate(parent)
                    && candidates.insert(parent.to_path_buf())
                {
                    return Some(Entry::Candidate(parent.to_path_buf()));
                }
            }
        });
        let pool = worker_pool(options)?;
        let mut chunks = pool.install(|| {
            entries
                .par_bridge()
                .filter_map(|entry| {
                    let _scope = collector.enter();
                    let path = match entry {
                        Entry::Warning(warning) => {
                            warnings
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .push(warning);
                            return None;
                        }
                        Entry::Candidate(path) => path,
                    };
                    inspection_started
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .get_or_insert_with(Instant::now);
                    let identity = match git::repository_common_dir(&path) {
                        Ok(identity) => identity,
                        Err(error) => {
                            warnings
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .push(format!("repository {}: {error:#}", path.display()));
                            return None;
                        }
                    };
                    if !identities
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert(identity.clone())
                    {
                        aliases
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .entry(identity)
                            .or_default()
                            .push(path);
                        return None;
                    }
                    let result = self.inspect_candidate(&path, &identity, &root, options);
                    match result {
                        Ok(chunk) => {
                            repositories.fetch_add(1, Ordering::Relaxed);
                            publish(&chunk, emit, &first_result, started);
                            Some(chunk)
                        }
                        Err(error) => {
                            identities
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .remove(&identity);
                            warnings
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .push(format!("repository {}: {error:#}", path.display()));
                            None
                        }
                    }
                })
                .collect::<Vec<_>>()
        });
        // A reserved identity may fail while other workers skip its healthy aliases.
        // Retry those aliases after the initial workers finish instead of losing the owner.
        let completed: HashSet<_> = chunks
            .iter()
            .map(|chunk| chunk.repository.common_dir.clone())
            .collect();
        let retries: Vec<_> = aliases
            .into_inner()
            .unwrap_or_else(|error| error.into_inner())
            .into_iter()
            .filter(|(identity, _)| !completed.contains(identity))
            .collect();
        chunks.extend(pool.install(|| {
            retries
                .into_par_iter()
                .filter_map(|(identity, paths)| {
                    let _scope = collector.enter();
                    for path in paths {
                        match self.inspect_candidate(&path, &identity, &root, options) {
                            Ok(chunk) => {
                                repositories.fetch_add(1, Ordering::Relaxed);
                                publish(&chunk, emit, &first_result, started);
                                return Some(chunk);
                            }
                            Err(error) => warnings
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .push(format!("repository {}: {error:#}", path.display())),
                        }
                    }
                    None
                })
                .collect::<Vec<_>>()
        }));
        let mut profile = collector.snapshot();
        profile.directories = directories.load(Ordering::Relaxed);
        profile.discovery_ms = discovery_finished
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unwrap_or_else(|| millis(started.elapsed()));
        profile.inspection_ms = inspection_started
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .map_or(0, |start| millis(start.elapsed()));
        profile.first_result_ms = *first_result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let warnings = warnings
            .into_inner()
            .unwrap_or_else(|error| error.into_inner());
        let inventory = Inventory {
            root,
            follow_links: options.follow_links,
            excludes: options.excludes.clone(),
            discovery_complete: warnings.is_empty(),
            warnings,
            chunks: chunks
                .into_iter()
                .map(|chunk| (chunk.repository.common_dir.clone(), chunk))
                .collect(),
        };
        self.finish(inventory, started, profile)
    }

    fn inspect_candidate(
        &self,
        path: &Path,
        identity: &Path,
        root: &Path,
        options: &ScanOptions,
    ) -> Result<Chunk> {
        let repo = git::repository(path)?;
        if repo.common_dir != identity {
            bail!("repository identity changed during discovery; retry full discovery");
        }
        self.inspect(repo, root, options)
    }

    fn inspect(&self, repo: Repository, root: &Path, options: &ScanOptions) -> Result<Chunk> {
        let mut scope_warnings = Vec::new();
        let mut identities = HashMap::new();
        let mut worktrees = git::worktrees_filtered_cached(
            &repo,
            |path| match scoped_path(path, root, options) {
                Ok(Some(identity)) => {
                    identities.insert(path.to_path_buf(), identity);
                    true
                }
                Ok(None) => false,
                Err(error) => {
                    scope_warnings.push(format!("worktree {}: {error:#}", path.display()));
                    false
                }
            },
            &self.cache,
            |_| {},
        )?;
        let complete = scope_warnings.is_empty();
        let mut seen = HashSet::new();
        worktrees.retain(|row| {
            identities
                .get(&row.path)
                .is_some_and(|identity| seen.insert(identity.clone()))
        });
        for row in &worktrees {
            scope_warnings.extend(
                row.warnings
                    .iter()
                    .map(|warning| format!("worktree {}: {warning}", row.path.display())),
            );
            if let Some(error) = &row.status.error {
                scope_warnings.push(format!("worktree {}: {error}", row.path.display()));
            }
        }
        Ok(Chunk {
            repository: repo,
            worktrees,
            warnings: scope_warnings,
            complete,
            refreshed: true,
        })
    }

    fn finish(
        &self,
        inventory: Inventory,
        started: Instant,
        profile: crate::profile::ScanProfile,
    ) -> Result<ScanReport> {
        let mut worktrees = Vec::new();
        let mut warnings = inventory.warnings.clone();
        let mut discovery_complete = inventory.discovery_complete;
        let mut pending_repositories = Vec::new();
        for chunk in inventory.chunks.values() {
            worktrees.extend(chunk.worktrees.iter().cloned());
            warnings.extend(chunk.warnings.iter().cloned());
            discovery_complete &= chunk.complete;
            if !chunk.refreshed {
                pending_repositories.push(chunk.repository.common_dir.clone());
            }
        }
        pending_repositories.sort();
        worktrees.sort_by(|a, b| a.repo.path.cmp(&b.repo.path).then(a.path.cmp(&b.path)));
        warnings.sort();
        warnings.dedup();
        let report = ScanReport {
            root: inventory.root.clone(),
            repositories: inventory.chunks.len(),
            worktrees,
            warnings,
            discovery_complete,
            pending_repositories,
            elapsed_ms: millis(started.elapsed()),
            profile,
        };
        *self
            .inventory
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(inventory);
        Ok(report)
    }
}

fn failed_chunk(previous: &Chunk, error: &str) -> Chunk {
    let mut chunk = previous.clone();
    chunk.complete = false;
    chunk.refreshed = false;
    chunk.warnings = vec![format!(
        "worktrees for {}: {error}",
        chunk.repository.path.display()
    )];
    for row in &mut chunk.worktrees {
        row.status.error = Some(format!("refresh failed: {error}"));
    }
    chunk
}
fn publish(
    chunk: &Chunk,
    emit: &(impl Fn(ScanEvent) + Send + Sync),
    first: &Mutex<Option<u64>>,
    started: Instant,
) {
    if chunk.refreshed && !chunk.worktrees.is_empty() {
        first
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert_with(|| millis(started.elapsed()));
    }
    emit(ScanEvent::Rows {
        repository: chunk.repository.clone(),
        worktrees: chunk.worktrees.clone(),
        refreshed: chunk.refreshed,
    });
}
fn scan_root(options: &ScanOptions) -> Result<PathBuf> {
    let root = options
        .root
        .canonicalize()
        .with_context(|| format!("cannot access scan root {}", options.root.display()))?;
    if !root.is_dir() {
        bail!("scan root {} is not a directory", root.display());
    }
    Ok(root)
}
fn worker_pool(options: &ScanOptions) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(options.jobs.max(1))
        .build()
        .context("cannot create repository inspection worker pool")
}

fn descend(
    entry: &DirEntry,
    root: &Path,
    options: &ScanOptions,
    metadata_roots: &mut HashMap<PathBuf, bool>,
) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if entry.file_name() == ".git" {
        return false;
    }
    if entry.file_type().is_dir() || entry.path_is_symlink() {
        if options
            .excludes
            .iter()
            .any(|name| entry.file_name() == name.as_str())
        {
            return false;
        }
        if options.follow_links
            && entry.file_type().is_dir()
            && entry.path_is_symlink()
            && entry
                .path()
                .canonicalize()
                .is_ok_and(|path| !path.starts_with(root))
        {
            return false;
        }
        if matches!(
            entry.file_name().to_str(),
            Some(
                "objects"
                    | "refs"
                    | "hooks"
                    | "info"
                    | "logs"
                    | "branches"
                    | "worktrees"
                    | "reftable"
                    | "rr-cache"
                    | "lost-found"
                    | "modules"
                    | "lfs"
            )
        ) && let Some(parent) = entry.path().parent()
            && is_bare_candidate(parent)
            && *metadata_roots
                .entry(parent.to_path_buf())
                .or_insert_with(|| {
                    // Source directories can contain HEAD/config/objects too. Only
                    // verified Git metadata roots permit administrative pruning.
                    git::repository(parent).is_ok_and(|repo| {
                        parent
                            .canonicalize()
                            .is_ok_and(|path| repo.common_dir == path && repo.path == path)
                    })
                })
        {
            return false;
        }
    }
    true
}

fn is_bare_candidate(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("config").is_file()
}

fn scoped_path(path: &Path, root: &Path, options: &ScanOptions) -> Result<Option<PathBuf>> {
    let absolute = absolute_path(path)?;
    // Resolve the existing prefix of missing registrations too, preventing a
    // registration below a symlink from escaping the physical scan root.
    let resolved = resolve_existing_prefix(&absolute)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    let Ok(relative) = resolved.strip_prefix(root) else {
        return Ok(None);
    };
    if relative.components().any(|component| {
        component.as_os_str() == ".git"
            || options
                .excludes
                .iter()
                .any(|name| component.as_os_str() == name.as_str())
    }) {
        return Ok(None);
    }
    Ok(Some(resolved))
}

fn resolve_existing_prefix(path: &Path) -> std::io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(resolved) => Ok(resolved),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // A dangling symlink cannot safely be treated as an ordinary missing
            // directory: its unresolved target might be outside the scan root.
            if path
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(error);
            }
            let Some(parent) = path.parent() else {
                return Err(error);
            };
            let Some(name) = path.file_name() else {
                return Err(error);
            };
            resolve_existing_prefix(parent).map(|resolved| resolved.join(name))
        }
        Err(error) => Err(error),
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("cannot read current directory")?
            .join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn missing_path_cannot_escape_through_symlink_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("scan");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let root = root.canonicalize().unwrap();
        let options = ScanOptions {
            root: root.clone(),
            ..ScanOptions::default()
        };
        assert!(
            scoped_path(&root.join("link/missing"), &root, &options)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_registration_below_symlinked_root_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("scan");
        std::fs::create_dir(&root).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let root = root.canonicalize().unwrap();
        let options = ScanOptions {
            root: alias.clone(),
            ..ScanOptions::default()
        };
        assert_eq!(
            scoped_path(&alias.join("missing"), &root, &options).unwrap(),
            Some(root.join("missing"))
        );
    }
}
