//! Responsive worktree dashboard. Terminal output is deliberately confined to stderr.

use std::{
    collections::BTreeSet,
    io::{self, Stderr},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::Utc;
use crossterm::{
    cursor::{MoveTo, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{
        Clear as ClearScreen, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{
        Block, Borders, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState, Wrap,
    },
};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    git::{self, AddOptions, RemoveOptions},
    model::{Repository, ScanOptions, ScanReport, Worktree},
    profile::ScanProfile,
    scan::{ScanEvent, ScanSession},
};

mod interaction;
use interaction::*;

type DashboardTerminal = Terminal<CrosstermBackend<Stderr>>;

/// Run the dashboard, returning the chosen directory when the user presses Enter.
pub fn run(options: ScanOptions, select_only: bool, mouse: bool) -> Result<Option<PathBuf>> {
    let _guard = TerminalGuard::enter(mouse)?;
    // Ratatui's cursor-preserving clear queries stdout, which belongs to path output.
    execute!(io::stderr(), ClearScreen(ClearType::All), MoveTo(0, 0))?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stderr()))?;
    let (sender, receiver) = mpsc::channel();
    let mut app = App::new(options, select_only, sender);
    app.rescan();
    event_loop(&mut terminal, &mut app, receiver)
}

fn event_loop(
    terminal: &mut DashboardTerminal,
    app: &mut App,
    receiver: Receiver<WorkerMessage>,
) -> Result<Option<PathBuf>> {
    let mut redraw = true;
    let mut last_draw = Instant::now();
    loop {
        // A large discovery stream must leave time for keyboard and resize events.
        for _ in 0..64 {
            let Ok(message) = receiver.try_recv() else {
                break;
            };
            app.receive(message);
            redraw = true;
        }
        if redraw || last_draw.elapsed() >= Duration::from_secs(1) {
            terminal.draw(|frame| app.render(frame))?;
            redraw = false;
            last_draw = Instant::now();
        }
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match app.key(key) {
                    Intent::Continue => redraw = true,
                    Intent::Quit => return Ok(None),
                    Intent::Select(path) => return Ok(Some(path)),
                },
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::Down(MouseButton::Left | MouseButton::Right)
                            | MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown
                    ) =>
                {
                    match app.mouse(mouse) {
                        Intent::Continue => redraw = true,
                        Intent::Quit => return Ok(None),
                        Intent::Select(path) => return Ok(Some(path)),
                    }
                }
                Event::Paste(text) => {
                    app.paste(&text);
                    redraw = true;
                }
                Event::Resize(_, _) => {
                    app.hits = HitMap::default();
                    redraw = true;
                }
                _ => {}
            }
        }
    }
}

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

struct TerminalGuard {
    previous_hook: Option<std::sync::Arc<PanicHook>>,
}

impl TerminalGuard {
    fn enter(mouse: bool) -> Result<Self> {
        enable_raw_mode().context("could not enable terminal raw mode")?;
        let mut guard = Self {
            previous_hook: None,
        };
        execute!(io::stderr(), EnterAlternateScreen, EnableBracketedPaste)
            .context("could not enter alternate screen")?;
        if mouse {
            execute!(io::stderr(), EnableMouseCapture).context("could not enable mouse input")?;
        }
        let previous_hook = std::sync::Arc::new(std::panic::take_hook());
        let hook = previous_hook.clone();
        let terminal_thread = thread::current().id();
        std::panic::set_hook(Box::new(move |info| {
            if thread::current().id() == terminal_thread {
                restore_terminal();
            }
            hook(info);
        }));
        guard.previous_hook = Some(previous_hook);
        Ok(guard)
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stderr(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        Show
    );
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        // The panic hook already restored the terminal; changing hooks while unwinding panics.
        if !thread::panicking()
            && let Some(hook) = self.previous_hook.take()
        {
            drop(std::panic::take_hook());
            match std::sync::Arc::try_unwrap(hook) {
                Ok(hook) => std::panic::set_hook(hook),
                Err(hook) => std::panic::set_hook(Box::new(move |info| hook(info))),
            }
        }
    }
}

enum WorkerMessage {
    Event(ScanEvent),
    Scan(std::result::Result<ScanReport, String>),
    Operation(std::result::Result<OperationOutcome, String>),
}

enum OperationOutcome {
    Updated(String),
    Notice(String),
    PrunePreview(Repository, String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sort {
    Repository,
    Activity,
    Status,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Self::Repository => Self::Activity,
            Self::Activity => Self::Status,
            Self::Status => Self::Repository,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Activity => "recent activity",
            Self::Status => "changes",
        }
    }
}

enum Intent {
    Continue,
    Quit,
    Select(PathBuf),
}

struct App {
    options: ScanOptions,
    select_only: bool,
    sender: Sender<WorkerMessage>,
    session: ScanSession,
    worktrees: Vec<Worktree>,
    warnings: Vec<String>,
    repositories: usize,
    elapsed_ms: u64,
    discovery_complete: bool,
    profile: ScanProfile,
    pending_repos: BTreeSet<PathBuf>,
    requested_repos: BTreeSet<PathBuf>,
    streamed_repos: BTreeSet<PathBuf>,
    scanning: bool,
    scan_started: Option<Instant>,
    scan_label: &'static str,
    directories: usize,
    visible: Vec<usize>,
    table: TableState,
    marked: BTreeSet<PathBuf>,
    search: String,
    search_cursor: usize,
    searching: bool,
    dirty_only: bool,
    sort: Sort,
    sort_reversed: bool,
    busy: Option<String>,
    mutation_running: bool,
    operation_scope: Option<PathBuf>,
    viewport_ready: bool,
    message: String,
    modal: Option<Modal>,
    hits: HitMap,
}

impl App {
    fn new(options: ScanOptions, select_only: bool, sender: Sender<WorkerMessage>) -> Self {
        Self {
            options,
            select_only,
            sender,
            session: ScanSession::default(),
            worktrees: Vec::new(),
            warnings: Vec::new(),
            repositories: 0,
            elapsed_ms: 0,
            discovery_complete: false,
            profile: ScanProfile::default(),
            pending_repos: BTreeSet::new(),
            requested_repos: BTreeSet::new(),
            streamed_repos: BTreeSet::new(),
            scanning: false,
            scan_started: None,
            scan_label: "Discovering nested repositories",
            directories: 0,
            visible: Vec::new(),
            table: TableState::default(),
            marked: BTreeSet::new(),
            search: String::new(),
            search_cursor: 0,
            searching: false,
            dirty_only: false,
            sort: Sort::Repository,
            sort_reversed: false,
            busy: None,
            mutation_running: false,
            operation_scope: None,
            viewport_ready: true,
            message: String::new(),
            modal: None,
            hits: HitMap::default(),
        }
    }

    fn selected(&self) -> Option<&Worktree> {
        self.table
            .selected()
            .and_then(|row| self.visible.get(row))
            .and_then(|index| self.worktrees.get(*index))
    }

    fn pending(&self, tree: &Worktree) -> bool {
        self.pending_repos.contains(&tree.repo.common_dir)
    }

    fn rebuild(&mut self, selected: Option<PathBuf>) {
        let query = self.search.to_lowercase();
        self.visible = self
            .worktrees
            .iter()
            .enumerate()
            .filter(|(_, tree)| {
                (!self.dirty_only
                    || self.pending(tree)
                    || tree.status.is_dirty()
                    || tree.status.error.is_some())
                    && (query.is_empty()
                        || format!(
                            "{} {} {}",
                            tree.repo.name,
                            tree.branch.as_deref().unwrap_or("detached"),
                            tree.path.display()
                        )
                        .to_lowercase()
                        .contains(&query))
            })
            .map(|(index, _)| index)
            .collect();
        self.visible.sort_by(|a, b| {
            let (a, b) = (&self.worktrees[*a], &self.worktrees[*b]);
            let ordering = match self.sort {
                Sort::Repository => a.repo.path.cmp(&b.repo.path),
                Sort::Activity => b.updated_at.cmp(&a.updated_at),
                Sort::Status => status_rank(b).cmp(&status_rank(a)),
            };
            ordering.then_with(|| a.path.cmp(&b.path))
        });
        if self.sort_reversed {
            self.visible.reverse();
        }
        let row = selected
            .and_then(|path| {
                self.visible
                    .iter()
                    .position(|i| self.worktrees[*i].path == path)
            })
            .or_else(|| {
                (!self.visible.is_empty()).then(|| {
                    self.table
                        .selected()
                        .unwrap_or(0)
                        .min(self.visible.len() - 1)
                })
            });
        self.table.select(row);
    }

