//! Per-scan measurements. Git costs are accumulated across concurrent workers.

use serde::Serialize;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Clone, Default, Serialize)]
pub struct CommandCost {
    pub calls: usize,
    pub elapsed_us: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanProfile {
    pub discovery_ms: u64,
    pub inspection_ms: u64,
    pub first_result_ms: Option<u64>,
    pub directories: usize,
    pub git_commands: usize,
    pub git_elapsed_ms: u64,
    pub activity_ms: u64,
    pub commit_cache_hits: usize,
    pub file_list_cache_hits: usize,
    pub commands: BTreeMap<String, CommandCost>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Collector(Arc<Mutex<ScanProfile>>);

thread_local! {
    static CURRENT: RefCell<Option<Collector>> = const { RefCell::new(None) };
}

pub(crate) struct Scope(Option<Collector>);
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

impl Collector {
    pub(crate) fn enter(&self) -> Scope {
        Scope(CURRENT.with(|slot| slot.replace(Some(self.clone()))))
    }
    pub(crate) fn snapshot(&self) -> ScanProfile {
        let mut snapshot = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        snapshot.git_elapsed_ms = snapshot
            .commands
            .values()
            .map(|cost| cost.elapsed_us)
            .sum::<u64>()
            / 1_000;
        snapshot
    }
}

fn record(update: impl FnOnce(&mut ScanProfile)) {
    CURRENT.with(|slot| {
        if let Some(collector) = slot.borrow().as_ref() {
            update(
                &mut collector
                    .0
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            );
        }
    });
}

pub(crate) fn git(command: &str, elapsed: Duration) {
    record(|profile| {
        profile.git_commands += 1;
        profile.git_elapsed_ms += millis(elapsed);
        let cost = profile.commands.entry(command.to_owned()).or_default();
        cost.calls += 1;
        cost.elapsed_us += elapsed.as_micros().try_into().unwrap_or(u64::MAX);
    });
}
pub(crate) fn activity(elapsed: Duration) {
    record(|p| p.activity_ms += millis(elapsed));
}
pub(crate) fn commit_hit() {
    record(|p| p.commit_cache_hits += 1);
}
pub(crate) fn file_list_hit() {
    record(|p| p.file_list_cache_hits += 1);
}
pub(crate) fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}
