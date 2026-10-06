use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Repository {
    /// Shared Git metadata directory; identifies a repository across worktrees.
    pub common_dir: PathBuf,
    pub path: PathBuf,
    pub name: String,
    pub remote: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WorktreeStatus {
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
    pub conflicted: usize,
    pub error: Option<String>,
}

impl WorktreeStatus {
    pub fn is_dirty(&self) -> bool {
        self.staged + self.modified + self.untracked + self.conflicted > 0
    }

    pub fn summary(&self) -> String {
        if self.error.is_some() {
            return "unknown".into();
        }
        if !self.is_dirty() {
            return "clean".into();
        }
        format!(
            "{} staged, {} modified, {} untracked, {} conflicts",
            self.staged, self.modified, self.untracked, self.conflicted
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Worktree {
    pub repo: Repository,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub short_head: Option<String>,
    pub commit_subject: Option<String>,
    pub committed_at: Option<DateTime<Utc>>,
    /// Latest tracked/untracked file or Git HEAD/index modification, when known.
    pub updated_at: Option<DateTime<Utc>>,
    pub status: WorktreeStatus,
    pub locked: Option<String>,
    pub prunable: Option<String>,
    pub is_main: bool,
    pub is_bare: bool,
    pub commit_url: Option<String>,
    pub upstream: Option<String>,
    pub ahead: Option<usize>,
    pub behind: Option<usize>,
    pub merged: Option<bool>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub root: PathBuf,
    pub follow_links: bool,
    pub jobs: usize,
    /// Directory basenames explicitly excluded by the user; hidden directories are included.
    pub excludes: Vec<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            follow_links: false,
            jobs: std::thread::available_parallelism()
                .map_or(4, usize::from)
                .min(8),
            excludes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub root: PathBuf,
    pub discovery_complete: bool,
    pub pending_repositories: Vec<PathBuf>,
    pub repositories: usize,
    pub worktrees: Vec<Worktree>,
    pub warnings: Vec<String>,
    pub elapsed_ms: u64,
    pub profile: crate::profile::ScanProfile,
}