    fn refresh(&mut self, only: Option<PathBuf>) {
        self.start_scan(false, only);
    }

    fn rescan(&mut self) {
        self.start_scan(true, None);
    }

    fn start_scan(&mut self, full: bool, only: Option<PathBuf>) {
        if self.busy.is_some() {
            self.message = "An operation is already running.".into();
            return;
        }
        self.requested_repos = self
            .worktrees
            .iter()
            .filter(|tree| {
                only.as_ref()
                    .is_none_or(|repo| tree.repo.common_dir == *repo)
            })
            .map(|tree| tree.repo.common_dir.clone())
            .collect();
        self.pending_repos
            .extend(self.requested_repos.iter().cloned());
        self.streamed_repos.clear();
        self.scanning = true;
        self.scan_started = Some(Instant::now());
        self.scan_label = if full {
            "Discovering nested repositories"
        } else {
            "Refreshing known repositories"
        };
        self.directories = 0;
        self.update_progress();
        let selected = self.selected().map(|tree| tree.path.clone());
        self.rebuild(selected);
        let options = self.options.clone();
        let session = self.session.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let event_sender = sender.clone();
            let callback = move |event| {
                let _ = event_sender.send(WorkerMessage::Event(event));
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if full {
                    session.scan(&options, callback)
                } else {
                    session.refresh(&options, only.as_deref(), callback)
                }
            }))
            .map_err(|_| "The scan worker panicked; refresh to retry.".to_owned())
            .and_then(|result| result.map_err(|error| format!("{error:#}")));
            let _ = sender.send(WorkerMessage::Scan(result));
        });
    }

    fn update_progress(&mut self) {
        self.busy = Some(format!(
            "{}: {} directories | {} repositories updated | {} awaiting refresh",
            self.scan_label,
            self.directories,
            self.streamed_repos.len(),
            self.pending_repos.len()
        ));
    }

    fn receive_event(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Progress {
                directories,
                repositories,
            } => {
                self.directories = directories;
                self.repositories = repositories;
            }
            ScanEvent::Rows {
                repository,
                worktrees,
                refreshed,
            } => {
                let selected = self.selected().map(|tree| tree.path.clone());
                if refreshed {
                    self.pending_repos.remove(&repository.common_dir);
                    self.streamed_repos.insert(repository.common_dir.clone());
                } else {
                    self.pending_repos.insert(repository.common_dir.clone());
                }
                self.worktrees
                    .retain(|tree| tree.repo.common_dir != repository.common_dir);
                self.worktrees.extend(worktrees);
                self.marked
                    .retain(|path| self.worktrees.iter().any(|tree| tree.path == *path));
                self.repositories = self.repositories.max(self.streamed_repos.len());
                self.rebuild(selected);
            }
        }
        self.update_progress();
    }

    fn operation<F>(&mut self, label: &str, mutating: bool, scope: Option<PathBuf>, action: F)
    where
        F: FnOnce() -> Result<OperationOutcome> + Send + 'static,
    {
        self.busy = Some(label.to_owned());
        self.mutation_running = mutating;
        self.operation_scope = scope;
        let sender = self.sender.clone();
        thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action))
                .map_err(|_| "The Git operation worker panicked. Inspect the refreshed state before retrying.".to_owned())
                .and_then(|result| result.map_err(|error| format!("{error:#}")));
            let _ = sender.send(WorkerMessage::Operation(result));
        });
    }

    fn receive(&mut self, message: WorkerMessage) {
        if let WorkerMessage::Event(event) = message {
            self.receive_event(event);
            return;
        }
        let mutating = self.mutation_running;
        let scope = self.operation_scope.take();
        self.busy = None;
        self.mutation_running = false;
        self.scanning = false;
        self.scan_started = None;
        match message {
            WorkerMessage::Event(_) => unreachable!("progress handled above"),
            WorkerMessage::Scan(Ok(report)) => {
                let selected = self.selected().map(|tree| tree.path.clone());
                self.repositories = report.repositories;
                self.elapsed_ms = report.elapsed_ms;
                self.discovery_complete = report.discovery_complete;
                self.profile = report.profile;
                self.worktrees = report.worktrees;
                self.warnings = report.warnings;
                self.pending_repos.retain(|repo| {
                    !self.requested_repos.contains(repo)
                        && self
                            .worktrees
                            .iter()
                            .any(|tree| tree.repo.common_dir == *repo)
                });
                self.pending_repos.extend(report.pending_repositories);
                self.marked
                    .retain(|path| self.worktrees.iter().any(|tree| tree.path == *path));
                self.rebuild(selected);
            }
            WorkerMessage::Scan(Err(error)) => {
                self.discovery_complete = false;
                self.show_error(error);
            }
            WorkerMessage::Operation(Err(error)) => {
                self.show_error(error);
                // A bulk operation can have succeeded for some entries before failing.
                if mutating {
                    self.refresh(scope);
                }
            }
            WorkerMessage::Operation(Ok(OperationOutcome::Updated(message))) => {
                self.message = message;
                self.refresh(scope);
            }
            WorkerMessage::Operation(Ok(OperationOutcome::Notice(message))) => {
                self.message = message;
            }
            WorkerMessage::Operation(Ok(OperationOutcome::PrunePreview(repo, preview))) => {
                self.modal = Some(Modal::Confirm {
                    title: "Prune stale registrations".into(),
                    explanation: format!(
                        "Repository: {}\nScope: ALL registered worktrees in this repository, including paths outside the scan root.\nDry-run output:\n{}\n\nPruning removes stale Git metadata; branches are kept.",
                        repo.path.display(),
                        if preview.trim().is_empty() {
                            "No stale registrations reported."
                        } else {
                            &preview
                        }
                    ),
                    required: "PRUNE".into(),
                    input: String::new(),
                    cursor: 0,
                    scroll: 0,
                    action: ConfirmAction::Prune(repo),
                });
            }
        }
    }

    fn show_error(&mut self, error: String) {
        self.message = format!("Error: {error}");
        self.modal = Some(Modal::Viewer {
            title: " Operation error ".into(),
            body: error,
            scroll: 0,
        });
    }

    fn key(&mut self, key: KeyEvent) -> Intent {
        let quit_key =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if quit_key
            || (!self.viewport_ready && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc))
            || (self.modal.is_none()
                && !self.searching
                && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc))
        {
            if self.mutation_running {
                self.message =
                    "A Git change is running. Wait for completion before exiting.".into();
                return Intent::Continue;
            }
            return Intent::Quit;
        }
        if !self.viewport_ready {
            self.message = "Enlarge the terminal to inspect and manage worktrees.".into();
            return Intent::Continue;
        }
        if self.modal.is_some() {
            return self.modal_key(key);
        }
        if self.searching {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.searching = false;
            } else {
                edit_text(&mut self.search, &mut self.search_cursor, key);
                self.rebuild(None);
            }
            return Intent::Continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Intent::Quit,
            KeyCode::Down | KeyCode::Char('j') => self.navigate(1),
            KeyCode::Up | KeyCode::Char('k') => self.navigate(-1),
            KeyCode::PageDown => self.navigate(10),
            KeyCode::PageUp => self.navigate(-10),
            KeyCode::Home => self.table.select((!self.visible.is_empty()).then_some(0)),
            KeyCode::End => self.table.select(self.visible.len().checked_sub(1)),
            KeyCode::Char('/') => {
                self.searching = true;
                self.search_cursor = self.search.len();
            }
            KeyCode::Char('v') => {
                self.modal = Some(Modal::Actions {
                    selected: 0,
                    offset: 0,
                })
            }
            KeyCode::Char('s') => {
                let selected = self.selected().map(|tree| tree.path.clone());
                self.sort = self.sort.next();
                self.rebuild(selected);
            }
            KeyCode::Char('S') => {
                let selected = self.selected().map(|tree| tree.path.clone());
                self.sort_reversed = !self.sort_reversed;
                self.rebuild(selected);
            }
            KeyCode::Char('d') => {
                let selected = self.selected().map(|tree| tree.path.clone());
                self.dirty_only = !self.dirty_only;
                self.rebuild(selected);
            }
            KeyCode::Char('r') => self.refresh(None),
            KeyCode::Char('R') => self.rescan(),
            KeyCode::Char('?') => self.modal = Some(Modal::Help { scroll: 0 }),
            KeyCode::Char('i') => {
                if let Some(tree) = self.selected() {
                    self.modal = Some(Modal::Viewer {
                        title: " Worktree details ".into(),
                        body: self.inspected_details(tree).join("\n"),
                        scroll: 0,
                    });
                }
            }
            KeyCode::Char('w') => {
                self.modal = Some(Modal::Viewer {
                    title: " Scan warnings ".into(),
                    body: if self.warnings.is_empty() {
                        "No scan warnings.".into()
                    } else {
                        self.warnings.join("\n\n")
                    },
                    scroll: 0,
                });
            }
            KeyCode::Char('e') => {
                self.modal = Some(Modal::Viewer {
                    title: " Last operation ".into(),
                    body: self.message.clone(),
                    scroll: 0,
                });
            }
            KeyCode::Char('P') => {
                self.modal = Some(Modal::Viewer {
                    title: " Last completed scan performance ".into(),
                    body: serde_json::to_string_pretty(&self.profile)
                        .unwrap_or_else(|error| error.to_string()),
                    scroll: 0,
                });
            }
            KeyCode::Char(' ') => {
                if let Some(tree) = self.selected() {
                    let path = tree.path.clone();
                    if !self.marked.remove(&path) {
                        self.marked.insert(path);
                    }
                }
            }
            KeyCode::Char('c') => {
                self.marked.clear();
                self.search.clear();
                self.search_cursor = 0;
                self.dirty_only = false;
                self.rebuild(None);
            }
            KeyCode::Enter => {
                if self.mutation_running {
                    self.message =
                        "Wait for the Git change to finish before choosing a directory.".into();
                    return Intent::Continue;
                }
                if let Some(tree) = self.selected() {
                    if tree.is_bare || !tree.path.is_dir() {
                        self.message = "This entry has no accessible working directory.".into();
                    } else {
                        return Intent::Select(tree.path.clone());
                    }
                }
            }
            KeyCode::Char('o') => {
                if let Some(url) = self.selected().and_then(|tree| tree.commit_url.clone()) {
                    if self.busy.is_none() {
                        self.operation("Opening commit", false, None, move || {
                            webbrowser::open(&url)?;
                            Ok(OperationOutcome::Notice("Opened commit in browser.".into()))
                        });
                    }
                } else {
                    self.message = "No supported remote commit URL is available.".into();
                }
            }
            KeyCode::Char('a' | 'x' | 'l' | 'p' | 'm' | 'f') => {
                if self.select_only {
                    self.message = "Management is disabled in selection mode.".into();
                } else if self.busy.is_some() {
                    self.message =
                        "Wait for the current operation before changing worktrees.".into();
                } else {
                    self.manage(key.code);
                }
            }
            _ => {}
        }
        Intent::Continue
    }

    fn navigate(&mut self, delta: isize) {
        if !self.visible.is_empty() {
            let next = self
                .table
                .selected()
                .unwrap_or(0)
                .saturating_add_signed(delta)
                .min(self.visible.len() - 1);
            self.table.select(Some(next));
        }
    }

    fn manage(&mut self, key: KeyCode) {
        let Some(tree) = self.selected().cloned() else {
            self.message = "Select a worktree first.".into();
            return;
        };
        if self.pending(&tree) {
            self.message =
                "This repository's status is awaiting refresh. Press r before changing it.".into();
            return;
        }
        match key {
            KeyCode::Char('a') => {
                self.modal = Some(Modal::Form(Form::add(tree.repo)));
            }
            KeyCode::Char('x') => {
                let trees = if self.marked.is_empty() {
                    vec![tree]
                } else {
                    self.worktrees
                        .iter()
                        .filter(|tree| self.marked.contains(&tree.path))
                        .cloned()
                        .collect()
                };
                let blocked: Vec<_> = trees
                    .iter()
                    .filter_map(|tree| {
                        (if self.pending(tree) {
                            Some("status is awaiting refresh")
                        } else {
                            removal_block(tree)
                        })
                        .map(|reason| format!("{}: {reason}", tree.path.display()))
                    })
                    .collect();
                if !blocked.is_empty() {
                    self.message = format!("Removal blocked: {}", blocked.join("; "));
                    return;
                }
                let explanation = format!(
                    "Remove {} clean worktree(s):\n{}\n\nWorking directories, including ignored files, will be deleted. Branches are kept. No force is used.",
                    trees.len(),
                    trees
                        .iter()
                        .map(|tree| format!(
                            "{} [{}]",
                            tree.path.display(),
                            tree.branch.as_deref().unwrap_or("detached")
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                self.modal = Some(Modal::Confirm {
                    title: "Remove worktrees".into(),
                    explanation,
                    required: "REMOVE".into(),
                    input: String::new(),
                    cursor: 0,
                    scroll: 0,
                    action: ConfirmAction::Remove(trees),
                });
            }
            KeyCode::Char('l') => {
                if tree.is_main || tree.is_bare {
                    self.message = "Only a linked worktree can be locked or unlocked.".into();
                } else if tree.locked.is_some() {
                    self.modal = Some(Modal::Confirm {
                        title: "Unlock worktree".into(),
                        explanation: format!(
                            "{}\nLock reason: {}\nUnlocking permits subsequent removal or pruning.",
                            tree.path.display(),
                            tree.locked.as_deref().unwrap_or("")
                        ),
                        required: "UNLOCK".into(),
                        input: String::new(),
                        cursor: 0,
                        scroll: 0,
                        action: ConfirmAction::Unlock(Box::new(tree)),
                    });
                } else {
                    self.modal = Some(Modal::Form(Form::lock(tree)));
                }
            }
            KeyCode::Char('p') => {
                let repo = tree.repo;
                self.operation(
                    "Previewing stale worktree registrations",
                    false,
                    None,
                    move || {
                        let output = git::prune(&repo, true)?;
                        let registered = git::worktrees(&repo)?;
                        let stale = registered
                            .iter()
                            .filter(|tree| tree.prunable.is_some() || !tree.path.is_dir())
                            .map(|tree| {
                                format!(
                                    "{} [{}]{}",
                                    tree.path.display(),
                                    tree.branch.as_deref().unwrap_or("detached"),
                                    if tree.locked.is_some() {
                                        " (locked, protected)"
                                    } else {
                                        ""
                                    }
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let preview = format!(
                            "{output}\n\nRegistered missing/stale paths:\n{}",
                            if stale.is_empty() { "None" } else { &stale }
                        );
                        Ok(OperationOutcome::PrunePreview(repo, preview))
                    },
                );
            }
            KeyCode::Char('m') => {
                if let Some(reason) = removal_block(&tree) {
                    self.message = format!("Move blocked: {reason}");
                } else {
                    self.modal = Some(Modal::Form(Form::move_tree(tree)));
                }
            }
            KeyCode::Char('f') => {
                self.modal = Some(Modal::Confirm {
                    title: "Fetch repository".into(),
                    explanation: format!(
                        "Fetch remote updates for {}.\nThis uses your existing Git credentials and updates remote tracking references.",
                        tree.repo.path.display()
                    ),
                    required: "FETCH".into(),
                    input: String::new(),
                    cursor: 0,
                    scroll: 0,
                    action: ConfirmAction::Fetch(tree.repo),
                });
            }
            _ => {}
        }
    }

    fn modal_key(&mut self, key: KeyEvent) -> Intent {
        let Some(mut modal) = self.modal.take() else {
            return Intent::Continue;
        };
        if key.code == KeyCode::Esc {
            return Intent::Continue;
        }
        match &mut modal {
            Modal::Actions { selected, .. } => {
                let actions = menu_actions(self.select_only);
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        *selected = (*selected + 1).min(actions.len() - 1)
                    }
                    KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                    KeyCode::Home => *selected = 0,
                    KeyCode::End => *selected = actions.len() - 1,
                    KeyCode::Enter => {
                        return self.key(KeyEvent::new(actions[*selected].1, KeyModifiers::NONE));
                    }
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::Help { scroll } => {
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                    KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                    KeyCode::Home => *scroll = 0,
                    KeyCode::Char('?') | KeyCode::Enter => return Intent::Continue,
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::Viewer { scroll, .. } => {
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                    KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                    KeyCode::Home => *scroll = 0,
                    KeyCode::Enter => return Intent::Continue,
                    _ => {}
                }
                self.modal = Some(modal);
            }
            Modal::Confirm {
                input,
                required,
                cursor,
                scroll,
                ..
            } => {
                match key.code {
                    KeyCode::Down => *scroll = scroll.saturating_add(1),
                    KeyCode::Up => *scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                    KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let _ = character;
                        edit_text(input, cursor, key);
                    }
                    KeyCode::Backspace => {
                        edit_text(input, cursor, key);
                    }
                    KeyCode::Enter if input == required => {
                        if let Modal::Confirm { action, .. } = modal {
                            self.confirm(action);
                        }
                        return Intent::Continue;
                    }
                    _ => {
                        edit_text(input, cursor, key);
                    }
                }
                self.modal = Some(modal);
            }
            Modal::Form(form) => {
                match key.code {
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        form.fields[form.focus].value.clear();
                        form.fields[form.focus].cursor = 0;
                    }
                    KeyCode::Tab | KeyCode::Down => {
                        form.focus = (form.focus + 1) % form.fields.len()
                    }
                    KeyCode::BackTab | KeyCode::Up => {
                        form.focus = (form.focus + form.fields.len() - 1) % form.fields.len()
                    }
                    KeyCode::Backspace => {
                        let field = &mut form.fields[form.focus];
                        edit_text(&mut field.value, &mut field.cursor, key);
                    }
                    KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let _ = character;
                        let field = &mut form.fields[form.focus];
                        edit_text(&mut field.value, &mut field.cursor, key);
                    }
                    KeyCode::Enter => {
                        if form.focus + 1 < form.fields.len() {
                            form.focus += 1;
                        } else if let Err(error) = self.submit_form(form) {
                            form.error = error;
                        } else {
                            return Intent::Continue;
                        }
                    }
                    _ => {
                        let field = &mut form.fields[form.focus];
                        edit_text(&mut field.value, &mut field.cursor, key);
                    }
                }
                self.modal = Some(modal);
            }
        }
        Intent::Continue
    }

    fn submit_form(&mut self, form: &Form) -> std::result::Result<(), String> {
        match &form.kind {
            FormKind::Add(repo) => {
                let path = self.target_path(&form.fields[0].value)?;
                let branch = form.fields[1].value.trim().to_owned();
                if branch.is_empty() {
                    return Err("A branch name is required.".into());
                }
                let new_branch = match form.fields[2].value.trim() {
                    "yes" => true,
                    "no" => false,
                    _ => return Err("Create new branch must be yes or no.".into()),
                };
                let start = form.fields[3].value.trim();
                let options = AddOptions {
                    repo: repo.common_dir.clone(),
                    path,
                    branch,
                    new_branch,
                    start_point: (!start.is_empty()).then(|| start.to_owned()),
                };
                self.operation(
                    "Adding worktree",
                    true,
                    Some(repo.common_dir.clone()),
                    move || {
                        let path = git::add(&options)?;
                        Ok(OperationOutcome::Updated(format!(
                            "Added {}",
                            path.display()
                        )))
                    },
                );
            }
            FormKind::Lock(tree) => {
                let tree = tree.clone();
                let reason = form.fields[0].value.trim().to_owned();
                self.operation(
                    "Locking worktree",
                    true,
                    Some(tree.repo.common_dir.clone()),
                    move || {
                        git::lock(&tree, &reason)?;
                        Ok(OperationOutcome::Updated("Worktree locked.".into()))
                    },
                );
            }
            FormKind::Move(tree) => {
                let path = self.target_path(&form.fields[0].value)?;
                self.modal = Some(Modal::Confirm {
                    title: "Move worktree".into(),
                    explanation: format!("Move {}\nto {}", tree.path.display(), path.display()),
                    required: "MOVE".into(),
                    input: String::new(),
                    cursor: 0,
                    scroll: 0,
                    action: ConfirmAction::Move(tree.clone(), path),
                });
            }
        }
        Ok(())
    }

    fn target_path(&self, value: &str) -> std::result::Result<PathBuf, String> {
        if value.trim().is_empty() {
            return Err("A target directory is required.".into());
        }
        let path = PathBuf::from(value.trim());
        Ok(if path.is_absolute() {
            path
        } else {
            self.options.root.join(path)
        })
    }

    fn confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::Remove(trees) => {
                let repositories: BTreeSet<_> = trees
                    .iter()
                    .map(|tree| tree.repo.common_dir.clone())
                    .collect();
                let scope = (repositories.len() == 1)
                    .then(|| repositories.into_iter().next())
                    .flatten();
                self.operation("Removing worktrees", true, scope, move || {
                    for tree in &trees {
                        git::validate_remove(tree, &RemoveOptions::default())?;
                    }
                    let mut removed = 0;
                    for tree in trees {
                        if let Err(error) = git::remove(&tree, &RemoveOptions::default()) {
                            anyhow::bail!("Removed {removed} worktrees before stopping: {error:#}");
                        }
                        removed += 1;
                    }
                    Ok(OperationOutcome::Updated(format!(
                        "Removed {removed} worktree(s); branches kept."
                    )))
                })
            }
            ConfirmAction::Unlock(tree) => self.operation(
                "Unlocking worktree",
                true,
                Some(tree.repo.common_dir.clone()),
                move || {
                    git::unlock(&tree)?;
                    Ok(OperationOutcome::Updated("Worktree unlocked.".into()))
                },
            ),
            ConfirmAction::Prune(repo) => self.operation(
                "Pruning stale registrations",
                true,
                Some(repo.common_dir.clone()),
                move || {
                    let output = git::prune(&repo, false)?;
                    Ok(OperationOutcome::Updated(format!(
                        "Prune completed. {output}"
                    )))
                },
            ),
            ConfirmAction::Move(tree, target) => self.operation(
                "Moving worktree",
                true,
                Some(tree.repo.common_dir.clone()),
                move || {
                    git::move_worktree(&tree, &target)?;
                    Ok(OperationOutcome::Updated(format!(
                        "Moved worktree to {}",
                        target.display()
                    )))
                },
            ),
            ConfirmAction::Fetch(repo) => self.operation(
                "Fetching repository",
                true,
                Some(repo.common_dir.clone()),
                move || {
                    git::fetch(&repo)?;
                    Ok(OperationOutcome::Updated("Fetch completed.".into()))
                },
            ),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        self.hits = HitMap::default();
        self.viewport_ready = area.height >= 14 && area.width >= 30;
        if !self.viewport_ready {
            let message = format!(
                "wtm\n{} worktrees\nEnlarge terminal for dashboard.\n{}",
                self.worktrees.len(),
                if self.mutation_running {
                    "Git change running; wait before exit."
                } else {
                    "q: quit"
                }
            );
            frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), area);
            return;
        }
        let detail_height = if area.height >= 32 {
            14
        } else if area.height >= 24 {
            8
        } else {
            3
        };
        let controls = toolbar_actions(area.width, self.select_only);
        let footer_height = button_rows(&controls, area.width) + 1;
        let [header, listing, details, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(detail_height),
            Constraint::Length(footer_height),
        ])
        .areas(area);
        let sort_label = format!(
            "{}{}",
            self.sort.label(),
            if self.sort_reversed { " (reverse)" } else { "" }
        );
        let summary = format!(
            "{}{} repositories | {}/{} worktrees | {} marked | sort: {} | {}ms{}",
            if self.scanning {
                "SCANNING | "
            } else if self.discovery_complete {
                ""
            } else {
                "INCOMPLETE DISCOVERY | "
            },
            self.repositories,
            self.visible.len(),
            self.worktrees.len(),
            self.marked.len(),
            sort_label,
            self.scan_started.map_or(self.elapsed_ms, |started| started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX)),
            if self.dirty_only {
                " | changes only"
            } else {
                ""
            }
        );
        let title = format!(" wtm - {} ", safe(&self.options.root.to_string_lossy()));
        frame.render_widget(
            Paragraph::new(vec![Line::from(summary), Line::from("")])
                .block(Block::default().title(title).borders(Borders::TOP)),
            header,
        );
        let search_area = Rect::new(header.x, header.y + 2, header.width, 1);
        frame.render_widget(Paragraph::new("/"), search_area);
        self.hits.search = Some(render_input(
            frame,
            Rect::new(
                search_area.x + 1,
                search_area.y,
                search_area.width.saturating_sub(1),
                1,
            ),
            &self.search,
            self.search_cursor,
            self.searching && self.modal.is_none(),
        ));
        self.render_table(frame, listing);
        self.render_details(frame, details);
        let progress = self.busy.as_deref().unwrap_or(&self.message);
        frame.render_widget(
            Paragraph::new(safe(progress)).style(Style::default().fg(
                if self.message.starts_with("Error:") && self.busy.is_none() {
                    Color::Red
                } else {
                    Color::Yellow
                },
            )),
            Rect::new(footer.x, footer.y, footer.width, 1),
        );
        draw_buttons(
            frame,
            Rect::new(
                footer.x,
                footer.y + 1,
                footer.width,
                footer.height.saturating_sub(1),
            ),
            &controls,
            &mut self.hits.buttons,
        );
        if let Some(modal) = &mut self.modal {
            self.hits.buttons.clear();
            render_modal(frame, modal, self.select_only, &mut self.hits);
        }
    }

    fn render_table(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let wide = area.width >= 110;
        let medium = area.width >= 70;
        let (headers, widths) = if wide {
            (
                vec![
                    "",
                    "Repository",
                    "Branch",
                    "Commit",
                    "Updated",
                    "Changes",
                    "State",
                ],
                vec![
                    Constraint::Length(3),
                    Constraint::Percentage(18),
                    Constraint::Percentage(23),
                    Constraint::Length(9),
                    Constraint::Length(10),
                    Constraint::Percentage(22),
                    Constraint::Min(10),
                ],
            )
        } else if medium {
            (
                vec!["", "Repository", "Branch", "Commit", "Updated", "State"],
                vec![
                    Constraint::Length(3),
                    Constraint::Percentage(24),
                    Constraint::Percentage(28),
                    Constraint::Length(9),
                    Constraint::Length(8),
                    Constraint::Min(10),
                ],
            )
        } else {
            (
                vec!["", "Repository", "Branch", "State"],
                vec![
                    Constraint::Length(3),
                    Constraint::Percentage(30),
                    Constraint::Percentage(35),
                    Constraint::Min(8),
                ],
            )
        };
        let rows: Vec<_> = self
            .visible
            .iter()
            .map(|index| {
                let tree = &self.worktrees[*index];
                let mut cells = vec![
                    Cell::from(if self.marked.contains(&tree.path) {
                        "[x]"
                    } else {
                        "[ ]"
                    }),
                    Cell::from(safe(&tree.repo.name)),
                    Cell::from(safe(tree.branch.as_deref().unwrap_or("(detached)"))),
                ];
                if medium {
                    cells.push(Cell::from(safe(tree.short_head.as_deref().unwrap_or("-"))));
                    cells.push(Cell::from(if self.pending(tree) {
                        "pending".into()
                    } else {
                        relative_time(tree.updated_at)
                    }));
                }
                if wide {
                    cells.push(Cell::from(if self.pending(tree) {
                        "awaiting refresh".into()
                    } else {
                        safe(&tree.status.summary())
                    }));
                }
                cells.push(Cell::from(if self.pending(tree) {
                    "pending".into()
                } else {
                    state_label(tree)
                }));
                Row::new(cells).style(
                    if tree.status.error.is_some() || tree.status.conflicted > 0 {
                        Style::default().fg(Color::Red)
                    } else if self.pending(tree) || tree.status.is_dirty() {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    },
                )
            })
            .collect();
        let table_inner = Block::default().borders(Borders::ALL).inner(area);
        self.hits.table = Rect::new(
            table_inner.x,
            table_inner.y + 1,
            table_inner.width,
            table_inner.height.saturating_sub(1),
        );
        self.hits.mark = Rect::new(
            table_inner.x + 1,
            self.hits.table.y,
            3,
            self.hits.table.height,
        );
        let columns = Layout::horizontal(widths.clone())
            .flex(Flex::Start)
            .spacing(1)
            .split(Rect::new(
                table_inner.x + 1,
                table_inner.y,
                table_inner.width.saturating_sub(1),
                1,
            ));
        for (name, column) in headers.iter().zip(columns.iter()) {
            let sort = match *name {
                "Repository" => Some(Sort::Repository),
                "Updated" => Some(Sort::Activity),
                "Changes" => Some(Sort::Status),
                _ => None,
            };
            if let Some(sort) = sort {
                self.hits.headers.push((*column, sort));
            }
        }
        let table = Table::new(rows, widths)
            .flex(Flex::Start)
            .highlight_spacing(HighlightSpacing::Always)
            .header(Row::new(headers).style(Style::default().add_modifier(Modifier::BOLD)))
            .block(Block::default().title(" Worktrees ").borders(Borders::ALL))
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">");
        frame.render_stateful_widget(table, area, &mut self.table);
        if self.visible.is_empty() && area.height > 3 {
            let message = if self.busy.is_some() {
                "Scanning..."
            } else if !self.worktrees.is_empty() {
                "No worktrees match the filter. Press c to clear filters."
            } else {
                "No worktrees found. Press R for full discovery; ? for help."
            };
            frame.render_widget(
                Paragraph::new(message).wrap(Wrap { trim: false }),
                Rect {
                    x: area.x + 2,
                    y: area.y + 2,
                    width: area.width.saturating_sub(4),
                    height: area.height.saturating_sub(3),
                },
            );
        }
    }

    fn render_details(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.hits.details = area;
        let mut lines = Vec::new();
        if let Some(tree) = self.selected() {
            lines.extend(self.inspected_details(tree).into_iter().map(Line::from));
        } else {
            lines.push(Line::from(
                "Select a worktree to inspect its repository and status.",
            ));
        }
        if let Some(warning) = self.warnings.first() {
            lines.push(
                Line::from(format!(
                    "Warnings ({}): {}",
                    self.warnings.len(),
                    safe(warning)
                ))
                .style(Style::default().fg(Color::Yellow)),
            );
        }
        frame.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .title(" Details (i: full view, w: warnings) ")
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn inspected_details(&self, tree: &Worktree) -> Vec<String> {
        let mut lines = detail_lines(tree);
        if self.pending(tree) {
            lines.insert(0, "Freshness: CACHED, awaiting refresh. All metadata below is from the previous inspection.".into());
        }
        lines
    }
}

