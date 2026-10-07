//! Application state for the TUI: modes, focus, selection, popups, per-entry
//! commit browsing (lazily loaded histories, scroll positions, picks), and
//! the translation of terminal input into state changes.
//!
//! The state machine is deliberately terminal-free — `ui` renders it and
//! `event` feeds it — so it can be unit-tested without a tty.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use git2::Repository;
use ratatui::layout::Rect;

use crate::gitops::{self, BranchEntry};
use crate::gitx::{self, CommitLog, RepoOverview, SubmoduleEntry};
use crate::{config, network};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Repositories,
    Configurations,
}

pub struct ConfigModule {
    pub entry_idx: usize,
    pub name: String,
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
}

/// Commits materialized when an entry is first opened.
const INITIAL_WINDOW: usize = 60;
/// Extra rows materialized ahead of the viewport while scrolling.
const AHEAD_MARGIN: usize = 40;

/// What taid is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Spawned outside any git repository.
    NotRepo,
    /// The repository could not be read (bare repo, permissions, ...).
    Error,
    /// Normal operation: browsing a repository and its submodules.
    Normal,
}

/// Which pane currently receives navigation keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Repos,
    Commits,
}

impl Focus {
    pub fn toggle(self) -> Self {
        match self {
            Focus::Repos => Focus::Commits,
            Focus::Commits => Focus::Repos,
        }
    }
}

/// Modal overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Popup {
    /// Keymap reference.
    Help,
    Network,
    PullConfirm,
}

/// State of the branch-switcher popup (`b`): the entry it acts on, that
/// entry's local/remote branches, and a notice line (info or switch error).
pub struct BranchPopup {
    /// Index into the repos list (0 = main repository).
    pub entry_idx: usize,
    pub branches: Vec<BranchEntry>,
    pub scroll: ScrollState,
    pub notice: Option<String>,
}

/// One planned change in the save flow: what the main repository should
/// record for an entry after Save.
#[derive(Debug, Clone)]
pub struct SaveChange {
    pub idx: usize,
    pub name: String,
    /// What the main repository records right now.
    pub current: Option<git2::Oid>,
    /// The picked commit.
    pub pick: git2::Oid,
    /// Title of the picked commit, for the confirmation list.
    pub title: Option<String>,
    /// Set when the target's working tree is dirty (checkout may be refused).
    pub dirty_warning: Option<String>,
}

/// Outcome of applying saves: one message per success and (name, error)
/// per failure. Failures never abort the remaining items.
#[derive(Debug, Clone, Default)]
pub struct ApplyOutcome {
    pub applied: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// The save popup (`s`): confirmation first, then the apply result.
#[derive(Debug, Clone)]
pub enum SaveStage {
    Confirm(Vec<SaveChange>),
    Result(ApplyOutcome),
}

/// A control region, registered by the renderer for layout verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    PaneRepos,
    PaneCommits,
    RepoRow(usize),
    CommitRow(usize),
    BranchRow(usize),
    SavePopup,
    SaveApply,
    SaveCancel,
    SaveOk,
    OkButton,
    ConfigModule(usize),
    ConfigFile(usize),
    ParameterRow(usize),
    ParameterSave,
    ParameterBack,
    ParameterHistory,
    ConfigConfirm,
    ConfigCancel,
    HistoryRow(usize),
    HistoryChanges,
    HistoryNotes,
    HistoryCompare,
    HistoryVersion,
    HistoryRestore,
    PullPopup,
    PullConfirm,
    PullCancel,
}

/// One row of the left pane: the main repository or one submodule.
#[derive(Debug)]
pub enum Entry<'a> {
    Main(&'a RepoOverview),
    Sub(&'a SubmoduleEntry),
}

/// The entry at list index `idx` (0 = main repository, 1.. = submodules).
pub fn entry_at(ov: &RepoOverview, idx: usize) -> Option<Entry<'_>> {
    if idx == 0 {
        Some(Entry::Main(ov))
    } else {
        ov.submodules.get(idx - 1).map(Entry::Sub)
    }
}

/// Viewport heights of the panes and save review, recorded by the renderer so key
/// handling can page-scroll and keep the selection visible.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneHeights {
    pub repos: usize,
    pub commits: usize,
    pub save: usize,
}

/// A selection plus a scroll offset into a list of `len` items.
#[derive(Debug, Clone)]
pub struct ScrollState {
    len: usize,
    offset: usize,
    selected: usize,
}

impl ScrollState {
    pub fn new(len: usize) -> Self {
        Self {
            len,
            offset: 0,
            selected: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    /// The renderer accounts for variable-height wrapped commit rows.
    pub fn set_offset(&mut self, offset: usize) {
        self.offset = offset.min(self.len.saturating_sub(1));
    }

    /// Resize the list, keeping selection and offset in range.
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
        if len == 0 {
            self.selected = 0;
            self.offset = 0;
        } else {
            self.selected = self.selected.min(len - 1);
            self.offset = self.offset.min(len - 1);
        }
    }

    pub fn select(&mut self, idx: usize) {
        if self.len > 0 {
            self.selected = idx.min(self.len - 1);
        }
    }

    pub fn move_by(&mut self, delta: i32) {
        if self.len == 0 {
            return;
        }
        self.selected = (self.selected as i32 + delta).clamp(0, self.len as i32 - 1) as usize;
    }

    pub fn page_by(&mut self, viewport: usize, delta: i32) {
        self.move_by(delta * viewport.max(1) as i32);
    }

    pub fn first(&mut self) {
        if self.len > 0 {
            self.selected = 0;
        }
    }

    pub fn last(&mut self) {
        if self.len > 0 {
            self.selected = self.len - 1;
        }
    }

    /// Adjust the offset so the selected item is inside the viewport.
    pub fn ensure_visible(&mut self, viewport: usize) {
        if self.len == 0 || viewport == 0 {
            return;
        }
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + viewport {
            self.offset = self.selected + 1 - viewport;
        }
    }

    /// Rows to render: `(first_index, count)` given a viewport of `viewport`
    /// rows. Never panics and never shows rows outside the list.
    pub fn visible(&self, viewport: usize) -> (usize, usize) {
        if self.len == 0 || viewport == 0 {
            return (0, 0);
        }
        let viewport = viewport.min(self.len);
        let offset = self.offset.min(self.len - viewport);
        (offset, viewport.min(self.len - offset))
    }
}

/// Stable identity of a browsable entry, used to carry picks across
/// refreshes even if the submodule list changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKey {
    Main,
    /// Identified by its path relative to the superproject root.
    Sub(String),
}

/// Per-entry browsing state: commit history, scroll position and the
/// commit the user picked for the main repository to record.
pub struct EntryState {
    pub key: EntryKey,
    pub log: CommitLog,
    pub scroll: ScrollState,
    pub pick: Option<git2::Oid>,
    pub loaded: bool,
    /// Visible row -> index in the full commit log.
    pub visible: Vec<usize>,
    pub filtered_for: String,
    pub branch: Option<String>,
    pub updates: Option<gitx::UpdateInfo>,
    /// Physical line offset within an oversized selected commit description.
    pub commit_text_scroll: usize,
    /// Wrapped height and visible commit count recorded by the renderer.
    pub commit_text_height: usize,
    pub commit_page: usize,
}

impl EntryState {
    fn new(key: EntryKey) -> Self {
        Self {
            key,
            log: CommitLog::empty(),
            scroll: ScrollState::new(0),
            pick: None,
            loaded: false,
            visible: Vec::new(),
            filtered_for: String::new(),
            branch: None,
            updates: None,
            commit_text_scroll: 0,
            commit_text_height: 0,
            commit_page: 1,
        }
    }
}

/// The whole application state, rendered by `ui` and mutated by input.
pub struct App {
    pub mode: Mode,
    pub view: View,
    pub config_modules: Vec<ConfigModule>,
    pub config_repos: ScrollState,
    pub config_files: ScrollState,
    pub form: Option<config::Form>,
    pub global_history: Option<crate::history::Browser>,
    pub batch_review: Option<crate::session::Review>,
    pub action_note: String,
    pub action_note_editing: bool,
    /// Frozen target for the pull confirmation; None means the whole stack.
    pub pull_target: Option<network::Target>,
    session: Option<crate::session::Session>,
    session_started: std::time::SystemTime,
    pub status: String,
    pub network_results: Vec<(String, Result<String, String>)>,
    pub report_scroll: usize,
    pub network_job: Option<Receiver<network::Event>>,
    pub network_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    network_operation: Option<network::Operation>,
    shortcut: Option<(char, Instant)>,
    /// Message shown by the NotRepo/Error popup.
    pub message: String,
    /// Directory context shown in popups (cwd or repo root).
    pub context: PathBuf,
    /// Present only in `Mode::Normal`.
    pub overview: Option<RepoOverview>,
    /// The main repository (`Mode::Normal` only).
    pub repo: Option<Repository>,
    /// Lazily opened submodule repositories, parallel to `overview.submodules`.
    pub sub_repos: Vec<Option<Repository>>,
    /// Browsing state per entry, parallel to the repos list (0 = main).
    pub entries: Vec<EntryState>,
    pub focus: Focus,
    /// Selection inside the left pane (main repo + submodules).
    pub repos: ScrollState,
    pub popup: Option<Popup>,
    pub branch_popup: Option<BranchPopup>,
    pub save_popup: Option<SaveStage>,
    pub save_scroll: usize,
    /// Control regions, refreshed by the renderer for layout verification.
    pub hit_zones: Vec<(Rect, Hit)>,
    pub pane_heights: PaneHeights,
    pub should_quit: bool,
    /// Case-insensitive title/SHA filter, shared by all repository panes.
    pub filter: String,
    pub filter_editing: bool,
}

impl App {
    pub fn not_repo(cwd: &Path) -> Self {
        Self::message_mode(
            Mode::NotRepo,
            cwd,
            "This directory is not a git repository.",
        )
    }

    pub fn error(cwd: &Path, msg: impl Into<String>) -> Self {
        Self::message_mode(Mode::Error, cwd, msg)
    }

    fn message_mode(mode: Mode, cwd: &Path, msg: impl Into<String>) -> Self {
        Self {
            mode,
            view: View::Repositories,
            config_modules: Vec::new(),
            config_repos: ScrollState::new(0),
            config_files: ScrollState::new(0),
            form: None,
            global_history: None,
            batch_review: None,
            action_note: String::new(),
            action_note_editing: false,
            pull_target: None,
            session: None,
            session_started: std::time::SystemTime::now(),
            status: String::new(),
            network_results: Vec::new(),
            report_scroll: 0,
            network_job: None,
            network_cancel: None,
            network_operation: None,
            shortcut: None,
            message: msg.into(),
            context: cwd.to_path_buf(),
            overview: None,
            repo: None,
            sub_repos: Vec::new(),
            entries: Vec::new(),
            focus: Focus::Repos,
            repos: ScrollState::new(0),
            popup: None,
            branch_popup: None,
            save_popup: None,
            save_scroll: 0,
            hit_zones: Vec::new(),
            pane_heights: PaneHeights::default(),
            should_quit: false,
            filter: String::new(),
            filter_editing: false,
        }
    }

    pub fn normal(overview: RepoOverview, repo: Repository) -> Self {
        let n = overview.submodules.len();
        let mut entries = vec![EntryState::new(EntryKey::Main)];
        for sm in &overview.submodules {
            entries.push(EntryState::new(EntryKey::Sub(sm.path.clone())));
        }
        let mut app = Self {
            mode: Mode::Normal,
            view: View::Repositories,
            config_modules: Vec::new(),
            config_repos: ScrollState::new(0),
            config_files: ScrollState::new(0),
            form: None,
            global_history: None,
            batch_review: None,
            action_note: String::new(),
            action_note_editing: false,
            pull_target: None,
            session: None,
            session_started: std::time::SystemTime::now(),
            status: String::new(),
            network_results: Vec::new(),
            report_scroll: 0,
            network_job: None,
            network_cancel: None,
            network_operation: None,
            shortcut: None,
            message: String::new(),
            context: overview.root.clone(),
            overview: Some(overview),
            repo: Some(repo),
            sub_repos: (0..n).map(|_| None).collect(),
            entries,
            // Without submodules there is no repos pane to focus.
            focus: if n == 0 { Focus::Commits } else { Focus::Repos },
            repos: ScrollState::new(1 + n),
            popup: None,
            branch_popup: None,
            save_popup: None,
            save_scroll: 0,
            hit_zones: Vec::new(),
            pane_heights: PaneHeights::default(),
            should_quit: false,
            filter: String::new(),
            filter_editing: false,
        };
        app.ensure_entry_loaded(0);
        app.refresh_updates();
        app
    }

    /// True when the repository has at least one submodule (two-pane mode).
    pub fn has_submodules(&self) -> bool {
        self.overview
            .as_ref()
            .is_some_and(|ov| !ov.submodules.is_empty())
    }