fn removal_block(tree: &Worktree) -> Option<&'static str> {
    if tree.is_main {
        Some("primary checkout is protected")
    } else if tree.is_bare {
        Some("bare repository is protected")
    } else if tree.locked.is_some() {
        Some("worktree is locked")
    } else if tree.prunable.is_some() || !tree.path.is_dir() {
        Some("stale registration; use prune preview")
    } else if tree.status.error.is_some() {
        Some("status is unknown")
    } else if tree.status.is_dirty() {
        Some("uncommitted changes are protected")
    } else {
        None
    }
}

fn status_rank(tree: &Worktree) -> usize {
    if tree.status.error.is_some() {
        usize::MAX
    } else {
        tree.status
            .conflicted
            .saturating_mul(1_000_000)
            .saturating_add(tree.status.staged)
            .saturating_add(tree.status.modified)
            .saturating_add(tree.status.untracked)
    }
}

fn state_label(tree: &Worktree) -> String {
    let mut states = Vec::new();
    if tree.is_main {
        states.push("main");
    }
    if tree.is_bare {
        states.push("bare");
    }
    if tree.locked.is_some() {
        states.push("locked");
    }
    if tree.prunable.is_some() {
        states.push("stale");
    }
    if tree.status.error.is_some() {
        states.push("unknown");
    } else if tree.status.is_dirty() {
        states.push("dirty");
    }
    if states.is_empty() {
        states.push("clean");
    }
    states.join(",")
}