    /// The entry under the left pane's cursor.
    pub fn selected_entry(&self) -> Option<Entry<'_>> {
        let ov = self.overview.as_ref()?;
        entry_at(ov, self.repos.selected())
    }

    /// The commit the main repository currently records for entry `idx`:
    /// HEAD for the main repo, the staged gitlink (falling back to the
    /// HEAD-tree value) for a submodule.
    pub fn current_oid(&self, idx: usize) -> Option<git2::Oid> {
        if idx == 0 {
            return self.overview.as_ref().and_then(|ov| ov.head_oid);
        }
        let sm = self
            .overview
            .as_ref()
            .and_then(|ov| ov.submodules.get(idx - 1))?;
        sm.index_oid.or(sm.recorded_oid)
    }

    /// The commit the submodule's own working directory is at
    /// (HEAD of the main repo for entry 0).
    pub fn workdir_oid(&self, idx: usize) -> Option<git2::Oid> {
        if idx == 0 {
            return self.overview.as_ref().and_then(|ov| ov.head_oid);
        }
        self.overview
            .as_ref()
            .and_then(|ov| ov.submodules.get(idx - 1))
            .and_then(|sm| sm.workdir_oid)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.code == KeyCode::Char('x') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(cancel) = &self.network_cancel {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                self.status = "Cancellation requested; fetches stop, an active checkout finishes, remaining repositories are skipped".into();
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && (key.code == KeyCode::Char('q')
                || (self.mode != Mode::Normal && key.code == KeyCode::Char('c')))
        {
            self.request_quit();
            return;
        }
        if self.batch_review.is_some() {
            self.handle_batch_key(key);
            return;
        }
        if self.branch_popup.is_none()
            && self.save_popup.is_none()
            && self.popup != Some(Popup::PullConfirm)
        {
            self.action_note_editing = false;
        }
        if self.action_note_editing {
            match key.code {
                KeyCode::Enter => self.action_note_editing = !self.require_action_note(),
                KeyCode::Esc => {
                    self.action_note_editing = false;
                    self.branch_popup = None;
                    self.save_popup = None;
                    self.popup = None;
                }
                KeyCode::Backspace => {
                    self.action_note.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.action_note.clear()
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.append_action_note(&c.to_string())
                }
                _ => {}
            }
            return;
        }
        if self.mode == Mode::Normal && self.handle_shortcut(key) {
            return;
        }
        if self.global_history.is_some() {
            self.handle_global_history_key(key);
            return;
        }
        if self.form.is_some() {
            self.handle_form_key(key);
            return;
        }
        match self.mode {
            Mode::NotRepo | Mode::Error => self.handle_message_key(key.code),
            Mode::Normal if self.save_popup.is_some() => self.handle_save_key(key.code),
            Mode::Normal if self.branch_popup.is_some() => self.handle_branch_key(key.code),
            Mode::Normal if self.filter_editing => self.handle_filter_key(key.code, key.modifiers),
            Mode::Normal if self.popup == Some(Popup::PullConfirm) => match key.code {
                KeyCode::Char('n') | KeyCode::Char('N') => self.action_note_editing = true,
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.start_network_target(network::Operation::Pull, self.pull_target.clone());
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => self.popup = None,
                _ => {}
            },
            Mode::Normal if self.popup.is_some() => match key.code {
                KeyCode::Esc
                | KeyCode::Enter
                | KeyCode::Char('?')
                | KeyCode::Char('q')
                | KeyCode::Char('Q') => self.popup = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    let limit = if self.popup == Some(Popup::Help) {
                        40
                    } else {
                        self.network_results.len().saturating_sub(1)
                    };
                    self.report_scroll = (self.report_scroll + 1).min(limit);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.report_scroll = self.report_scroll.saturating_sub(1)
                }
                _ => {}
            },
            Mode::Normal if self.view == View::Configurations => self.handle_config_key(key.code),
            Mode::Normal => self.handle_normal_key(key.code),
        }
    }

    fn request_quit(&mut self) {
        if self.network_job.is_some() {
            self.status = "Remote operation running; wait for it to finish before quitting".into();
            if let Some(form) = &mut self.form {
                form.notice = self.status.clone();
            }
        } else if let Some(form) = self.form.as_mut().filter(|e| e.dirty()) {
            form.notice = "Unsaved changes: Ctrl+S save or Ctrl+D discard and close".into();
        } else {
            self.should_quit = true;
        }
    }

    fn handle_shortcut(&mut self, key: KeyEvent) -> bool {
        if let Some((prefix, started)) = self.shortcut.take() {
            if started.elapsed() < Duration::from_secs(2)
                && matches!(key.code, KeyCode::Char(c) if c.to_ascii_lowercase() == prefix)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                match prefix {
                    'r' => self.switch_view(View::Repositories),
                    'c' => self.switch_view(View::Configurations),
                    'p' => self.request_pull(),
                    _ => {}
                }
                return true;
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(c @ ('r' | 'c' | 'p')) = key.code {
                self.shortcut = Some((c, Instant::now()));
                return true;
            }
        }
        false
    }

    pub fn shortcut_hint(&self) -> Option<String> {
        self.shortcut
            .filter(|(_, t)| t.elapsed() < Duration::from_secs(2))
            .map(|(c, _)| {
                format!(
                    "Press {c} again for {}",
                    match c {
                        'c' => "Configurations",
                        'r' => "Repositories",
                        _ => "Pull all",
                    }
                )
            })
    }

    fn switch_view(&mut self, view: View) {
        if let Some(form) = self.form.as_mut().filter(|e| e.dirty()) {
            form.notice = "Unsaved changes: Ctrl+S save or Ctrl+D discard and close".into();
            return;
        }
        if self.form.take().is_some() {
            self.refresh();
        }
        self.popup = None;
        self.global_history = None;
        self.save_popup = None;
        self.branch_popup = None;
        self.filter_editing = false;
        self.view = view;
        if view == View::Configurations {
            self.discover_configs();
        }
        self.focus = if view == View::Repositories && !self.has_submodules() {
            Focus::Commits
        } else {
            Focus::Repos
        };
    }

    pub fn discover_configs(&mut self) {
        let selected = self
            .config_modules
            .get(self.config_repos.selected())
            .map(|m| m.root.clone());
        self.config_modules.clear();
        if let Some(ov) = &self.overview {
            for (i, sm) in ov.submodules.iter().enumerate() {
                if let Some(root) = &sm.workdir {
                    match config::discover(root) {
                        Ok(files) if !files.is_empty() => self.config_modules.push(ConfigModule {
                            entry_idx: i + 1,
                            name: sm.path.clone(),
                            root: root.clone(),
                            files,
                        }),
                        Err(e) => self.status = format!("{}: {e:#}", sm.path),
                        _ => {}
                    }
                }
            }
        }
        self.config_repos.set_len(self.config_modules.len());
        if let Some(idx) = self
            .config_modules
            .iter()
            .position(|m| Some(&m.root) == selected.as_ref())
        {
            self.config_repos.select(idx);
        }
        self.select_config_module(self.config_repos.selected());
    }

    fn select_config_module(&mut self, idx: usize) {
        let previous = self.config_repos.selected();
        self.config_repos.select(idx);
        let len = self
            .config_modules
            .get(self.config_repos.selected())
            .map(|m| m.files.len())
            .unwrap_or(0);
        if previous != self.config_repos.selected() {
            self.config_files = ScrollState::new(len);
        } else {
            self.config_files.set_len(len);
        }
        self.config_repos
            .ensure_visible(self.pane_heights.repos.max(1));
    }

    fn handle_config_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('p') => self.request_selected_pull(),
            KeyCode::Char('h') | KeyCode::Char('H') => self.open_global_history(),
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => self.request_quit(),
            KeyCode::Char('?') => {
                self.report_scroll = 0;
                self.popup = Some(Popup::Help);
            }
            KeyCode::Char('n') => {
                self.report_scroll = 0;
                self.popup = Some(Popup::Network);
            }
            KeyCode::Char('r') => {
                self.refresh();
                self.discover_configs();
            }
            KeyCode::Tab | KeyCode::BackTab => self.focus = self.focus.toggle(),
            KeyCode::Left => self.focus = Focus::Repos,
            KeyCode::Right => self.focus = Focus::Commits,
            KeyCode::Enter => {
                if self.focus == Focus::Repos {
                    self.focus = Focus::Commits;
                } else {
                    self.open_config();
                }
            }
            _ => {
                let (scroll, page) = if self.focus == Focus::Repos {
                    (&mut self.config_repos, self.pane_heights.repos)
                } else {
                    (&mut self.config_files, self.pane_heights.commits)
                };
                let previous = scroll.selected();
                match code {
                    KeyCode::Up | KeyCode::Char('k') => scroll.move_by(-1),
                    KeyCode::Down | KeyCode::Char('j') => scroll.move_by(1),
                    KeyCode::PageUp => scroll.page_by(page, -1),
                    KeyCode::PageDown => scroll.page_by(page, 1),
                    KeyCode::Home => scroll.first(),
                    KeyCode::End => scroll.last(),
                    _ => {}
                }
                let selected = scroll.selected();
                scroll.ensure_visible(page.max(1));
                if self.focus == Focus::Repos && previous != selected {
                    self.config_files = ScrollState::new(0);
                    self.select_config_module(selected);
                }
            }
        }
    }

    fn open_config(&mut self) {
        if self.is_pulling() {
            self.status = "Wait for the remote operation before editing config files".into();
            return;
        }
        let path = self
            .config_modules
            .get(self.config_repos.selected())
            .and_then(|m| m.files.get(self.config_files.selected()))
            .cloned();
        if let Some(path) = path {
            let root = &self.overview.as_ref().unwrap().root;
            match config::Form::open_in(path, root) {
                Ok(form) => self.form = Some(form),
                Err(e) => self.status = format!("{e:#}"),
            }
        }
    }

    fn open_global_history(&mut self) {
        if let Some(form) = self.form.as_mut().filter(|f| f.dirty()) {
            form.notice =
                "Save or discard pending config edits before opening global history".into();
            return;
        }
        let Some(ov) = &self.overview else {
            return;
        };
        match crate::history::Browser::open(&ov.root) {
            Ok(browser) => {
                self.global_history = Some(browser);
                self.popup = None;
                self.save_popup = None;
                self.branch_popup = None;
                self.filter_editing = false;
            }
            Err(e) => {
                self.status = format!("Global history unavailable: {e:#}");
                if let Some(form) = &mut self.form {
                    form.notice = self.status.clone();
                }
            }
        }
    }

    fn handle_global_history_key(&mut self, key: KeyEvent) {
        let action = self.global_history.as_mut().unwrap().key(
            key.code,
            key.modifiers,
            self.pane_heights.commits.max(1),
        );
        match action {
            crate::history::BrowserAction::None => {}
            crate::history::BrowserAction::Close => self.global_history = None,
            crate::history::BrowserAction::Restore => {
                if let Err(e) = self.restore_global_history() {
                    self.global_history.as_mut().unwrap().notice =
                        format!("Cannot load restore: {e:#}");
                }
            }
            crate::history::BrowserAction::RestoreBatch => {
                let result = (|| -> Result<crate::session::Review> {
                    if self.is_pulling() {
                        anyhow::bail!("wait for the pull operation before restoring");
                    }
                    if self.form.as_ref().is_some_and(|f| f.dirty()) {
                        anyhow::bail!("save/discard current edits before restoring");
                    }
                    let browser = self.global_history.as_ref().unwrap();
                    Ok(crate::session::Review::prepare_project(
                        &browser.root,
                        browser.targets()?,
                        browser.repository_targets()?,
                        browser.before,
                        format!(
                            "{} snapshot · {}",
                            if browser.before { "before" } else { "after" },
                            browser.selected_label()
                        ),
                        browser.recovery_id(),
                    ))
                })();
                match result {
                    Ok(review) => self.batch_review = Some(review),
                    Err(e) => {
                        self.global_history.as_mut().unwrap().notice =
                            format!("Cannot review restore: {e:#}")
                    }
                }
            }
        }
    }

    fn ensure_session(&mut self) -> Result<String> {
        if self.session.is_none() {
            let root = &self.overview.as_ref().context("no opened checkout")?.root;
            self.session = Some(crate::session::Session::begin(root, self.session_started)?);
        }
        Ok(self.session.as_ref().unwrap().id.clone())
    }

    pub fn finish_session(&self) -> Result<()> {
        if let Some(session) = &self.session {
            session.finish()?;
        }
        Ok(())
    }

    fn close_batch_review(&mut self) {
        self.batch_review = None;
        self.form = None; // Any underlying form was clean; reopen from current disk.
        self.refresh();
        if let Some(browser) = &mut self.global_history {
            if let Err(e) = browser.reload() {
                browser.notice = format!("History reload failed: {e:#}");
            }
        }
    }

    fn handle_batch_key(&mut self, key: KeyEvent) {
        let review = self.batch_review.as_mut().unwrap();
        if review.note_editing {
            match key.code {
                KeyCode::Enter => {
                    review.note_editing = crate::session::require_note(&review.note).is_err()
                }
                KeyCode::Esc => self.close_batch_review(),
                KeyCode::Backspace => {
                    review.note.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    review.note.clear()
                }
                KeyCode::Char(c)
                    if !c.is_control()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && review.note.chars().count() < 500 =>
                {
                    review.note.push(c)
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_batch_review(),
            KeyCode::Enter if review.outcome.is_some() => self.close_batch_review(),
            KeyCode::Char('n') if review.outcome.is_none() => review.note_editing = true,
            KeyCode::Enter => {
                if crate::session::require_note(&review.note).is_err() {
                    review.note_editing = true;
                    return;
                }
                let result = (|| -> Result<()> {
                    if self.is_pulling() {
                        anyhow::bail!("remote operation in progress; nothing was written");
                    }
                    let id = self.ensure_session()?;
                    let root = self.overview.as_ref().unwrap().root.clone();
                    self.batch_review.as_mut().unwrap().confirm(&root, &id)
                })();
                if let Err(e) = result {
                    self.batch_review
                        .as_mut()
                        .unwrap()
                        .errors
                        .push(format!("{e:#}"));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => review.scroll = review.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => review.scroll = review.scroll.saturating_sub(1),
            KeyCode::PageDown => {
                review.scroll = review
                    .scroll
                    .saturating_add(self.pane_heights.commits.max(1))
            }
            KeyCode::PageUp => {
                review.scroll = review
                    .scroll
                    .saturating_sub(self.pane_heights.commits.max(1))
            }
            KeyCode::Home => review.scroll = 0,
            KeyCode::End => review.scroll = usize::MAX,
            _ => {}
        }
    }

    fn restore_global_history(&mut self) -> Result<()> {
        if self.is_pulling() {
            anyhow::bail!("wait for the pull operation before loading a restore");
        }
        if self.form.as_ref().is_some_and(|f| f.dirty()) {
            anyhow::bail!("save or discard current edits before restoring");
        }
        let browser = self.global_history.as_ref().unwrap();
        let entry = browser.selected().context("no entry selected")?;
        // Reverify the record on restore, then resolve only a regular file inside
        // this opened checkout. Historical entries remain viewable if removed.
        let record = crate::history::Store::load_global(&browser.root, entry)?;
        let path = crate::history::resolve_config(&browser.root, &record.path)?;
        let mut form = config::Form::open_in(path.clone(), &browser.root)?;
        form.restore(
            if browser.before {
                &record.before
            } else {
                &record.after
            },
            format!(
                "{}:{}:{}",
                record.path,
                record.id,
                if browser.before { "before" } else { "after" }
            ),
        )?;
        self.global_history = None;
        self.view = View::Configurations;
        self.discover_configs();
        if let Some(module_idx) = self.config_modules.iter().position(|m| {
            m.files
                .iter()
                .any(|p| p.canonicalize().ok().as_ref() == Some(&path))
        }) {
            self.select_config_module(module_idx);
            if let Some(file_idx) = self.config_modules[module_idx]
                .files
                .iter()
                .position(|p| p.canonicalize().ok().as_ref() == Some(&path))
            {
                self.config_files.select(file_idx);
            }
        }
        self.focus = Focus::Commits;
        self.form = Some(form);
        Ok(())
    }

    fn handle_form_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('h')
            && self
                .form
                .as_ref()
                .is_some_and(|f| f.overlay.is_none() && f.editing.is_none() && !f.filtering)
        {
            self.open_global_history();
            return;
        }
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            match self.ensure_session() {
                Ok(id) => self
                    .form
                    .as_mut()
                    .unwrap()
                    .set_session(&self.overview.as_ref().unwrap().root, &id),
                Err(e) => {
                    self.form.as_mut().unwrap().notice =
                        format!("Session journal unavailable; nothing saved: {e:#}");
                    return;
                }
            }
        }
        let form = self.form.as_mut().unwrap();
        if form.overlay.is_some() {
            if form.overlay_key(key.code, key.modifiers, self.pane_heights.commits) {
                self.refresh();
            }
            return;
        }
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if let Err(e) = form.request_save() {
                form.notice = format!("{e:#}");
            }
        } else if key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.form = None;
            self.refresh();
        } else if key.code == KeyCode::Esc && (form.editing.is_some() || form.filtering) {
            form.key(key.code, key.modifiers, self.pane_heights.commits);
        } else if key.code == KeyCode::Esc {
            if form.dirty() {
                form.notice = "Unsaved changes: Ctrl+S save or Ctrl+D discard and close".into();
            } else {
                self.form = None;
                self.refresh();
            }
        } else {
            form.key(key.code, key.modifiers, self.pane_heights.commits);
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.action_note_editing {
            self.append_action_note(text);
            return;
        }
        if let Some(review) = &mut self.batch_review {
            if review.note_editing {
                review.note.extend(
                    text.chars()
                        .filter(|c| !c.is_control())
                        .take(500usize.saturating_sub(review.note.chars().count())),
                );
            }
            return;
        }
        if let Some(history) = &mut self.global_history {
            history.paste(text);
            return;
        }
        if let Some(form) = &mut self.form {
            form.paste(text);
        }
    }

    fn request_pull(&mut self) {
        self.request_pull_target(None);
    }

    fn request_selected_pull(&mut self) {
        let idx = if let Some(branch) = &self.branch_popup {
            branch.entry_idx
        } else if self.view == View::Configurations {
            let Some(module) = self.config_modules.get(self.config_repos.selected()) else {
                return;
            };
            module.entry_idx
        } else {
            self.repos.selected()
        };
        let Some(ov) = &self.overview else {
            return;
        };
        let target = if idx == 0 {
            network::Target {
                name: self.entry_name(0),
                path: Some(ov.root.clone()),
            }
        } else {
            let Some(module) = ov.submodules.get(idx - 1) else {
                return;
            };
            network::Target {
                name: module.path.clone(),
                path: module.workdir.clone(),
            }
        };
        if target.path.is_none() {
            self.status = format!(
                "{} is not initialized; initialize this submodule before pulling",
                target.name
            );
            return;
        }
        self.request_pull_target(Some(target));
    }

    fn request_pull_target(&mut self, target: Option<network::Target>) {
        if !self.can_start_network(network::Operation::Pull, target.as_ref()) {
            return;
        }
        self.pull_target = target;
        self.branch_popup = None;
        self.save_popup = None;
        self.filter_editing = false;
        self.global_history = None;
        self.action_note.clear();
        self.action_note_editing = true;
        self.popup = Some(Popup::PullConfirm);
    }

    fn append_action_note(&mut self, text: &str) {
        self.action_note.extend(
            text.chars()
                .filter(|c| !c.is_control())
                .take(500usize.saturating_sub(self.action_note.chars().count())),
        );
    }

    fn require_action_note(&mut self) -> bool {
        if let Err(error) = crate::session::require_note(&self.action_note) {
            self.action_note_editing = true;
            self.status = error.to_string();
            if let Some(branch) = &mut self.branch_popup {
                branch.notice = Some(error.to_string());
            }
            false
        } else {
            if self.status.starts_with("A note is required:") {
                self.status.clear();
            }
            true
        }
    }

    fn can_start_network(
        &mut self,
        operation: network::Operation,
        target: Option<&network::Target>,
    ) -> bool {
        if self.mode != Mode::Normal {
            return false;
        }
        if self.network_job.is_some() {
            self.status = "Remote operation already running; retry after it finishes".into();
            return false;
        }
        if let Some(form) = self.form.as_mut() {
            form.notice = "Close the config form before pulling".into();
            return false;
        }
        let pending = self.entries.iter().any(|entry| {
            entry.pick.is_some()
                && target.is_none_or(|target| match &entry.key {
                    EntryKey::Main => self
                        .overview
                        .as_ref()
                        .is_some_and(|ov| target.path.as_ref() == Some(&ov.root)),
                    EntryKey::Sub(path) => target.name == *path,
                })
        });
        if operation == network::Operation::Pull && pending {
            self.status = if target.is_some() {
                "Apply or clear pending commit picks for the selected repository before pulling it"
            } else {
                "Apply or clear pending commit picks before pulling all repositories"
            }
            .into();
            return false;
        }
        self.overview.is_some()
    }

    pub fn start_network(&mut self, operation: network::Operation) {
        self.start_network_target(operation, None);
    }

    fn start_network_target(
        &mut self,
        operation: network::Operation,
        target: Option<network::Target>,
    ) {
        if !self.can_start_network(operation, target.as_ref()) {
            return;
        }
        if operation == network::Operation::Pull && !self.require_action_note() {
            return;
        }
        self.action_note_editing = false;
        let Some(ov) = &self.overview else {
            return;
        };
        // Snapshot all targets before the superproject's pull can change .gitmodules.
        let selected_name = target.as_ref().map(|target| target.name.clone());
        let targets = if let Some(target) = target {
            vec![target]
        } else {
            let mut targets = vec![network::Target {
                name: self.entry_name(0),
                path: Some(ov.root.clone()),
            }];
            for sm in &ov.submodules {
                targets.push(network::Target {
                    name: sm.path.clone(),
                    path: sm.workdir.clone(),
                });
            }
            targets
        };
        let root = ov.root.clone();
        let recording = if operation == network::Operation::Pull {
            match self
                .ensure_session()
                .and_then(|session| Ok((session, crate::session::identifier()?)))
            {
                Ok((session, note_group)) => Some(network::Recording {
                    root,
                    session,
                    note: self.action_note.clone(),
                    note_group,
                }),
                Err(e) => {
                    self.status = format!("Pull not started: history unavailable: {e:#}");
                    return;
                }
            }
        } else {
            None
        };
        self.status = if operation == network::Operation::Pull {
            selected_name
                .map(|name| format!("Pulling only {name}…"))
                .unwrap_or_else(|| "Pulling all repositories…".into())
        } else {
            "Checking remote updates…".into()
        };
        self.network_operation = Some(operation);
        if operation == network::Operation::Pull {
            self.branch_popup = None;
            self.save_popup = None;
            self.popup = None;
            self.filter_editing = false;
        }
        let (receiver, cancel) = network::start_recorded(targets, operation, recording);
        self.network_job = Some(receiver);
        self.network_cancel = Some(cancel);
    }

    pub fn poll_network(&mut self) {
        let Some(rx) = self.network_job.as_ref() else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.network_job = None;
                self.network_cancel = None;
                self.network_operation = None;
                self.network_results.push(("Worker".into(), Err("Remote worker stopped unexpectedly; inspect git status before retrying; outcomes may be partial".into())));
                self.status = "Remote worker stopped unexpectedly; inspect results and repository status before retrying".into();
                self.refresh();
                self.popup = Some(Popup::Network);
                return;
            }
        };
        match event {
            network::Event::Progress(text) => self.status = text,
            network::Event::Done(results) => {
                let failed = results.iter().filter(|(_, r)| r.is_err()).count();
                self.network_results = results;
                self.network_job = None;
                self.network_cancel = None;
                let pulling = self.network_operation.take() == Some(network::Operation::Pull);
                self.status = format!(
                    "{} complete: {} succeeded, {failed} failed · n results",
                    if pulling { "Pull" } else { "Update check" },
                    self.network_results.len() - failed
                );
                self.refresh();
                if self.view == View::Configurations {
                    self.discover_configs();
                }
                if !pulling {
                    if let Some(idx) = self.branch_popup.as_ref().map(|p| p.entry_idx) {
                        self.ensure_sub_repo(idx);
                        let repo = if idx == 0 {
                            self.repo.as_ref()
                        } else {
                            self.sub_repos[idx - 1].as_ref()
                        };
                        if let (Some(repo), Some(popup)) = (repo, self.branch_popup.as_mut()) {
                            match gitops::branches(repo) {
                                Ok(branches) => {
                                    popup.scroll = ScrollState::new(branches.len());
                                    popup.branches = branches;
                                    popup.notice = Some(format!(
                                        "Remote branches refreshed · {failed} fetch failures · Esc close, then n results"
                                    ));
                                }
                                Err(e) => {
                                    popup.notice = Some(format!("Cannot reload branches: {e:#}"))
                                }
                            }
                        }
                    }
                }
                if pulling {
                    self.report_scroll = 0;
                    self.popup = Some(Popup::Network);
                }
            }
        }
    }

    fn refresh_updates(&mut self) {
        for idx in 0..self.entries.len() {
            self.ensure_sub_repo(idx);
            let repo = if idx == 0 {
                self.repo.as_ref()
            } else {
                self.sub_repos[idx - 1].as_ref()
            };
            if let Some(repo) = repo {
                let entry = &mut self.entries[idx];
                entry.branch = gitx::selected_branch(repo, entry.branch.as_deref());
                entry.updates = gitx::update_info(repo, entry.branch.as_deref());
            }
        }
    }

    fn is_pulling(&self) -> bool {
        self.network_job.is_some() && self.network_operation == Some(network::Operation::Pull)
    }

    fn handle_message_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                self.should_quit = true;
            }
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('p') => self.request_selected_pull(),
            KeyCode::Char('h') | KeyCode::Char('H') => self.open_global_history(),
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => self.request_quit(),
            KeyCode::Char('n') => {
                self.report_scroll = 0;
                self.popup = Some(Popup::Network);
            }
            KeyCode::Char('?') => {
                self.report_scroll = 0;
                self.popup = Some(Popup::Help);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => self.refresh(),
            KeyCode::Char('b') | KeyCode::Char('B') => self.open_branch_popup(),
            KeyCode::Char('s') | KeyCode::Char('S') => self.open_save_popup(),
            KeyCode::Char('/') => {
                self.focus = Focus::Commits;
                self.filter_editing = true;
            }
            KeyCode::Tab | KeyCode::BackTab if self.has_submodules() => {
                self.focus = self.focus.toggle()
            }
            KeyCode::Left if self.has_submodules() => self.focus = Focus::Repos,
            KeyCode::Right => self.focus = Focus::Commits,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.page_selection(-1),
            KeyCode::PageDown => self.page_selection(1),
            KeyCode::Home => self.first_selection(),
            KeyCode::End => self.last_selection(),
            KeyCode::Enter if self.focus == Focus::Commits => self.toggle_pick(),
            _ => {}
        }
    }

    fn handle_filter_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Enter => self.filter_editing = false,
            KeyCode::Esc => {
                self.filter_editing = false;
                self.filter.clear();
                self.apply_filter();
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.apply_filter();
            }
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.clear();
                self.apply_filter();
            }
            KeyCode::Char(c)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.filter.push(c);
                self.apply_filter();
            }
            _ => {}
        }
    }

    fn apply_filter(&mut self) {
        let idx = self.repos.selected();
        self.apply_filter_to_entry(idx);
    }

    fn apply_filter_to_entry(&mut self, idx: usize) {
        let App {
            repo,
            sub_repos,
            entries,
            filter,
            ..
        } = self;
        let Some(entry) = entries.get_mut(idx) else {
            return;
        };
        if !filter.is_empty() {
            let entry_repo = if idx == 0 {
                repo.as_ref()
            } else {
                sub_repos.get(idx - 1).and_then(|r| r.as_ref())
            };
            if let Some(r) = entry_repo {
                let _ = entry.log.ensure_loaded(r, entry.log.len());
            }
        }
        let needle = filter.to_lowercase();
        entry.visible = (0..entry.log.len())
            .filter(|&i| {
                needle.is_empty()
                    || entry.log.get(i).is_some_and(|c| {
                        c.title.to_lowercase().contains(&needle)
                            || c.oid.to_string().contains(&needle)
                            || c.short_sha.to_lowercase().contains(&needle)
                    })
            })
            .collect();
        entry.scroll = ScrollState::new(entry.visible.len());
        entry.commit_text_scroll = 0;
        entry.commit_text_height = 0;
        entry.commit_page = 1;
        entry.filtered_for = filter.clone();
    }

    /// Mouse input is deliberately ignored: taid is keyboard-only.
    pub fn handle_mouse(&mut self, _mouse: MouseEvent) {}

    /// Select entry `idx` in the repos pane and make sure its history is
    /// loaded (the public selection entry point used by keys and tests).
    pub fn select_repo(&mut self, idx: usize) {
        self.repos.select(idx);
        self.ensure_entry_loaded(self.repos.selected());
    }

    // ---- branch switcher -------------------------------------------------

    /// Rows per page inside the branch popup.
    const BRANCH_PAGE: i32 = 8;

    fn handle_branch_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('p') => self.request_selected_pull(),
            KeyCode::Char('f') => self.start_network(network::Operation::Check),
            KeyCode::Char('n') => self.action_note_editing = true,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => self.branch_popup = None,
            KeyCode::Up | KeyCode::Char('k') => self.branch_scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => self.branch_scroll(1),
            KeyCode::PageUp => self.branch_scroll(-Self::BRANCH_PAGE),
            KeyCode::PageDown => self.branch_scroll(Self::BRANCH_PAGE),
            KeyCode::Home => {
                if let Some(bp) = self.branch_popup.as_mut() {
                    bp.scroll.first();
                }
            }
            KeyCode::End => {
                if let Some(bp) = self.branch_popup.as_mut() {
                    bp.scroll.last();
                }
            }
            KeyCode::Enter => self.switch_selected_branch(),
            _ => {}
        }
    }

    fn branch_scroll(&mut self, delta: i32) {
        if let Some(bp) = self.branch_popup.as_mut() {
            bp.scroll.move_by(delta);
        }
    }

    /// Open the branch switcher for the entry selected in the repos pane.
    fn open_branch_popup(&mut self) {
        if self.is_pulling() {
            self.status = "Wait for the remote operation before switching branches".into();
            return;
        }
        if self.mode != Mode::Normal {
            return;
        }
        let idx = self.repos.selected();
        self.ensure_sub_repo(idx);
        let (notice, branches) = if idx > 0
            && self
                .overview
                .as_ref()
                .and_then(|ov| ov.submodules.get(idx - 1))
                .is_some_and(|sm| !sm.initialized)
        {
            (
                Some("submodule is not initialized — nothing to switch".to_string()),
                Vec::new(),
            )
        } else {
            let repo = if idx == 0 {
                self.repo.as_ref()
            } else {
                self.sub_repos.get(idx - 1).and_then(|c| c.as_ref())
            };
            match repo {
                Some(r) => (None, gitops::branches(r).unwrap_or_default()),
                None => (Some("cannot open this repository".to_string()), Vec::new()),
            }
        };
        let notice =
            notice.or_else(|| (branches.is_empty()).then(|| "no branches yet".to_string()));

        let len = branches.len();
        self.action_note.clear();
        self.action_note_editing = false;
        self.branch_popup = Some(BranchPopup {
            entry_idx: idx,
            branches,
            scroll: ScrollState::new(len),
            notice,
        });
    }

    /// Switch the popup's entry to the selected branch, then refresh
    /// everything. A refused switch (dirty tree) keeps the popup open with
    /// the error as its notice.
    fn switch_selected_branch(&mut self) {
        self.action_note_editing = false;
        if self.network_job.is_some() {
            if let Some(popup) = &mut self.branch_popup {
                popup.notice =
                    Some("Wait for the remote operation before switching branches".into());
            }
            return;
        }
        let Some((idx, name, is_remote)) = self.branch_popup.as_ref().and_then(|bp| {
            bp.branches
                .get(bp.scroll.selected())
                .map(|b| (bp.entry_idx, b.name.clone(), b.is_remote))
        }) else {
            return;
        };
        if !self.require_action_note() {
            return;
        }
        let session = match self.ensure_session() {
            Ok(id) => id,
            Err(e) => {
                self.branch_popup.as_mut().unwrap().notice =
                    Some(format!("History unavailable; branch unchanged: {e:#}"));
                return;
            }
        };
        let root = self.overview.as_ref().unwrap().root.clone();
        let module = if idx == 0 {
            ".".to_owned()
        } else {
            self.overview.as_ref().unwrap().submodules[idx - 1]
                .path
                .clone()
        };
        self.ensure_sub_repo(idx);
        let repo = if idx == 0 {
            self.repo.as_ref()
        } else {
            self.sub_repos.get(idx - 1).and_then(|c| c.as_ref())
        };
        let Some(repo) = repo else {
            return;
        };
        match crate::repository_history::run_noted(
            &root,
            &session,
            "branch switch",
            &module,
            &name,
            &self.action_note,
            || {
                if is_remote {
                    gitops::switch_remote_branch(repo, &name)?;
                } else {
                    gitops::switch_branch(repo, &name)?;
                }
                Ok(format!("Selected branch {name}"))
            },
        ) {
            Ok(_) => {
                self.branch_popup = None;
                self.refresh();
            }
            Err(e) => {
                if let Some(bp) = self.branch_popup.as_mut() {
                    bp.notice = Some(format!("{e:#}"));
                }
            }
        }
    }

    // ---- save flow --------------------------------------------------------

    fn handle_save_key(&mut self, code: KeyCode) {
        let confirming = matches!(self.save_popup, Some(SaveStage::Confirm(_)));
        match code {
            KeyCode::Up | KeyCode::Char('k') if confirming => {
                self.save_scroll = self.save_scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') if confirming => {
                self.save_scroll = self.save_scroll.saturating_add(1)
            }
            KeyCode::PageUp if confirming => {
                self.save_scroll = self
                    .save_scroll
                    .saturating_sub(self.pane_heights.save.max(1))
            }
            KeyCode::PageDown if confirming => {
                self.save_scroll = self
                    .save_scroll
                    .saturating_add(self.pane_heights.save.max(1))
            }
            KeyCode::Home if confirming => self.save_scroll = 0,
            KeyCode::End if confirming => self.save_scroll = usize::MAX,
            KeyCode::Char('n') if confirming => self.action_note_editing = true,
            KeyCode::Enter if confirming => self.apply_saves(),
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => self.save_popup = None,
            KeyCode::Enter => self.save_popup = None, // result popup: close
            _ => {}
        }
    }

    /// `s`: open the confirmation popup with every pending pick. State is
    /// refreshed first so warnings reflect the working trees as they are
    /// right now, not as they were at startup.
    fn open_save_popup(&mut self) {
        if self.is_pulling() {
            self.status = "Wait for the remote operation before applying commit picks".into();
            return;
        }
        if self.mode != Mode::Normal {
            return;
        }
        self.refresh();
        let changes = self.pending_saves();
        self.action_note.clear();
        self.action_note_editing = !changes.is_empty();
        self.save_scroll = 0;
        self.save_popup = Some(SaveStage::Confirm(changes));
    }

    /// All picks that would change something, in entry order (main first).
    fn pending_saves(&mut self) -> Vec<SaveChange> {
        let mut changes = Vec::new();
        for idx in 0..self.entries.len() {
            let Some(pick) = self.entries[idx].pick else {
                continue;
            };
            let current = self.current_oid(idx);
            if current == Some(pick) {
                continue;
            }
            self.ensure_sub_repo(idx);
            let repo = if idx == 0 {
                self.repo.as_ref()
            } else {
                self.sub_repos.get(idx - 1).and_then(|c| c.as_ref())
            };
            let title = repo
                .and_then(|r| r.find_commit(pick).ok())
                .and_then(|c| c.summary().map(str::to_string));
            let name = self.entry_name(idx);
            let dirty_warning = if idx == 0 {
                self.overview
                    .as_ref()
                    .filter(|ov| ov.dirty)
                    .map(|_| "worktree has changes — checkout may be refused".to_string())
            } else {
                self.overview
                    .as_ref()
                    .and_then(|ov| ov.submodules.get(idx - 1))
                    .filter(|sm| sm.dirty == Some(true))
                    .map(|_| "worktree is dirty — update may be refused".to_string())
            };
            changes.push(SaveChange {
                idx,
                name,
                current,
                pick,
                title,
                dirty_warning,
            });
        }
        changes
    }

    /// Apply every pending pick, best-effort: main-repo checkout first (its
    /// index rewrite must not clobber gitlinks staged afterwards), then each
    /// submodule pointer. Failures are collected, never abort the run, and
    /// keep their pick for a retry.
    fn apply_saves(&mut self) {
        self.action_note_editing = false;
        if self.is_pulling() {
            return;
        }
        // Fresh plan: state may have drifted since the popup opened.
        let mut plan = Vec::new();
        for idx in 0..self.entries.len() {
            let Some(pick) = self.entries[idx].pick else {
                continue;
            };
            let current = self.current_oid(idx);
            if current == Some(pick) {
                continue;
            }
            let path = if idx == 0 {
                None
            } else {
                self.overview
                    .as_ref()
                    .and_then(|ov| ov.submodules.get(idx - 1))
                    .map(|sm| sm.path.clone())
            };
            plan.push((idx, self.entry_name(idx), path, pick, current));
        }

        if !plan.is_empty() && !self.require_action_note() {
            return;
        }
        let mut outcome = ApplyOutcome::default();
        let session = match self.ensure_session() {
            Ok(id) => id,
            Err(e) => {
                outcome
                    .failed
                    .push(("History".into(), format!("No commits applied: {e:#}")));
                self.save_popup = Some(SaveStage::Result(outcome));
                return;
            }
        };
        let root = self.overview.as_ref().unwrap().root.clone();
        let note_group = match crate::session::identifier() {
            Ok(id) => id,
            Err(e) => {
                self.status = format!("Save not started: {e:#}");
                return;
            }
        };
        let repo = self.repo.take();
        for (idx, name, path, pick, current) in plan {
            let result = crate::repository_history::run_grouped(
                &root,
                &session,
                "commit pick",
                path.as_deref().unwrap_or("."),
                &pick.to_string(),
                &crate::repository_history::Annotation {
                    note: &self.action_note,
                    group: Some(&note_group),
                },
                || {
                    let result = if idx == 0 {
                        match repo.as_ref() {
                            Some(r) => gitops::checkout_oid(r, pick),
                            None => Err(anyhow!("repository unavailable")),
                        }
                    } else {
                        match (repo.as_ref(), path.as_deref()) {
                            (Some(r), Some(p)) => gitops::set_submodule_pointer(r, p, pick),
                            _ => Err(anyhow!("submodule path unavailable")),
                        }
                    };
                    result.map(|()| format!("Applied commit {pick}"))
                },
            );
            match result {
                Ok(_) => {
                    outcome.applied.push(format!(
                        "{name}: {} → {}",
                        current.map(short_oid).unwrap_or_else(|| "—".into()),
                        short_oid(pick)
                    ));
                    if let Some(entry) = self.entries.get_mut(idx) {
                        entry.pick = None; // done; failed picks stay for retry
                    }
                }
                Err(e) => outcome.failed.push((name, format!("{e:#}"))),
            }
        }
        self.repo = repo;
        self.save_popup = Some(SaveStage::Result(outcome));
        self.refresh();
    }

    fn entry_name(&self, idx: usize) -> String {
        if idx == 0 {
            self.overview
                .as_ref()
                .map(|ov| {
                    ov.root
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| ov.root.display().to_string())
                })
                .unwrap_or_else(|| "main".to_string())
        } else {
            self.overview
                .as_ref()
                .and_then(|ov| ov.submodules.get(idx - 1))
                .map(|sm| sm.name.clone())
                .unwrap_or_else(|| format!("submodule {idx}"))
        }
    }

    // ---- navigation -----------------------------------------------------

    fn move_selection(&mut self, delta: i32) {
        match self.focus {
            Focus::Repos => {
                self.repos.move_by(delta);
                self.ensure_entry_loaded(self.repos.selected());
                self.ensure_repos_visible();
            }
            Focus::Commits => self.commit_scroll(|s| s.move_by(delta)),
        }
    }

    fn page_selection(&mut self, delta: i32) {
        match self.focus {
            Focus::Repos => {
                let h = self.pane_heights.repos.max(1);
                self.repos.page_by(h, delta);
                self.ensure_entry_loaded(self.repos.selected());
                self.ensure_repos_visible();
            }
            Focus::Commits => {
                let viewport = self.pane_heights.commits.max(1);
                let entry = &mut self.entries[self.repos.selected()];
                let max = entry.commit_text_height.saturating_sub(viewport);
                if delta > 0 && entry.commit_text_scroll < max {
                    entry.commit_text_scroll = entry
                        .commit_text_scroll
                        .saturating_add(viewport.saturating_sub(1).max(1))
                        .min(max);
                    return;
                }
                if delta < 0 && entry.commit_text_scroll > 0 {
                    entry.commit_text_scroll = entry
                        .commit_text_scroll
                        .saturating_sub(viewport.saturating_sub(1).max(1));
                    return;
                }
                let h = entry.commit_page.max(1);
                self.commit_scroll(|s| s.page_by(h, delta));
            }
        }
    }

    fn first_selection(&mut self) {
        match self.focus {
            Focus::Repos => {
                self.repos.first();
                self.ensure_entry_loaded(self.repos.selected());
                self.ensure_repos_visible();
            }
            Focus::Commits => self.commit_scroll(|s| s.first()),
        }
    }

    fn last_selection(&mut self) {
        match self.focus {
            Focus::Repos => {
                self.repos.last();
                self.ensure_entry_loaded(self.repos.selected());
                self.ensure_repos_visible();
            }
            Focus::Commits => self.commit_scroll(|s| s.last()),
        }
    }

    fn commit_scroll(&mut self, f: impl FnOnce(&mut ScrollState)) {
        let idx = self.repos.selected();
        // The entry must be loaded before moving its cursor: a freshly
        // selected entry has an empty (len 0) scroll until its history loads.
        self.ensure_entry_loaded(idx);
        if let Some(entry) = self.entries.get_mut(idx) {
            let selected = entry.scroll.selected();
            f(&mut entry.scroll);
            if selected != entry.scroll.selected() {
                entry.commit_text_scroll = 0;
                entry.commit_text_height = 0;
            }
            entry
                .scroll
                .ensure_visible(self.pane_heights.commits.max(1));
        }
        self.ensure_commits_ahead();
    }

    fn ensure_repos_visible(&mut self) {
        self.repos.ensure_visible(self.pane_heights.repos.max(1));
    }

    // ---- history loading ------------------------------------------------

    /// Discover and cache the repository of entry `idx` (no-op for the
    /// main repository and for uninitialized submodules).
    fn ensure_sub_repo(&mut self, idx: usize) {
        if idx == 0 {
            return;
        }
        let Some(cache) = self.sub_repos.get_mut(idx - 1) else {
            return;
        };
        if cache.is_some() {
            return;
        }
        let workdir = self
            .overview
            .as_ref()
            .and_then(|ov| ov.submodules.get(idx - 1))
            .and_then(|sm| sm.workdir.clone());
        // discover() resolves the submodule's `.git` file/link the same way
        // overview() decided it is initialized.
        *cache = workdir.and_then(|wd| gitx::discover(&wd).ok().flatten());
    }

    /// Load the history of entry `idx` (once); cheaper entries (submodules)
    /// are opened lazily and cached in `sub_repos`.
    fn ensure_entry_loaded(&mut self, idx: usize) {
        self.ensure_sub_repo(idx);
        let filter = self.filter.clone();
        let needle = filter.to_lowercase();
        let App {
            repo,
            sub_repos,
            entries,
            ..
        } = self;
        let Some(entry) = entries.get_mut(idx) else {
            return;
        };
        if entry.loaded {
            if entry.filtered_for != filter {
                self.apply_filter_to_entry(idx);
            }
            return;
        }
        entry.loaded = true;

        let entry_repo: Option<&Repository> = if idx == 0 {
            repo.as_ref()
        } else {
            sub_repos.get(idx - 1).and_then(|c| c.as_ref())
        };

        if let Some(r) = entry_repo {
            let tips = gitx::history_tips(r);
            if let Ok(mut log) = CommitLog::load_from(r, &tips) {
                let total = log.len();
                let limit = if needle.is_empty() {
                    INITIAL_WINDOW
                } else {
                    total
                };
                let _ = log.ensure_loaded(r, limit);
                entry.visible = (0..total)
                    .filter(|&i| {
                        needle.is_empty()
                            || log.get(i).is_some_and(|c| {
                                c.title.to_lowercase().contains(&needle)
                                    || c.oid.to_string().contains(&needle)
                                    || c.short_sha.to_lowercase().contains(&needle)
                            })
                    })
                    .collect();
                entry.scroll.set_len(entry.visible.len());
                entry.filtered_for = filter;
                entry.log = log;
            }
        }
    }

    /// Materialize more rows of the selected entry's history so the viewport
    /// (plus a margin) is always rendered.
    fn ensure_commits_ahead(&mut self) {
        let idx = self.repos.selected();
        self.ensure_entry_loaded(idx);
        let needed = match self.entries.get(idx) {
            Some(entry) => {
                entry
                    .visible
                    .get(entry.scroll.selected() + self.pane_heights.commits.max(1) + 1)
                    .or_else(|| entry.visible.last())
                    .copied()
                    .unwrap_or(0)
                    + 1
            }
            None => return,
        };

        let App {
            repo,
            sub_repos,
            entries,
            ..
        } = self;
        let Some(entry) = entries.get_mut(idx) else {
            return;
        };
        if needed <= entry.log.materialized() {
            return;
        }
        let entry_repo: Option<&Repository> = if idx == 0 {
            repo.as_ref()
        } else {
            sub_repos.get(idx - 1).and_then(|c| c.as_ref())
        };
        if let Some(r) = entry_repo {
            let _ = entry.log.ensure_loaded(r, needed + AHEAD_MARGIN);
        }
    }

    // ---- picks ----------------------------------------------------------

    /// Pick (or un-pick) the commit under the commits cursor: the commit the
    /// main repository should record for the selected entry. Picking the
    /// currently recorded commit is a no-op (clears the pick).
    fn toggle_pick(&mut self) {
        if self.is_pulling() {
            self.status = "Wait for the remote operation before picking commits".into();
            return;
        }
        let idx = self.repos.selected();
        let current = self.current_oid(idx);
        let Some(entry) = self.entries.get_mut(idx) else {
            return;
        };
        let Some(&row) = entry.visible.get(entry.scroll.selected()) else {
            return;
        };
        let Some(oid) = entry.log.get(row).map(|c| c.oid) else {
            return;
        };
        entry.pick = if entry.pick == Some(oid) || Some(oid) == current {
            None // un-pick, or picking the current commit (a no-op)
        } else {
            Some(oid)
        };
    }

    // ---- refresh ----------------------------------------------------------

    /// Re-read the repository from disk. Picks survive (matched by entry
    /// key); histories and scroll positions are rebuilt.
    pub fn refresh(&mut self) {
        let Some(repo) = &self.repo else {
            return;
        };
        let Ok(ov) = gitx::overview(repo) else {
            return; // keep showing the previous snapshot
        };

        let picks: Vec<(EntryKey, git2::Oid)> = self
            .entries
            .iter()
            .filter_map(|e| e.pick.map(|p| (e.key.clone(), p)))
            .collect();

        let branches: Vec<_> = self
            .entries
            .iter()
            .map(|e| (e.key.clone(), e.branch.clone()))
            .collect();
        let n = ov.submodules.len();
        let mut entries = vec![EntryState::new(EntryKey::Main)];
        for sm in &ov.submodules {
            entries.push(EntryState::new(EntryKey::Sub(sm.path.clone())));
        }
        for (key, oid) in picks {
            if let Some(entry) = entries.iter_mut().find(|e| e.key == key) {
                entry.pick = Some(oid);
            }
        }
        for (key, branch) in branches {
            if let Some(entry) = entries.iter_mut().find(|e| e.key == key) {
                entry.branch = branch;
            }
        }

        self.overview = Some(ov);
        self.sub_repos = (0..n).map(|_| None).collect();
        self.entries = entries;
        for idx in 0..self.entries.len() {
            if self.entries[idx].pick.is_some() && self.entries[idx].pick == self.current_oid(idx) {
                self.entries[idx].pick = None;
            }
        }
        self.repos.set_len(1 + n);
        if n == 0 {
            self.focus = Focus::Commits;
        }
        self.ensure_entry_loaded(self.repos.selected());
        self.refresh_updates();
    }
}