/// Git metadata and paths may contain terminal escape sequences and control characters.
fn safe(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn relative_time(value: Option<chrono::DateTime<Utc>>) -> String {
    let Some(value) = value else {
        return "unknown".into();
    };
    let seconds = (Utc::now() - value).num_seconds();
    if seconds < 0 {
        "future".into()
    } else if seconds < 60 {
        "now".into()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

fn timestamp(value: Option<chrono::DateTime<Utc>>) -> String {
    value.map_or_else(
        || "unknown".into(),
        |time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

fn count(value: Option<usize>) -> String {
    value.map_or_else(|| "?".into(), |value| value.to_string())
}

fn detail_lines(tree: &Worktree) -> Vec<String> {
    let mut lines = vec![
        format!("Worktree: {}", safe(&tree.path.to_string_lossy())),
        format!("Repository: {}", safe(&tree.repo.path.to_string_lossy())),
        format!(
            "Common Git directory: {}",
            safe(&tree.repo.common_dir.to_string_lossy())
        ),
        format!(
            "Origin: {}",
            safe(tree.repo.remote.as_deref().unwrap_or("none"))
        ),
        format!(
            "HEAD: {} | {}",
            safe(tree.head.as_deref().unwrap_or("unborn")),
            safe(tree.commit_subject.as_deref().unwrap_or("no commit"))
        ),
        format!(
            "Committed: {} | Last file/Git update: {}",
            timestamp(tree.committed_at),
            timestamp(tree.updated_at)
        ),
        format!(
            "Changes: {} | State: {}",
            safe(&tree.status.summary()),
            state_label(tree)
        ),
        format!(
            "Upstream: {} | ahead {} / behind {} | merged: {}",
            safe(tree.upstream.as_deref().unwrap_or("none")),
            count(tree.ahead),
            count(tree.behind),
            tree.merged
                .map_or("unknown", |merged| if merged { "yes" } else { "no" })
        ),
    ];
    if let Some(url) = &tree.commit_url {
        lines.push(format!("Commit link (o): {}", safe(url)));
    }
    if let Some(error) = &tree.status.error {
        lines.push(format!("Status error: {}", safe(error)));
    }
    if let Some(reason) = &tree.locked {
        lines.push(format!("Lock: {}", safe(reason)));
    }
    if let Some(reason) = &tree.prunable {
        lines.push(format!("Stale registration: {}", safe(reason)));
    }
    for warning in &tree.warnings {
        lines.push(format!("Metadata warning: {}", safe(warning)));
    }
    lines
}

enum Modal {
    Actions {
        selected: usize,
        offset: usize,
    },
    Help {
        scroll: u16,
    },
    Form(Form),
    Viewer {
        title: String,
        body: String,
        scroll: u16,
    },
    Confirm {
        title: String,
        explanation: String,
        required: String,
        input: String,
        cursor: usize,
        scroll: u16,
        action: ConfirmAction,
    },
}

enum ConfirmAction {
    Remove(Vec<Worktree>),
    Unlock(Box<Worktree>),
    Prune(Repository),
    Move(Box<Worktree>, PathBuf),
    Fetch(Repository),
}

struct Field {
    label: &'static str,
    value: String,
    cursor: usize,
}

enum FormKind {
    Add(Repository),
    Lock(Box<Worktree>),
    Move(Box<Worktree>),
}

struct Form {
    kind: FormKind,
    fields: Vec<Field>,
    focus: usize,
    error: String,
}

impl Form {
    fn add(repo: Repository) -> Self {
        Self {
            kind: FormKind::Add(repo),
            fields: vec![
                Field {
                    label: "Target path (relative to scan root or absolute)",
                    value: String::new(),
                    cursor: 0,
                },
                Field {
                    label: "Branch",
                    value: String::new(),
                    cursor: 0,
                },
                Field {
                    label: "Create new branch (yes/no)",
                    value: "yes".into(),
                    cursor: 3,
                },
                Field {
                    label: "Start point (optional)",
                    value: String::new(),
                    cursor: 0,
                },
            ],
            focus: 0,
            error: String::new(),
        }
    }
    fn lock(tree: Worktree) -> Self {
        Self {
            kind: FormKind::Lock(Box::new(tree)),
            fields: vec![Field {
                label: "Lock reason (optional)",
                value: String::new(),
                cursor: 0,
            }],
            focus: 0,
            error: String::new(),
        }
    }
    fn move_tree(tree: Worktree) -> Self {
        Self {
            kind: FormKind::Move(Box::new(tree)),
            fields: vec![Field {
                label: "Target path (relative to scan root or absolute)",
                value: String::new(),
                cursor: 0,
            }],
            focus: 0,
            error: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WorktreeStatus;
    use ratatui::backend::TestBackend;
    use tempfile::TempDir;

    pub(super) fn tree(root: &std::path::Path, name: &str) -> Worktree {
        let path = root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        Worktree {
            repo: Repository {
                common_dir: root.join("repo/.git"),
                path: root.join("repo"),
                name: "nested-repository".into(),
                remote: Some("git@github.com:owner/project.git".into()),
            },
            path,
            branch: Some(name.into()),
            head: Some("0123456789abcdef0123456789abcdef01234567".into()),
            short_head: Some("0123456".into()),
            commit_subject: Some("Useful commit subject".into()),
            committed_at: Some(Utc::now() - chrono::Duration::days(7)),
            updated_at: Some(Utc::now() - chrono::Duration::hours(2)),
            status: WorktreeStatus::default(),
            locked: None,
            prunable: None,
            is_main: false,
            is_bare: false,
            commit_url: Some("https://github.com/owner/project/commit/0123456".into()),
            upstream: Some("origin/main".into()),
            ahead: Some(2),
            behind: Some(0),
            merged: Some(false),
            warnings: Vec::new(),
        }
    }

    pub(super) fn app(root: &std::path::Path, trees: Vec<Worktree>) -> App {
        let (sender, _) = mpsc::channel();
        let mut app = App::new(
            ScanOptions {
                root: root.to_owned(),
                ..ScanOptions::default()
            },
            false,
            sender,
        );
        app.worktrees = trees;
        app.repositories = 1;
        app.discovery_complete = true;
        app.rebuild(None);
        app
    }

    pub(super) fn press(app: &mut App, code: KeyCode) -> Intent {
        app.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    pub(super) fn rendered(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut result = String::new();
        for y in 0..height {
            for x in 0..width {
                result.push_str(buffer[(x, y)].symbol());
            }
            result.push('\n');
        }
        result
    }

    #[test]
    fn desktop_renders_repo_hash_activity_dirty_status_and_full_metadata() {
        let root = TempDir::new().unwrap();
        let mut tree = tree(root.path(), "feature/payments");
        tree.status.modified = 2;
        let mut app = app(root.path(), vec![tree]);
        let output = rendered(&mut app, 160, 40);
        for expected in [
            "Repository",
            "nested-repository",
            "0123456",
            "2h ago",
            "2 modified",
            "dirty",
            "Useful commit subject",
            "origin/main",
            "ahead 2 / behind 0",
            "https://github.com/owner/project/commit/0123456",
        ] {
            assert!(output.contains(expected), "missing {expected}:\n{output}");
        }
        assert!(output.contains("Committed:"));
        assert!(output.contains("Last file/Git update:"));
    }

    #[test]
    fn narrow_and_tiny_views_do_not_panic_and_preserve_navigation() {
        let root = TempDir::new().unwrap();
        let mut app = app(root.path(), vec![tree(root.path(), "topic")]);
        let narrow = rendered(&mut app, 48, 16);
        assert!(narrow.contains("Repository"));
        assert!(narrow.contains("topic"));
        assert!(narrow.contains("clean"), "missing worktree row: {narrow}");
        let tiny = rendered(&mut app, 25, 5);
        assert!(tiny.contains("Enlarge terminal"));
        assert!(matches!(press(&mut app, KeyCode::Char('q')), Intent::Quit));
        rendered(&mut app, 0, 0);
    }

    #[test]
    fn empty_and_filtered_empty_states_are_distinct() {
        let root = TempDir::new().unwrap();
        let mut app = app(root.path(), Vec::new());
        assert!(rendered(&mut app, 100, 24).contains("No worktrees found"));
        assert!(matches!(press(&mut app, KeyCode::Enter), Intent::Continue));
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.message, "Select a worktree first.");
        app.worktrees.push(tree(root.path(), "topic"));
        app.search = "does-not-match".into();
        app.rebuild(None);
        assert!(rendered(&mut app, 100, 24).contains("No worktrees match"));
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(app.visible.len(), 1);
    }

    #[test]
    fn search_finds_nested_paths_and_escape_keeps_filter_without_quitting() {
        let root = TempDir::new().unwrap();
        let trees = vec![
            tree(root.path(), "nested/topic"),
            tree(root.path(), "other"),
        ];
        let mut app = app(root.path(), trees);
        press(&mut app, KeyCode::Char('/'));
        for character in "NESTED/TOP".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        assert_eq!(app.visible.len(), 1);
        assert_eq!(
            app.selected().unwrap().branch.as_deref(),
            Some("nested/topic")
        );
        assert!(matches!(press(&mut app, KeyCode::Esc), Intent::Continue));
        assert!(!app.searching);
        assert_eq!(app.search, "NESTED/TOP");
    }

    #[test]
    fn refresh_and_sort_preserve_selected_path_and_prune_gone_marks() {
        let root = TempDir::new().unwrap();
        let first = tree(root.path(), "a");
        let mut second = tree(root.path(), "b");
        second.updated_at = Some(Utc::now());
        let mut app = app(root.path(), vec![first.clone(), second.clone()]);
        press(&mut app, KeyCode::Down);
        let selected = app.selected().unwrap().path.clone();
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.selected().unwrap().path, selected);
        assert_eq!(app.table.selected(), Some(0));
        app.marked.insert(first.path.clone());
        app.receive(WorkerMessage::Scan(Ok(ScanReport {
            pending_repositories: vec![],
            profile: Default::default(),
            root: root.path().into(),
            repositories: 1,
            worktrees: vec![second],
            warnings: vec![],
            elapsed_ms: 12,
            discovery_complete: true,
        })));
        assert_eq!(app.selected().unwrap().path, selected);
        assert!(app.marked.is_empty());
    }

    #[test]
    fn changes_filter_includes_unknown_status_but_not_clean() {
        let root = TempDir::new().unwrap();
        let clean = tree(root.path(), "clean");
        let mut unknown = tree(root.path(), "unknown");
        unknown.status.error = Some("cannot read index".into());
        let mut app = app(root.path(), vec![clean, unknown]);
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.selected().unwrap().branch.as_deref(), Some("unknown"));
        assert!(rendered(&mut app, 100, 28).contains("unknown"));
    }

    #[test]
    fn removal_requires_exact_confirmation_and_escape_preserves_directory() {
        let root = TempDir::new().unwrap();
        let tree = tree(root.path(), "feature");
        let path = tree.path.clone();
        let mut app = app(root.path(), vec![tree]);
        press(&mut app, KeyCode::Char('x'));
        let output = rendered(&mut app, 100, 24);
        assert!(output.contains("including ignored files"));
        assert!(output.contains("Branches are kept"));
        assert!(output.contains("Type REMOVE then Enter"));
        press(&mut app, KeyCode::Enter);
        assert!(app.modal.is_some());
        for character in "yes".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        press(&mut app, KeyCode::Enter);
        assert!(app.modal.is_some());
        assert!(app.busy.is_none());
        press(&mut app, KeyCode::Esc);
        assert!(app.modal.is_none());
        assert!(path.exists());
    }

    #[test]
    fn dirty_locked_main_bare_stale_and_unknown_entries_cannot_be_removed() {
        let root = TempDir::new().unwrap();
        for state in 0..6 {
            let mut tree = tree(root.path(), "feature");
            match state {
                0 => tree.status.untracked = 1,
                1 => tree.locked = Some("do not remove".into()),
                2 => tree.is_main = true,
                3 => tree.is_bare = true,
                4 => tree.prunable = Some("missing path".into()),
                5 => tree.status.error = Some("permission denied".into()),
                _ => unreachable!(),
            }
            let mut app = app(root.path(), vec![tree]);
            press(&mut app, KeyCode::Char('x'));
            assert!(app.modal.is_none(), "unsafe confirmation for state {state}");
            assert!(app.message.starts_with("Removal blocked:"));
        }
    }

    #[test]
    fn bulk_removal_is_reviewed_together_and_any_protected_entry_blocks_it() {
        let root = TempDir::new().unwrap();
        let mut dirty = tree(root.path(), "dirty");
        dirty.status.staged = 1;
        let mut app = app(root.path(), vec![tree(root.path(), "clean"), dirty]);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char('x'));
        assert!(app.modal.is_none());
        assert!(app.message.contains("uncommitted changes are protected"));
        app.worktrees[1].status.staged = 0;
        press(&mut app, KeyCode::Char('x'));
        assert!(
            matches!(&app.modal, Some(Modal::Confirm { action: ConfirmAction::Remove(trees), .. }) if trees.len() == 2)
        );
        let output = rendered(&mut app, 80, 16);
        assert!(output.contains("Type REMOVE then Enter"));
    }

    #[test]
    fn too_small_viewport_cannot_confirm_hidden_destructive_action() {
        let root = TempDir::new().unwrap();
        let mut app = app(root.path(), vec![tree(root.path(), "feature")]);
        press(&mut app, KeyCode::Char('x'));
        for character in "REMOVE".chars() {
            press(&mut app, KeyCode::Char(character));
        }
        rendered(&mut app, 20, 5);
        press(&mut app, KeyCode::Enter);
        assert!(app.busy.is_none());
        assert!(app.modal.is_some());
        assert!(app.message.contains("Enlarge"));
    }

    #[test]
    fn pending_mutations_block_quit_ctrl_c_and_selection() {
        let root = TempDir::new().unwrap();
        let mut app = app(root.path(), vec![tree(root.path(), "feature")]);
        app.busy = Some("Removing worktrees".into());
        app.mutation_running = true;
        assert!(matches!(
            press(&mut app, KeyCode::Char('q')),
            Intent::Continue
        ));
        assert!(matches!(
            app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Intent::Continue
        ));
        assert!(matches!(press(&mut app, KeyCode::Enter), Intent::Continue));
        app.mutation_running = false;
        assert!(matches!(press(&mut app, KeyCode::Char('q')), Intent::Quit));
    }

    #[test]
    fn selection_mode_blocks_writes_but_returns_selected_directory() {
        let root = TempDir::new().unwrap();
        let mut app = app(root.path(), vec![tree(root.path(), "feature")]);
        app.select_only = true;
        for key in ['a', 'x', 'l', 'p', 'm', 'f'] {
            press(&mut app, KeyCode::Char(key));
            assert!(app.modal.is_none());
            assert!(app.message.contains("disabled"));
        }
        assert!(
            matches!(press(&mut app, KeyCode::Enter), Intent::Select(path) if path == root.path().join("feature"))
        );
    }

    #[test]
    fn metadata_warning_and_error_viewers_are_scrollable_and_sanitized() {
        let root = TempDir::new().unwrap();
        let mut tree = tree(root.path(), "topic");
        tree.commit_subject = Some("subject\x1b]52;c;secret\x07".into());
        tree.warnings = vec!["metadata unavailable".into()];
        let mut app = app(root.path(), vec![tree]);
        press(&mut app, KeyCode::Char('i'));
        assert!(
            matches!(&app.modal, Some(Modal::Viewer { body, .. }) if body.contains("metadata unavailable") && !body.contains('\x1b'))
        );
        press(&mut app, KeyCode::PageDown);
        assert!(matches!(app.modal, Some(Modal::Viewer { scroll: 10, .. })));
        press(&mut app, KeyCode::Esc);
        app.receive(WorkerMessage::Scan(Err("bad\x1b[2Jscan".into())));
        let output = rendered(&mut app, 100, 24);
        assert!(output.contains("Operation error"));
        assert!(!output.contains('\x1b'));
        assert_eq!(safe("a\n\r\t\u{009b}b"), "a    b");
    }

    #[test]
    fn add_form_validates_required_fields_and_retains_input_on_error() {
        let root = TempDir::new().unwrap();
        let mut app = app(root.path(), vec![tree(root.path(), "topic")]);
        press(&mut app, KeyCode::Char('a'));
        for _ in 0..4 {
            press(&mut app, KeyCode::Enter);
        }
        assert!(
            matches!(&app.modal, Some(Modal::Form(Form { error, .. })) if error.contains("target directory"))
        );
        assert!(app.busy.is_none());
        press(&mut app, KeyCode::Esc);
        assert_eq!(
            app.target_path("linked").unwrap(),
            root.path().join("linked")
        );
        assert!(app.target_path("   ").is_err());
    }

    fn pending_scan(app: &mut App) {
        app.requested_repos = app
            .worktrees
            .iter()
            .map(|tree| tree.repo.common_dir.clone())
            .collect();
        app.pending_repos = app.requested_repos.clone();
        app.scanning = true;
        app.scan_started = Some(Instant::now());
        app.update_progress();
    }

    #[test]
    fn progressive_rows_are_visible_before_completion_and_preserve_selection_and_filters() {
        let root = TempDir::new().unwrap();
        let first = tree(root.path(), "first");
        let selected = tree(root.path(), "selected");
        let mut other = tree(root.path(), "other");
        other.repo.common_dir = root.path().join("other-repo/.git");
        other.repo.path = root.path().join("other-repo");
        other.repo.name = "another-repository".into();
        let mut app = app(
            root.path(),
            vec![first.clone(), selected.clone(), other.clone()],
        );
        app.search = "selected".into();
        app.rebuild(Some(selected.path.clone()));
        app.marked.insert(selected.path.clone());
        pending_scan(&mut app);

        app.receive(WorkerMessage::Event(ScanEvent::Progress {
            directories: 128,
            repositories: 2,
        }));
        other.status.modified = 3;
        app.receive(WorkerMessage::Event(ScanEvent::Rows {
            refreshed: true,
            repository: other.repo.clone(),
            worktrees: vec![other.clone()],
        }));
        assert_eq!(app.selected().unwrap().path, selected.path);
        assert_eq!(app.search, "selected");
        assert!(app.marked.contains(&selected.path));
        assert_eq!(
            app.worktrees
                .iter()
                .find(|tree| tree.path == other.path)
                .unwrap()
                .status
                .modified,
            3
        );
        assert!(app.busy.is_some());
        assert!(app.scanning);
        assert!(app.pending(app.selected().unwrap()));
        assert!(
            !app.pending(
                app.worktrees
                    .iter()
                    .find(|tree| tree.path == other.path)
                    .unwrap()
            )
        );
        let output = rendered(&mut app, 140, 32);
        assert!(output.contains("SCANNING"));
        assert!(output.contains("pending"));
        assert!(output.contains("128 directories"));
        press(&mut app, KeyCode::Char('x'));
        assert!(app.modal.is_none());
        assert!(app.message.contains("Wait for"));

        app.receive(WorkerMessage::Event(ScanEvent::Rows {
            refreshed: true,
            repository: selected.repo.clone(),
            worktrees: vec![selected.clone()],
        }));
        assert_eq!(app.selected().unwrap().path, selected.path);
        assert!(app.marked.contains(&selected.path));
        assert!(app.pending_repos.is_empty());
        assert_eq!(
            app.worktrees.len(),
            2,
            "completed group replaces its removed registrations"
        );
        assert!(
            app.scanning,
            "row delivery alone must not claim discovery is complete"
        );
        app.receive(WorkerMessage::Scan(Ok(ScanReport {
            pending_repositories: vec![],
            root: root.path().into(),
            discovery_complete: true,
            repositories: 2,
            worktrees: vec![selected, other],
            warnings: vec![],
            elapsed_ms: 99,
            profile: Default::default(),
        })));
        assert!(!app.scanning);
        assert!(app.busy.is_none());
        assert!(app.discovery_complete);
    }

    #[test]
    fn cached_clean_rows_awaiting_refresh_are_not_presented_as_fresh_or_hidden_by_dirty_filter() {
        let root = TempDir::new().unwrap();
        let mut app = app(root.path(), vec![tree(root.path(), "cached")]);
        pending_scan(&mut app);
        app.dirty_only = true;
        app.rebuild(None);
        assert_eq!(
            app.visible.len(),
            1,
            "pending statuses may turn dirty during refresh"
        );
        let output = rendered(&mut app, 150, 36);
        assert!(output.contains("awaiting refresh"));
        assert!(output.contains("Freshness: CACHED"));
        app.receive(WorkerMessage::Scan(Err(
            "directory became inaccessible".into()
        )));
        press(&mut app, KeyCode::Esc);
        assert!(app.pending(app.selected().unwrap()));
        assert!(!app.discovery_complete);
        press(&mut app, KeyCode::Char('l'));
        assert!(app.modal.is_none());
        assert!(app.message.contains("awaiting refresh"));
    }

    #[test]
    fn targeted_refresh_does_not_clear_unrelated_pending_statuses() {
        let root = TempDir::new().unwrap();
        let first = tree(root.path(), "first");
        let mut second = tree(root.path(), "second");
        second.repo.common_dir = root.path().join("other/.git");
        second.repo.path = root.path().join("other");
        let mut app = app(root.path(), vec![first.clone(), second.clone()]);
        pending_scan(&mut app);
        app.requested_repos = BTreeSet::from([first.repo.common_dir.clone()]);
        app.receive(WorkerMessage::Event(ScanEvent::Rows {
            refreshed: true,
            repository: first.repo.clone(),
            worktrees: vec![first.clone()],
        }));
        app.receive(WorkerMessage::Scan(Ok(ScanReport {
            pending_repositories: vec![],
            root: root.path().into(),
            discovery_complete: true,
            repositories: 2,
            worktrees: vec![first, second.clone()],
            warnings: vec![],
            elapsed_ms: 5,
            profile: Default::default(),
        })));
        assert_eq!(
            app.pending_repos,
            BTreeSet::from([second.repo.common_dir.clone()])
        );
        app.rebuild(Some(second.path.clone()));
        press(&mut app, KeyCode::Char('x'));
        assert!(app.modal.is_none());
        assert!(app.message.contains("awaiting refresh"));
    }

    #[test]
    fn scan_profile_viewer_shows_last_completed_costs_without_starting_a_scan() {
        let root = TempDir::new().unwrap();
        let mut app = app(root.path(), vec![tree(root.path(), "topic")]);
        app.profile.git_commands = 42;
        app.profile.first_result_ms = Some(10);
        app.profile.commit_cache_hits = 7;
        press(&mut app, KeyCode::Char('P'));
        assert!(
            matches!(&app.modal, Some(Modal::Viewer { body, .. }) if body.contains("\"git_commands\": 42") && body.contains("\"commit_cache_hits\": 7"))
        );
        assert!(app.busy.is_none());
    }

    #[test]
    fn failed_owner_refresh_keeps_cached_commit_and_activity_pending_after_final_report() {
        let root = TempDir::new().unwrap();
        let mut cached = tree(root.path(), "failed-owner");
        let hash = cached.head.clone();
        let activity = cached.updated_at;
        let mut app = app(root.path(), vec![cached.clone()]);
        pending_scan(&mut app);
        cached.status.error = Some("repository metadata became inaccessible".into());
        app.receive(WorkerMessage::Event(ScanEvent::Rows {
            repository: cached.repo.clone(),
            worktrees: vec![cached.clone()],
            refreshed: false,
        }));
        assert!(app.pending(app.selected().unwrap()));
        assert!(
            app.streamed_repos.is_empty(),
            "failed cached rows do not count as refreshed repositories"
        );
        app.receive(WorkerMessage::Scan(Ok(ScanReport {
            root: root.path().into(),
            discovery_complete: false,
            pending_repositories: vec![cached.repo.common_dir.clone()],
            repositories: 1,
            worktrees: vec![cached],
            warnings: vec!["owner refresh failed".into()],
            elapsed_ms: 3,
            profile: Default::default(),
        })));
        let selected = app.selected().unwrap();
        assert_eq!(selected.head, hash);
        assert_eq!(selected.updated_at, activity);
        assert!(app.pending(selected));
        assert!(app.busy.is_none());
        let output = rendered(&mut app, 150, 36);
        assert!(output.contains("INCOMPLETE DISCOVERY"));
        assert!(output.contains("awaiting refresh"));
        assert!(output.contains("Freshness: CACHED"));
        assert!(output.contains("pending"));
        for key in ['a', 'x', 'm', 'l', 'p', 'f'] {
            press(&mut app, KeyCode::Char(key));
            assert!(app.modal.is_none());
            assert!(app.message.contains("awaiting refresh"));
        }
    }
}