fn short_oid(oid: git2::Oid) -> String {
    oid.to_string()[..7].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, BASE_TIME, SubSpec, TempDir};
    use crossterm::event::{MouseButton, MouseEventKind};
    use std::fs;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn wheel(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 40,
            row: 10,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn normal_app() -> (App, TempDir) {
        let temp = TempDir::new("app").unwrap();
        let sup = fixture::make_super(
            &temp.path().join("super"),
            &[
                SubSpec {
                    name: "subA",
                    msgs: &["a1", "a2"],
                    recorded: None,
                    drop_workdir: false,
                },
                SubSpec {
                    name: "subC",
                    msgs: &["c1"],
                    recorded: None,
                    drop_workdir: true,
                },
            ],
        )
        .unwrap();
        let ov = crate::gitx::overview(&sup.repo).unwrap();
        (App::normal(ov, sup.repo), temp)
    }

    #[test]
    fn not_repo_quit_paths() {
        let mut app = App::not_repo(Path::new("/x"));
        assert!(!app.should_quit);
        crate::fixture::confirm_action(&mut app);
        assert!(app.should_quit);

        let mut app = App::not_repo(Path::new("/x"));
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);

        let mut app = App::not_repo(Path::new("/x"));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    #[test]
    fn error_mode_also_exits_on_ok() {
        let mut app = App::error(Path::new("/x"), "boom");
        crate::fixture::confirm_action(&mut app);
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_q_quits_even_with_popup_open() {
        let (mut app, _t) = normal_app();
        app.popup = Some(Popup::Help);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn global_history_restores_another_module_without_writing_until_confirmation() {
        let (mut app, temp) = normal_app();
        let root = temp.path().join("super").canonicalize().unwrap();
        let mut paths = Vec::new();
        for module in ["subA", "subB"] {
            let path = root.join(module).join("config/gains.yaml");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "gain: 1\n").unwrap();
            let mut form = config::Form::open_in(path.clone(), &root).unwrap();
            form.activate();
            form.paste("2");
            form.commit_input().unwrap();
            form.save_with_note("Test config change").unwrap();
            paths.push(path);
        }
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.view, View::Repositories);
        assert_eq!(app.global_history.as_ref().unwrap().entries.len(), 2);
        app.handle_key(key(KeyCode::Esc));
        // A clean form in one module may restore a different module's save.
        app.form = Some(config::Form::open_in(paths[0].clone(), &root).unwrap());
        app.handle_key(key(KeyCode::Char('h')));
        let browser = app.global_history.as_mut().unwrap();
        browser.key(KeyCode::Char('/'), KeyModifiers::NONE, 10);
        browser.paste("subB");
        browser.key(KeyCode::Enter, KeyModifiers::NONE, 10);
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Char('r')));
        assert!(app.global_history.is_none());
        assert_eq!(app.view, View::Configurations);
        assert_eq!(app.form.as_ref().unwrap().path, paths[1]);
        assert!(app.form.as_ref().unwrap().dirty());
        assert_eq!(fs::read_to_string(&paths[1]).unwrap(), "gain: 2\n");
        // Pending restoration cannot be silently abandoned by opening history.
        app.handle_key(key(KeyCode::Char('h')));
        assert!(app.global_history.is_none());
        assert!(
            app.form
                .as_ref()
                .unwrap()
                .notice
                .contains("Save or discard")
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(
            app.form.as_ref().unwrap().overlay,
            Some(config::Overlay::Save { .. })
        ));
        assert_eq!(fs::read_to_string(&paths[1]).unwrap(), "gain: 2\n");
        crate::fixture::confirm_action(&mut app);
        assert_eq!(fs::read_to_string(&paths[1]).unwrap(), "gain: 1\n");
        assert_eq!(fs::read_to_string(&paths[0]).unwrap(), "gain: 2\n");
        assert_eq!(
            crate::history::Store::list_global(&root, &[])
                .unwrap()
                .0
                .len(),
            3
        );
    }

    #[test]
    fn config_shortcuts_filter_modules_and_save_the_selected_file() {
        let (mut app, temp) = normal_app();
        let file = temp.path().join("super/subA/ros_pkg/config/tuning.yaml");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "gain: 1\n").unwrap();
        // The main repo has a config, but the configurations view is submodule-only.
        fs::create_dir_all(temp.path().join("super/config")).unwrap();
        fs::write(temp.path().join("super/config/main.yaml"), "main: true").unwrap();
        app.select_repo(1);
        app.entries[1].pick = app.entries[1].log.get(1).map(|c| c.oid);
        let pick = app.entries[1].pick;
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.should_quit);
        assert_eq!(app.view, View::Repositories);
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.view, View::Configurations);
        assert_eq!(app.config_modules.len(), 1);
        assert_eq!(app.config_modules[0].entry_idx, 1);
        app.handle_key(key(KeyCode::Tab));
        crate::fixture::confirm_action(&mut app);
        app.handle_key(key(KeyCode::Char('2')));
        assert!(
            !app.form.as_ref().unwrap().dirty(),
            "typing outside a value input cannot edit YAML"
        );
        crate::fixture::confirm_action(&mut app);
        app.handle_key(key(KeyCode::Char('2')));
        crate::fixture::confirm_action(&mut app);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.form.is_some(), "unsaved edits prevent accidental close");
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(
            app.view,
            View::Configurations,
            "unsaved edits prevent view switching"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), "gain: 1\n");
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "gain: 1\n",
            "save needs confirmation"
        );
        crate::fixture::confirm_action(&mut app);
        assert_eq!(fs::read_to_string(&file).unwrap(), "gain: 2\n");
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Repositories);
        assert_eq!(
            app.entries[1].pick, pick,
            "view changes retain commit picks"
        );
    }

    #[test]
    fn shortcuts_expire_and_plain_characters_do_not_switch_views() {
        let (mut app, _temp) = normal_app();
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.view, View::Repositories);
        app.shortcut = Some(('c', Instant::now() - Duration::from_secs(3)));
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.view, View::Repositories);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(app.view, View::Configurations);
        assert!(app.config_modules.is_empty());
        crate::fixture::confirm_action(&mut app);
        crate::fixture::confirm_action(&mut app);
        assert!(app.form.is_none());
    }

    #[test]
    fn retained_branch_badge_survives_a_detached_commit_pick() {
        let (mut app, _temp) = normal_app();
        let repo = app.repo.as_ref().unwrap();
        let old = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent_id(0)
            .unwrap();
        gitops::checkout_oid(repo, old).unwrap();
        app.refresh();
        assert_eq!(app.entries[0].branch.as_deref(), Some("main"));
        assert_eq!(app.entries[0].updates.as_ref().unwrap().newer, 1);
    }

    #[test]
    fn pull_shortcut_runs_all_entries_and_collects_partial_failures() {
        let (mut app, _temp) = normal_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.popup, Some(Popup::PullConfirm));
        assert!(
            app.network_job.is_none(),
            "shortcut only requests confirmation"
        );
        assert!(app.action_note_editing);
        app.handle_paste("Refresh before testing");
        crate::fixture::confirm_action(&mut app);
        assert!(
            app.network_job.is_none(),
            "finishing a note does not start a pull"
        );
        crate::fixture::confirm_action(&mut app);
        assert!(app.network_job.is_some());
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.network_job.is_some() {
            assert!(Instant::now() < deadline, "pull-all worker must complete");
            app.poll_network();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.network_results.len(), 3);
        assert!(app.network_results[0].1.is_ok());
        assert!(app.network_results[1].1.is_ok());
        assert!(app.network_results[2].1.is_err());
        assert_eq!(app.popup, Some(Popup::Network));
        let root = &app.overview.as_ref().unwrap().root;
        let events = crate::repository_history::events(root).unwrap().0;
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .all(|e| e.intent.session == app.session.as_ref().unwrap().id
                    && e.intent.note == "Refresh before testing")
        );
    }

    #[test]
    fn selected_pull_updates_only_its_frozen_target_then_pull_all_still_updates_the_stack()
    -> Result<()> {
        let temp = TempDir::new("app-selected-pull")?;
        let root = temp.path().join("super");
        let stack = fixture::make_super(
            &root,
            &[
                SubSpec {
                    name: "subA",
                    msgs: &["a1", "a2"],
                    recorded: None,
                    drop_workdir: false,
                },
                SubSpec {
                    name: "subB",
                    msgs: &["b1", "b2"],
                    recorded: None,
                    drop_workdir: false,
                },
            ],
        )?;
        let mut tips = Vec::new();
        for (idx, path) in [&root, &root.join("subA"), &root.join("subB")]
            .into_iter()
            .enumerate()
        {
            let local = Repository::open(path)?;
            let old = local.head()?.target().unwrap();
            let remote_path = temp.path().join(format!("remote-{idx}"));
            let remote = Repository::clone(path.to_str().unwrap(), &remote_path)?;
            local.remote("origin", remote_path.to_str().unwrap())?;
            local.reference("refs/remotes/origin/main", old, false, "fixture upstream")?;
            local
                .find_branch("main", git2::BranchType::Local)?
                .set_upstream(Some("origin/main"))?;
            let new = fixture::commit_file(
                &remote,
                "latest.txt",
                "new\n",
                "Remote trial",
                BASE_TIME + 10_000 + idx as i64,
            )?;
            tips.push((old, new));
        }
        let mut app = App::normal(gitx::overview(&stack.repo)?, stack.repo);
        app.entries[2].pick = Some(stack.sub_oids["subB"][0]);
        app.select_repo(1);
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.pull_target.as_ref().unwrap().name, "subA");
        assert!(app.action_note_editing && app.network_job.is_none());
        app.select_repo(2); // Selection drift cannot retarget an existing confirmation.
        app.handle_paste("Update steering module");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.session.is_none());
        app.handle_key(key(KeyCode::Enter));
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.network_job.is_some() {
            assert!(Instant::now() < deadline);
            app.poll_network();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.network_results.len(), 1);
        assert_eq!(app.network_results[0].0, "subA");
        assert!(
            app.network_results[0].1.is_ok(),
            "{:?}",
            app.network_results
        );
        assert_eq!(app.entries[2].pick, Some(stack.sub_oids["subB"][0]));
        for (idx, path) in [&root, &root.join("subA"), &root.join("subB")]
            .into_iter()
            .enumerate()
        {
            let repo = Repository::open(path)?;
            let expected = if idx == 1 { tips[idx].1 } else { tips[idx].0 };
            assert_eq!(repo.head()?.target(), Some(expected));
            assert_eq!(
                repo.find_reference("refs/remotes/origin/main")?.target(),
                Some(expected),
                "unselected refs are not fetched"
            );
        }
        let events = crate::repository_history::events(&root)?.0;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].intent.subject, "subA");
        assert_eq!(events[0].intent.note, "Update steering module");
        let overview = app.overview.as_ref().unwrap();
        assert_eq!(
            overview.submodules[0].index_oid,
            Some(tips[1].0),
            "pull never stages a new submodule pointer"
        );
        app.handle_key(key(KeyCode::Esc));
        app.entries[2].pick = None;
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('p')));
        assert!(
            app.pull_target.is_none(),
            "all shortcut resets the previous selected scope"
        );
        app.handle_paste("Update entire stack");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.network_job.is_some() {
            assert!(Instant::now() < deadline);
            app.poll_network();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.network_results.len(), 3);
        assert!(
            app.network_results.iter().all(|(_, result)| result.is_ok()),
            "{:?}",
            app.network_results
        );
        for (idx, path) in [&root, &root.join("subA"), &root.join("subB")]
            .into_iter()
            .enumerate()
        {
            assert_eq!(Repository::open(path)?.head()?.target(), Some(tips[idx].1));
        }
        assert_eq!(crate::history::Browser::open(&root)?.notes().len(), 2);
        Ok(())
    }

    #[test]
    fn selected_pull_requires_a_note_and_blocks_only_target_picks_or_uninitialized_targets() {
        let (mut app, _temp) = normal_app();
        app.select_repo(1);
        let pick = app.entries[1].log.get(1).unwrap().oid;
        app.entries[1].pick = Some(pick);
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.popup.is_none() && app.network_job.is_none());
        assert!(app.status.contains("selected repository"));
        assert_eq!(app.entries[1].pick, Some(pick));
        app.entries[1].pick = None;
        app.entries[0].pick = app
            .repo
            .as_ref()
            .unwrap()
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent_id(0)
            .ok();
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.popup, Some(Popup::PullConfirm));
        assert!(app.action_note_editing);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.action_note_editing && app.session.is_none() && app.network_job.is_none());
        app.handle_paste(" ");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.action_note_editing && app.session.is_none());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.popup.is_none());
        app.request_pull();
        assert!(
            app.popup.is_none(),
            "all pulls still block every pending pick"
        );
        app.entries[0].pick = None;
        app.select_repo(2);
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.popup.is_none() && app.session.is_none() && app.network_job.is_none());
        assert!(app.status.contains("not initialized"));
        app.select_repo(0);
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(
            app.pull_target.as_ref().unwrap().path.as_ref(),
            Some(&app.overview.as_ref().unwrap().root)
        );
        crate::fixture::confirm_action(&mut app);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.network_job.is_some() {
            assert!(Instant::now() < deadline);
            app.poll_network();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            app.network_results.len(),
            1,
            "selected main repository is also supported"
        );
    }

    #[test]
    fn config_module_and_branch_picker_pull_use_their_selected_repository() -> Result<()> {
        let (mut app, temp) = normal_app();
        let file = temp.path().join("super/subA/config/params.yaml");
        fs::create_dir_all(file.parent().unwrap())?;
        fs::write(&file, "gain: 1\n")?;
        app.switch_view(View::Configurations);
        assert_eq!(
            app.repos.selected(),
            0,
            "repository cursor is deliberately different"
        );
        app.handle_key(key(KeyCode::Char('p')));
        assert_eq!(app.pull_target.as_ref().unwrap().name, "subA");
        app.handle_key(key(KeyCode::Esc));
        app.switch_view(View::Repositories);
        app.select_repo(1);
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.branch_popup.is_none());
        assert_eq!(app.pull_target.as_ref().unwrap().name, "subA");
        assert!(app.action_note_editing && app.session.is_none());
        Ok(())
    }

    #[test]
    fn three_repository_save_has_one_note_but_separate_saves_keep_identical_notes() -> Result<()> {
        let temp = TempDir::new("app-batch-notes")?;
        let root = temp.path().join("super");
        let sup = fixture::make_super(
            &root,
            &[
                SubSpec {
                    name: "subA",
                    msgs: &["a1", "a2"],
                    recorded: None,
                    drop_workdir: false,
                },
                SubSpec {
                    name: "subB",
                    msgs: &["b1", "b2"],
                    recorded: None,
                    drop_workdir: false,
                },
                SubSpec {
                    name: "subC",
                    msgs: &["c1", "c2"],
                    recorded: None,
                    drop_workdir: false,
                },
            ],
        )?;
        let mut app = App::normal(gitx::overview(&sup.repo)?, sup.repo);
        for (idx, name) in [(1, "subA"), (2, "subB"), (3, "subC")] {
            app.select_repo(idx);
            app.entries[idx].pick = Some(sup.sub_oids[name][0]);
        }
        app.handle_key(key(KeyCode::Char('s')));
        assert!(app.action_note_editing);
        app.handle_paste("Same test note");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.session.is_none(), "finishing notes never writes");
        app.handle_key(key(KeyCode::Enter));
        let browser = crate::history::Browser::open(&root)?;
        assert_eq!(browser.repository_events.len(), 3);
        assert_eq!(browser.notes().len(), 1);
        for name in ["subA", "subB", "subC"] {
            assert!(browser.notes()[0].0.contains(name));
        }
        let first_group = browser.repository_events[0].intent.note_group.clone();
        assert!(first_group.is_some());
        assert!(
            browser
                .repository_events
                .iter()
                .all(|e| e.intent.note_group == first_group)
        );
        app.handle_key(key(KeyCode::Enter)); // close result
        app.entries[1].pick = Some(sup.sub_oids["subA"][1]);
        app.handle_key(key(KeyCode::Char('s')));
        app.handle_paste("Same test note");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        app.finish_session()?;
        drop(app);
        let mut browser = crate::history::Browser::open(&root)?;
        assert_eq!(
            browser.notes().len(),
            2,
            "separate confirmations must never collapse by note text"
        );
        assert!(
            browser
                .notes()
                .iter()
                .all(|(_, note)| note == "Same test note")
        );
        browser.key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert_eq!(
            browser.notes().len(),
            2,
            "drilling into an action preserves grouped session notes"
        );
        Ok(())
    }

    #[test]
    fn branch_picker_fetches_remote_branches_without_checkout_then_prompts_for_tracking_switch()
    -> Result<()> {
        let temp = TempDir::new("app-fetch-branches")?;
        let (remote, _) = fixture::make_history(&temp.path().join("remote"), &["initial"])?;
        let root = temp.path().join("local");
        let local = Repository::clone(remote.workdir().unwrap().to_str().unwrap(), &root)?;
        let initial = local.head()?.target();
        let new_commit = fixture::commit_file(
            &remote,
            "test.txt",
            "trial\n",
            "Trial commit",
            BASE_TIME + 100,
        )?;
        remote.branch("test/track", &remote.find_commit(new_commit)?, false)?;
        let overview = gitx::overview(&local)?;
        let mut app = App::normal(overview, local);
        app.handle_key(key(KeyCode::Char('b')));
        assert!(
            !app.branch_popup
                .as_ref()
                .unwrap()
                .branches
                .iter()
                .any(|b| b.name == "origin/test/track")
        );
        app.handle_key(key(KeyCode::Char('f')));
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.network_job.is_some() {
            assert!(Instant::now() < deadline);
            app.poll_network();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(app.repo.as_ref().unwrap().head()?.target(), initial);
        assert!(
            app.session.is_none(),
            "fetch-only has no checkout or documentation requirement"
        );
        let popup = app.branch_popup.as_mut().unwrap();
        let row = popup
            .branches
            .iter()
            .position(|b| b.name == "origin/test/track" && b.is_remote)
            .unwrap();
        popup.scroll.select(row);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.action_note_editing && app.session.is_none());
        app.handle_paste("Try track branch");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.branch_popup.is_none());
        let repo = app.repo.as_ref().unwrap();
        assert_eq!(repo.head()?.shorthand(), Some("test/track"));
        assert_eq!(repo.head()?.target(), Some(new_commit));
        assert_eq!(
            repo.find_branch("test/track", git2::BranchType::Local)?
                .upstream()?
                .name()?,
            Some("origin/test/track")
        );
        assert_eq!(
            crate::repository_history::events(&root)?.0[0].intent.note,
            "Try track branch"
        );
        Ok(())
    }

    #[test]
    fn pull_confirmation_is_modal_and_cancellation_never_starts_a_worker() {
        let (mut app, _temp) = normal_app();
        assert!(app.network_job.is_none(), "startup has no remote operation");
        for cancel in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('Q')] {
            app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
            app.handle_key(key(KeyCode::Char('p')));
            assert_eq!(app.popup, Some(Popup::PullConfirm));
            app.handle_key(key(KeyCode::Char('p')));
            app.handle_key(key(KeyCode::Char('s')));
            assert!(app.network_job.is_none());
            assert!(app.save_popup.is_none());
            if cancel != KeyCode::Esc {
                app.handle_key(key(KeyCode::Enter));
            }
            app.handle_key(key(cancel));
            assert_eq!(app.popup, None);
            assert!(app.network_job.is_none());
            assert!(!app.should_quit);
        }
    }

    #[test]
    fn missing_notes_block_repository_actions_before_creating_a_session() {
        let (mut app, _temp) = branchy_app();
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.action_note_editing && app.session.is_none());
        assert_eq!(
            app.repo.as_ref().unwrap().head().unwrap().shorthand(),
            Some("main")
        );
        app.handle_paste(" ");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.action_note_editing && app.session.is_none());
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.handle_paste("x");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.repo.as_ref().unwrap().head().unwrap().shorthand(),
            Some("feature")
        );
        assert!(!app.status.starts_with("A note is required:"));
        assert_eq!(
            crate::repository_history::events(&app.overview.as_ref().unwrap().root)
                .unwrap()
                .0[0]
                .intent
                .note,
            "x"
        );

        let (mut app, _temp) = normal_app();
        let pick = pick_sub_a_older(&mut app);
        let before = app.workdir_oid(1);
        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.action_note_editing && app.session.is_none());
        assert_eq!(app.entries[1].pick, Some(pick));
        assert_eq!(app.workdir_oid(1), before);
        app.handle_paste("x");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.workdir_oid(1), Some(pick));

        let (mut app, _temp) = normal_app();
        app.request_pull();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.action_note_editing && app.session.is_none());
        assert!(app.network_job.is_none());
        assert_eq!(app.popup, Some(Popup::PullConfirm));
    }

    #[test]
    fn repository_note_input_does_not_apply_and_cancelled_notes_do_not_leak() {
        let (mut app, _temp) = branchy_app();
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_paste("Trial branch\n\u{1b}");
        crate::fixture::confirm_action(&mut app);
        assert_eq!(app.action_note, "Trial branch");
        assert!(!app.action_note_editing);
        assert_eq!(
            app.repo.as_ref().unwrap().head().unwrap().shorthand(),
            Some("main")
        );
        assert!(
            app.session.is_none(),
            "finishing a note never saves a change"
        );
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('b')));
        assert!(app.action_note.is_empty());
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_paste(&"λ".repeat(600));
        assert_eq!(app.action_note.chars().count(), 500);
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.handle_paste("Use trial branch");
        crate::fixture::confirm_action(&mut app);
        crate::fixture::confirm_action(&mut app);
        let events = crate::repository_history::events(&app.overview.as_ref().unwrap().root)
            .unwrap()
            .0;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].intent.note, "Use trial branch");
    }

    #[test]
    fn worker_disconnect_clears_busy_state_and_reports_uncertain_outcomes() {
        let (mut app, _temp) = normal_app();
        let (tx, rx) = std::sync::mpsc::channel();
        drop(tx);
        app.network_job = Some(rx);
        app.network_cancel = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )));
        app.network_operation = Some(network::Operation::Pull);
        app.poll_network();
        assert!(
            app.network_job.is_none()
                && app.network_cancel.is_none()
                && app.network_operation.is_none()
        );
        assert_eq!(app.popup, Some(Popup::Network));
        assert!(
            app.network_results
                .last()
                .unwrap()
                .1
                .as_ref()
                .unwrap_err()
                .contains("partial")
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn pull_confirmation_preserves_pending_picks_and_open_forms() {
        let (mut app, temp) = normal_app();
        app.entries[0].pick = app.overview.as_ref().unwrap().head_oid;
        app.request_pull();
        assert_eq!(app.popup, None);
        assert!(app.network_job.is_none());
        assert!(app.status.contains("pending commit picks"));
        app.entries[0].pick = None;
        let file = temp.path().join("params.yaml");
        fs::write(&file, "gain: 1\n").unwrap();
        app.form = Some(config::Form::open(file).unwrap());
        app.request_pull();
        assert_eq!(app.popup, None);
        assert!(app.network_job.is_none());
        assert!(
            app.form
                .as_ref()
                .unwrap()
                .notice
                .contains("Close the config form")
        );
    }

    #[test]
    fn help_popup_open_and_close_then_quit() {
        let (mut app, _t) = normal_app();

        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.popup, Some(Popup::Help));

        // While the popup is open, q closes it instead of quitting.
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.popup, None);
        assert!(!app.should_quit);

        // Esc closes too.
        app.handle_key(key(KeyCode::Char('?')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.popup, None);

        // And with no popup, q quits.
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn tab_cycles_focus_and_arrows_set_it() {
        let (mut app, _t) = normal_app();
        assert_eq!(app.focus, Focus::Repos);

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Commits);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Repos);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Commits);

        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.focus, Focus::Repos);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.focus, Focus::Commits);
    }

    #[test]
    fn selection_moves_pages_and_clamps() {
        let (mut app, _t) = normal_app();
        assert_eq!(app.repos.len(), 3); // main + subA + subC

        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.repos.selected(), 2);
        app.handle_key(key(KeyCode::Down)); // clamped
        assert_eq!(app.repos.selected(), 2);
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.repos.selected(), 1);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.repos.selected(), 2);

        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.repos.selected(), 0);
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.repos.selected(), 2);

        app.pane_heights.repos = 2;
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.repos.selected(), 0);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.repos.selected(), 2);
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.repos.selected(), 0);
    }

    #[test]
    fn ensure_visible_tracks_selection() {
        let mut s = ScrollState::new(10);
        s.select(5);
        s.ensure_visible(3);
        assert_eq!(s.offset(), 3); // 5 - 3 + 1
        s.select(1);
        s.ensure_visible(3);
        assert_eq!(s.offset(), 1);
        s.select(0);
        s.ensure_visible(3);
        assert_eq!(s.offset(), 0);
    }

    #[test]
    fn visible_window_is_clamped() {
        let mut s = ScrollState::new(5);
        assert_eq!(s.visible(10), (0, 5)); // viewport larger than list
        s.select(4);
        s.ensure_visible(2);
        assert_eq!(s.visible(2), (3, 2));
        assert_eq!(s.visible(0), (0, 0));
        let empty = ScrollState::new(0);
        assert_eq!(empty.visible(5), (0, 0));
        assert!(empty.is_empty());
    }

    #[test]
    fn set_len_keeps_selection_in_range() {
        let mut s = ScrollState::new(5);
        s.select(4);
        s.set_len(2);
        assert_eq!(s.selected(), 1);
        s.set_len(0);
        assert_eq!(s.selected(), 0);
    }

    #[test]
    fn mouse_rows_do_not_select_or_focus() {
        let (mut app, _temp) = normal_app();
        app.hit_zones = vec![(Rect::new(0, 0, 20, 10), Hit::RepoRow(1))];
        app.handle_mouse(click(5, 2));
        assert_eq!(app.repos.selected(), 0);
        assert_eq!(app.focus, Focus::Repos);
    }

    #[test]
    fn mouse_does_not_dismiss_help() {
        let (mut app, _temp) = normal_app();
        app.popup = Some(Popup::Help);
        app.handle_mouse(click(5, 2));
        assert_eq!(app.popup, Some(Popup::Help));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.popup.is_none());
    }

    #[test]
    fn mouse_does_not_quit_outside_a_repo() {
        let mut app = App::not_repo(Path::new("/x"));
        app.hit_zones = vec![(Rect::new(30, 13, 20, 1), Hit::OkButton)];
        app.handle_mouse(click(40, 13));
        assert!(!app.should_quit);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.should_quit);
    }

    #[test]
    fn selected_entry_follows_cursor() {
        let (mut app, _t) = normal_app();
        assert!(matches!(app.selected_entry(), Some(Entry::Main(_))));
        app.repos.select(1);
        assert!(matches!(app.selected_entry(), Some(Entry::Sub(_))));
        app.repos.select(99); // clamped to last
        assert!(matches!(app.selected_entry(), Some(Entry::Sub(_))));
    }

    // ---- step 5: browsing, picks, refresh --------------------------------

    #[test]
    fn histories_load_lazily_per_entry() {
        let (mut app, _t) = normal_app();

        assert!(app.entries[0].loaded, "main loads at startup");
        assert_eq!(app.entries[0].log.len(), 2); // root + "Add submodules"
        assert!(!app.entries[1].loaded, "subA not loaded until selected");

        app.handle_key(key(KeyCode::Down)); // → subA
        assert!(app.entries[1].loaded);
        assert_eq!(app.entries[1].log.len(), 2); // a1, a2

        app.handle_key(key(KeyCode::Down)); // → subC (not initialized)
        assert!(app.entries[2].loaded);
        assert_eq!(app.entries[2].log.len(), 0);
    }

    #[test]
    fn enter_toggles_pick_and_picking_current_is_a_noop() {
        let (mut app, _t) = normal_app();
        app.handle_key(key(KeyCode::Tab)); // focus commits

        // Row 0 is the recorded commit (main HEAD): picking it is a no-op.
        crate::fixture::confirm_action(&mut app);
        assert_eq!(app.entries[0].pick, None);

        // Move to the older commit and pick it.
        app.handle_key(key(KeyCode::Down));
        crate::fixture::confirm_action(&mut app);
        let picked = app.entries[0].pick.expect("pick recorded");
        assert_eq!(app.entries[0].log.get(1).map(|c| c.oid), Some(picked));
        assert_ne!(Some(picked), app.current_oid(0));

        // Enter again un-picks.
        crate::fixture::confirm_action(&mut app);
        assert_eq!(app.entries[0].pick, None);
    }

    #[test]
    fn lazy_materialization_follows_scrolling() {
        let temp = TempDir::new("app-lazy").unwrap();
        let msgs: Vec<String> = (0..200).map(|i| format!("c{i}")).collect();
        let refs: Vec<&str> = msgs.iter().map(String::as_str).collect();
        let (repo, _) = fixture::make_history(&temp.path().join("r"), &refs).unwrap();
        let ov = crate::gitx::overview(&repo).unwrap();
        let mut app = App::normal(ov, repo);

        assert_eq!(app.entries[0].log.len(), 200);
        assert_eq!(app.entries[0].log.materialized(), INITIAL_WINDOW);
        assert!(app.entries[0].log.get(INITIAL_WINDOW).is_none());

        app.handle_key(key(KeyCode::End));
        assert_eq!(app.entries[0].scroll.selected(), 199);
        assert!(
            app.entries[0].log.get(199).is_some(),
            "scrolling to the end materializes the last row"
        );
        assert_eq!(app.entries[0].log.materialized(), 200);

        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.entries[0].scroll.selected(), 0);
        assert!(app.entries[0].log.get(0).is_some());
    }

    #[test]
    fn filter_searches_full_history_and_picks_matching_commit() {
        let temp = TempDir::new("app-filter").unwrap();
        let msgs: Vec<String> = (0..100).map(|i| format!("commit {i}")).collect();
        let refs: Vec<&str> = msgs.iter().map(String::as_str).collect();
        let (repo, _) = fixture::make_history(&temp.path().join("r"), &refs).unwrap();
        let ov = crate::gitx::overview(&repo).unwrap();
        let mut app = App::normal(ov, repo);
        assert_eq!(app.entries[0].log.materialized(), INITIAL_WINDOW);

        app.handle_key(key(KeyCode::Char('/')));
        for c in "COMMIT 3".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        crate::fixture::confirm_action(&mut app);
        assert!(!app.filter_editing);
        assert_eq!(app.entries[0].visible.len(), 11);
        app.handle_key(key(KeyCode::End));
        let original = *app.entries[0].visible.last().unwrap();
        assert_eq!(app.entries[0].log.get(original).unwrap().title, "commit 3");
        crate::fixture::confirm_action(&mut app);
        assert_eq!(
            app.entries[0].pick,
            app.entries[0].log.get(original).map(|c| c.oid)
        );

        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.should_quit);
        assert!(app.filter.is_empty());
        assert_eq!(app.entries[0].visible.len(), 100);
        assert!(app.entries[0].pick.is_some());
    }

    #[test]
    fn filter_follows_repo_switches_and_refresh() {
        let (mut app, _temp) = normal_app();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('a')));
        app.handle_key(key(KeyCode::Char('1')));
        crate::fixture::confirm_action(&mut app);
        assert_eq!(app.entries[0].visible.len(), 0);

        app.select_repo(1);
        assert_eq!(app.entries[1].visible.len(), 1);
        assert_eq!(
            app.entries[1]
                .log
                .get(app.entries[1].visible[0])
                .unwrap()
                .title,
            "a1"
        );
        app.refresh();
        assert_eq!(app.filter, "a1");
        assert_eq!(app.entries[1].visible.len(), 1);
    }

    #[test]
    fn focus_stays_on_commits_without_submodules() {
        let temp = TempDir::new("app-nosub").unwrap();
        let (repo, _) = fixture::make_history(&temp.path().join("r"), &["c1"]).unwrap();
        let ov = crate::gitx::overview(&repo).unwrap();
        let mut app = App::normal(ov, repo);

        assert_eq!(app.focus, Focus::Commits);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Commits);
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.focus, Focus::Commits);
    }

    #[test]
    fn mouse_wheel_does_not_scroll() {
        let (mut app, _temp) = normal_app();
        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        app.handle_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.repos.selected(), 0);
        app.focus = Focus::Commits;
        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.entries[0].scroll.selected(), 0);
    }

    #[test]
    fn mouse_commit_rows_do_not_select() {
        let (mut app, _temp) = normal_app();
        app.hit_zones = vec![(Rect::new(30, 5, 40, 1), Hit::CommitRow(1))];
        app.handle_mouse(click(45, 5));
        assert_eq!(app.focus, Focus::Repos);
        assert_eq!(app.entries[0].scroll.selected(), 0);
    }

    #[test]
    fn refresh_preserves_picks_and_reloads_state() {
        let (mut app, _t) = normal_app();
        app.handle_key(key(KeyCode::Tab)); // commits
        app.handle_key(key(KeyCode::Down)); // row 1
        crate::fixture::confirm_action(&mut app); // pick
        let pick = app.entries[0].pick.expect("pick set");

        app.handle_key(key(KeyCode::Char('r')));

        assert_eq!(app.entries[0].pick, Some(pick), "pick survives refresh");
        assert!(app.entries[0].loaded, "selected entry reloads");
        assert_eq!(app.entries[0].log.len(), 2);
        assert!(!app.entries[1].loaded, "other entries go lazy again");
        assert_eq!(
            app.entries[0].log.find(pick).map(|c| c.title.as_str()),
            Some("root")
        );
    }

    #[test]
    fn refresh_clears_pick_already_at_current_commit() {
        let (mut app, _t) = normal_app();
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Down));
        crate::fixture::confirm_action(&mut app);
        let pick = app.entries[0].pick.expect("pick set");

        gitops::checkout_oid(app.repo.as_ref().unwrap(), pick).unwrap();
        app.refresh();

        assert_eq!(app.current_oid(0), Some(pick));
        assert!(app.entries[0].pick.is_none());
    }

    #[test]
    fn fixture_base_time_is_deterministic() {
        // Guards the timestamps asserted on in the UI tests.
        let info = crate::gitx::CommitInfo {
            oid: git2::Oid::zero(),
            short_sha: "0000000".to_string(),
            title: "t".to_string(),
            author_name: "a".to_string(),
            time: git2::Time::new(BASE_TIME + 3600, 60),
        };
        assert_eq!(info.time_str(), "2023-11-15 00:13");
    }

    // ---- step 6: branch switcher -----------------------------------------

    /// Super + one submodule, where the main repo also has a `feature`
    /// branch and the submodule has an `old` branch at its first commit.
    fn branchy_app() -> (App, TempDir) {
        let temp = TempDir::new("app-branch").unwrap();
        let sup = fixture::make_super(
            &temp.path().join("super"),
            &[SubSpec {
                name: "subA",
                msgs: &["a1", "a2"],
                recorded: None,
                drop_workdir: false,
            }],
        )
        .unwrap();
        {
            let head = sup.repo.head().unwrap().peel_to_commit().unwrap();
            sup.repo.branch("feature", &head, false).unwrap();
        }

        let sub = crate::gitx::discover(&temp.path().join("super/subA"))
            .unwrap()
            .unwrap();
        let a1 = sup.sub_oids["subA"][0];
        let c1 = sub.find_commit(a1).unwrap();
        sub.branch("old", &c1, false).unwrap();

        let ov = crate::gitx::overview(&sup.repo).unwrap();
        (App::normal(ov, sup.repo), temp)
    }

    #[test]
    fn branch_popup_lists_local_branches() {
        let (mut app, _t) = branchy_app();
        assert!(app.branch_popup.is_none());

        app.handle_key(key(KeyCode::Char('b')));
        let bp = app.branch_popup.as_ref().expect("popup opens on b");
        assert_eq!(bp.entry_idx, 0);
        assert_eq!(
            bp.branches
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>(),
            vec!["feature", "main"],
            "alphabetical"
        );
        assert!(bp.branches[1].is_head, "main is current");
        assert!(!bp.branches[0].is_head);
        assert_eq!(bp.scroll.selected(), 0);
    }

    #[test]
    fn branch_switch_updates_state_and_closes_popup() {
        let (mut app, _t) = branchy_app();
        app.handle_key(key(KeyCode::Char('b')));
        crate::fixture::confirm_action(&mut app); // switch to `feature` (row 0)

        assert!(app.branch_popup.is_none(), "popup closes on success");
        let ov = app.overview.as_ref().unwrap();
        assert_eq!(ov.head, crate::gitx::HeadInfo::Branch("feature".into()));
        assert!(
            app.entries[0].loaded,
            "selected entry reloads after refresh"
        );
        // History still shows every commit (HEAD + branches).
        assert_eq!(app.entries[0].log.len(), 2);
        let events = crate::repository_history::events(&ov.root).unwrap().0;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].intent.kind, "branch switch");
    }

    #[test]
    fn dirty_tree_refusal_keeps_popup_with_error() {
        // Branches at *different trees* with a locally-modified tracked
        // file: switching would overwrite it, so it must be refused. (A
        // branch at the same commit switches fine even when dirty — git
        // behaves the same — so the fixture needs differing trees.)
        let temp = TempDir::new("app-branch-dirty").unwrap();
        let root = temp.path().join("r");
        let repo = fixture::init_repo(&root, "main").unwrap();
        let oid1 = fixture::commit_file(&repo, "f.txt", "one\n", "c1", BASE_TIME).unwrap();
        {
            let c1 = repo.find_commit(oid1).unwrap();
            repo.branch("feature", &c1, false).unwrap();
        }
        fixture::commit_file(&repo, "f.txt", "two\n", "c2", BASE_TIME + 60).unwrap();
        fs::write(root.join("f.txt"), "local\n").unwrap();

        let ov = crate::gitx::overview(&repo).unwrap();
        let mut app = App::normal(ov, repo);

        app.handle_key(key(KeyCode::Char('b')));
        crate::fixture::confirm_action(&mut app); // row 0 = feature → refused

        let bp = app.branch_popup.as_ref().expect("popup stays open");
        let notice = bp.notice.as_deref().expect("error shown");
        assert!(notice.contains("would be overwritten"), "got: {notice}");
        assert_eq!(
            app.overview.as_ref().unwrap().head,
            crate::gitx::HeadInfo::Branch("main".into()),
            "HEAD unchanged"
        );

        // Once the tree is clean again, the same switch succeeds.
        fs::write(root.join("f.txt"), "two\n").unwrap();
        crate::fixture::confirm_action(&mut app);
        assert!(app.branch_popup.is_none());
        assert_eq!(
            app.overview.as_ref().unwrap().head,
            crate::gitx::HeadInfo::Branch("feature".into())
        );
        assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), "one\n");
    }

    #[test]
    fn esc_closes_branch_popup_without_switching() {
        let (mut app, _t) = branchy_app();
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.branch_popup.is_none());
        assert_eq!(
            app.overview.as_ref().unwrap().head,
            crate::gitx::HeadInfo::Branch("main".into())
        );
    }

    #[test]
    fn branch_popup_on_submodule_switches_its_branch_only() {
        let (mut app, temp) = branchy_app();
        let a1 = {
            // stash the oids we care about before moving the app
            let sub = crate::gitx::discover(&temp.path().join("super/subA"))
                .unwrap()
                .unwrap();
            sub.head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .parent(0)
                .unwrap()
                .id()
        };

        app.handle_key(key(KeyCode::Down)); // select subA
        app.handle_key(key(KeyCode::Char('b')));
        let bp = app.branch_popup.as_ref().expect("popup opens");
        assert_eq!(bp.entry_idx, 1);
        assert_eq!(
            bp.branches
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>(),
            vec!["main", "old"]
        );

        app.handle_key(key(KeyCode::Down)); // → old
        crate::fixture::confirm_action(&mut app);

        assert!(app.branch_popup.is_none());
        let ov = app.overview.as_ref().unwrap();
        let sm = &ov.submodules[0];
        assert_eq!(
            sm.head.as_ref().unwrap(),
            &crate::gitx::HeadInfo::Branch("old".into()),
            "submodule switched"
        );
        assert_eq!(sm.workdir_oid, Some(a1), "submodule workdir at a1");
        assert_eq!(
            sm.recorded_oid, sm.index_oid,
            "the main repo's recorded pointer is untouched"
        );
        assert_ne!(sm.recorded_oid, Some(a1));
        assert!(ov.dirty, "drift makes the superproject dirty");
    }

    #[test]
    fn branch_popup_on_uninitialized_submodule_shows_notice() {
        let (mut app, _t) = normal_app();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down)); // subC (not initialized)
        app.handle_key(key(KeyCode::Char('b')));

        let bp = app.branch_popup.as_ref().expect("popup opens");
        assert!(bp.branches.is_empty());
        let notice = bp.notice.as_deref().expect("notice set");
        assert!(notice.contains("not initialized"), "got: {notice}");

        // Enter does nothing (nothing to switch to); Esc closes.
        crate::fixture::confirm_action(&mut app);
        assert!(app.branch_popup.is_some());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.branch_popup.is_none());
    }

    #[test]
    fn pick_survives_branch_switch() {
        let (mut app, _t) = branchy_app();
        // Pick the older commit on the main repo.
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Down));
        crate::fixture::confirm_action(&mut app);
        let pick = app.entries[0].pick.expect("pick set");

        app.handle_key(key(KeyCode::Tab)); // back to repos
        app.handle_key(key(KeyCode::Char('b')));
        crate::fixture::confirm_action(&mut app); // switch to feature

        assert_eq!(app.entries[0].pick, Some(pick), "pick survives the switch");
    }

    #[test]
    fn branch_popup_ignores_mouse_and_prompts_for_notes_on_enter() {
        let (mut app, _temp) = branchy_app();
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        app.handle_mouse(click(15, 10));
        app.handle_mouse(click(70, 2));
        assert_eq!(app.branch_popup.as_ref().unwrap().scroll.selected(), 0);
        assert!(!app.action_note_editing && app.session.is_none());
        app.handle_key(key(KeyCode::Enter));
        assert!(app.action_note_editing && app.session.is_none());
        app.handle_paste("Trial branch");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.branch_popup.is_none());
        assert_eq!(
            app.overview.as_ref().unwrap().head,
            crate::gitx::HeadInfo::Branch("feature".into())
        );
    }

    // ---- step 7: save flow ----------------------------------------------

    /// Pick the older commit of subA (a1) via keys, leaving the app ready
    /// to save.
    fn pick_sub_a_older(app: &mut App) -> git2::Oid {
        app.handle_key(key(KeyCode::Down)); // repos → subA
        app.handle_key(key(KeyCode::Tab)); // commits
        app.handle_key(key(KeyCode::Down)); // row 1 = a1
        crate::fixture::confirm_action(app); // pick
        app.entries[1].pick.expect("pick set")
    }

    #[test]
    fn save_confirm_lists_planned_change_with_details() {
        let (mut app, temp) = normal_app();
        let pick = pick_sub_a_older(&mut app);
        let tip = app.overview.as_ref().unwrap().submodules[0]
            .recorded_oid
            .unwrap();

        app.handle_key(key(KeyCode::Char('s')));
        match &app.save_popup {
            Some(SaveStage::Confirm(changes)) => {
                assert_eq!(changes.len(), 1);
                let c = &changes[0];
                assert_eq!(c.idx, 1);
                assert_eq!(c.name, "subA");
                assert_eq!(c.current, Some(tip));
                assert_eq!(c.pick, pick);
                assert_eq!(c.title.as_deref(), Some("a1"));
                assert!(c.dirty_warning.is_none(), "clean subA gets no warning");
            }
            other => panic!("expected Confirm, got {other:?}"),
        }

        // Dirtying subA adds a warning line.
        fs::write(temp.path().join("super/subA/untracked.txt"), "x").unwrap();
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('s')));
        match &app.save_popup {
            Some(SaveStage::Confirm(changes)) => {
                assert!(
                    changes[0].dirty_warning.is_some(),
                    "dirty subA gets a warning"
                );
            }
            other => panic!("expected Confirm, got {other:?}"),
        }
    }

    #[test]
    fn save_applies_submodule_pick_and_stages_pointer() {
        let (mut app, temp) = normal_app();
        let pick = pick_sub_a_older(&mut app);
        let tip = app.overview.as_ref().unwrap().submodules[0]
            .recorded_oid
            .unwrap();

        app.handle_key(key(KeyCode::Char('s')));
        crate::fixture::confirm_action(&mut app); // apply

        // Result popup: one success, no failures; pick cleared.
        match &app.save_popup {
            Some(SaveStage::Result(outcome)) => {
                assert_eq!(outcome.applied.len(), 1);
                assert!(outcome.failed.is_empty());
                assert!(outcome.applied[0].contains("subA"));
                assert!(outcome.applied[0].contains('→'));
            }
            other => panic!("expected Result, got {other:?}"),
        }
        assert_eq!(app.entries[1].pick, None, "applied pick is cleared");

        // On disk: submodule workdir moved to the old commit (detached),
        // superproject index staged it, HEAD tree still records the tip,
        // and the submodule's own branch/commits are untouched.
        let ov = app.overview.as_ref().unwrap();
        let sm = &ov.submodules[0];
        assert_eq!(sm.workdir_oid, Some(pick));
        assert_eq!(sm.index_oid, Some(pick), "gitlink staged");
        assert_eq!(sm.recorded_oid, Some(tip), "nothing was committed");
        assert_eq!(
            sm.head.as_ref().unwrap(),
            &crate::gitx::HeadInfo::Detached(pick)
        );
        assert!(!temp.path().join("super/subA/file1.txt").exists());
        let sub = crate::gitx::discover(&temp.path().join("super/subA"))
            .unwrap()
            .unwrap();
        let sub_main = sub
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .target()
            .unwrap();
        assert_eq!(sub_main, tip, "newer commits still reachable on the branch");
        let events = crate::repository_history::events(&ov.root).unwrap().0;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].intent.kind, "commit pick");
    }

    #[test]
    fn save_applies_main_repo_pick_as_detached_checkout() {
        let (mut app, temp) = normal_app();
        // Pick the root commit of the main repo.
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Down)); // row 1 = root
        crate::fixture::confirm_action(&mut app);
        let pick = app.entries[0].pick.expect("pick set");

        app.handle_key(key(KeyCode::Char('s')));
        crate::fixture::confirm_action(&mut app);

        let ov = app.overview.as_ref().unwrap();
        assert_eq!(ov.head, crate::gitx::HeadInfo::Detached(pick));
        assert_eq!(ov.head_oid, Some(pick));
        assert!(temp.path().join("super/README.md").exists());
        assert!(
            !temp.path().join("super/.gitmodules").exists(),
            "workdir at root commit"
        );
        assert_eq!(app.entries[0].pick, None);
    }

    #[test]
    fn save_failure_is_reported_and_pick_kept_for_retry() {
        let (mut app, temp) = normal_app();
        let root = temp.path().join("super");

        // Conflict setup: subA moves to a3 (changing file0.txt), the file is
        // then dirtied locally, and the pick targets a1 — a different commit
        // whose file0.txt differs, so the checkout must be refused. (Picking
        // the commit HEAD is already on is a legitimate no-op, not a failure.)
        let a3 = {
            let sub = crate::gitx::discover(&root.join("subA")).unwrap().unwrap();
            fixture::commit_file(&sub, "file0.txt", "changed\n", "a3", BASE_TIME + 7200).unwrap()
        };
        fs::write(root.join("subA/file0.txt"), "dirty\n").unwrap();

        app.handle_key(key(KeyCode::Down)); // subA
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Down)); // row 1 = a2
        app.handle_key(key(KeyCode::Down)); // row 2 = a1
        crate::fixture::confirm_action(&mut app); // pick a1
        let pick = app.entries[1].pick.expect("pick set");
        assert_ne!(Some(pick), app.current_oid(1));

        app.handle_key(key(KeyCode::Char('s')));
        // The dirty state is flagged as a warning in the confirmation.
        match &app.save_popup {
            Some(SaveStage::Confirm(changes)) => {
                assert!(changes[0].dirty_warning.is_some());
            }
            other => panic!("expected Confirm, got {other:?}"),
        }
        crate::fixture::confirm_action(&mut app); // apply → refused

        match &app.save_popup {
            Some(SaveStage::Result(outcome)) => {
                assert!(outcome.applied.is_empty());
                assert_eq!(outcome.failed.len(), 1);
                let (name, err) = &outcome.failed[0];
                assert_eq!(name, "subA");
                assert!(err.contains("would be overwritten"), "got: {err}");
            }
            other => panic!("expected Result, got {other:?}"),
        }
        assert_eq!(app.entries[1].pick, Some(pick), "failed pick kept");
        let ov = app.overview.as_ref().unwrap();
        assert_eq!(
            ov.submodules[0].workdir_oid,
            Some(a3),
            "submodule untouched: still at a3"
        );
        assert_eq!(
            ov.submodules[0].index_oid, ov.submodules[0].recorded_oid,
            "nothing staged"
        );

        // Restore the file to a1's content and retry: the pick now applies.
        app.handle_key(key(KeyCode::Esc)); // close result
        fs::write(root.join("subA/file0.txt"), "content of file0.txt\n").unwrap();
        app.handle_key(key(KeyCode::Char('s')));
        crate::fixture::confirm_action(&mut app);
        match &app.save_popup {
            Some(SaveStage::Result(outcome)) => {
                assert_eq!(outcome.applied.len(), 1);
                assert!(outcome.failed.is_empty());
            }
            other => panic!("expected Result, got {other:?}"),
        }
        assert_eq!(app.entries[1].pick, None);
        let ov = app.overview.as_ref().unwrap();
        assert_eq!(ov.submodules[0].index_oid, Some(pick), "staged after retry");
        assert_eq!(ov.submodules[0].workdir_oid, Some(pick));
    }

    #[test]
    fn esc_cancels_save_and_changes_nothing() {
        let (mut app, _t) = normal_app();
        let pick = pick_sub_a_older(&mut app);
        let tip = app.overview.as_ref().unwrap().submodules[0]
            .recorded_oid
            .unwrap();

        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Esc)); // cancel

        assert!(app.save_popup.is_none());
        assert_eq!(app.entries[1].pick, Some(pick), "pick still pending");
        let ov = app.overview.as_ref().unwrap();
        assert_eq!(ov.submodules[0].index_oid, Some(tip), "index untouched");
        assert_eq!(ov.submodules[0].workdir_oid, Some(tip), "workdir untouched");
    }

    #[test]
    fn save_with_no_picks_is_a_noop() {
        let (mut app, _t) = normal_app();
        app.handle_key(key(KeyCode::Char('s')));

        match &app.save_popup {
            Some(SaveStage::Confirm(changes)) => assert!(changes.is_empty()),
            other => panic!("expected Confirm, got {other:?}"),
        }
        crate::fixture::confirm_action(&mut app); // apply empty plan
        match &app.save_popup {
            Some(SaveStage::Result(outcome)) => {
                assert!(outcome.applied.is_empty());
                assert!(outcome.failed.is_empty());
            }
            other => panic!("expected Result, got {other:?}"),
        }
        // Nothing changed on disk.
        let ov = app.overview.as_ref().unwrap();
        let sm = &ov.submodules[0];
        assert_eq!(sm.index_oid, sm.recorded_oid);
        assert_eq!(sm.workdir_oid, sm.recorded_oid);
    }

    #[test]
    fn save_automatically_opens_notes_and_ignores_mouse() {
        let (mut app, _temp) = normal_app();
        let pick = pick_sub_a_older(&mut app);
        app.handle_key(key(KeyCode::Char('s')));
        assert!(app.action_note_editing && app.action_note.is_empty());
        app.hit_zones = vec![
            (Rect::new(10, 14, 9, 1), Hit::SaveApply),
            (Rect::new(30, 14, 10, 1), Hit::SaveCancel),
        ];
        app.handle_mouse(click(35, 14));
        app.handle_mouse(click(12, 14));
        assert!(app.session.is_none() && app.save_popup.is_some());
        app.handle_paste("Commit trial");
        app.handle_key(key(KeyCode::Enter));
        app.handle_mouse(click(12, 14));
        assert!(app.session.is_none());
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.overview.as_ref().unwrap().submodules[0].index_oid,
            Some(pick)
        );
        app.handle_mouse(click(32, 14));
        assert!(matches!(app.save_popup, Some(SaveStage::Result(_))));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.save_popup.is_none());
    }
}
