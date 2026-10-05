//! Application state for the TUI: modes, focus, selection, popups, per-entry
//! commit browsing (lazily loaded histories, scroll positions, picks), and
//! the translation of terminal input into state changes.
//!
//! The state machine is deliberately terminal-free — `ui` renders it and
//! `event` feeds it — so it can be unit-tested without a tty.

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use git2::Repository;
use ratatui::layout::Rect;

use crate::gitops::{self, BranchEntry};
use crate::gitx::{self, CommitLog, RepoOverview, SubmoduleEntry};

/// Commits materialized when an entry is first opened.
const INITIAL_WINDOW: usize = 60;
/// Extra rows materialized ahead of the viewport while scrolling.
const AHEAD_MARGIN: usize = 40;
/// Rows scrolled per wheel notch in the commit list.
const WHEEL_STEP: i32 = 3;

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
}

/// State of the branch-switcher popup (`b`): the entry it acts on, that
/// entry's local branches, and a notice line (info or switch error).
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

/// A clickable region, registered by the renderer each frame.
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

/// Viewport heights of the two panes, recorded by the renderer so key
/// handling can page-scroll and keep the selection visible.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneHeights {
    pub repos: usize,
    pub commits: usize,
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
        }
    }
}

/// The whole application state, rendered by `ui` and mutated by input.
pub struct App {
    pub mode: Mode,
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
    /// Clickable regions, refreshed by the renderer every frame.
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
            hit_zones: Vec::new(),
            pane_heights: PaneHeights::default(),
            should_quit: false,
            filter: String::new(),
            filter_editing: false,
        };
        app.ensure_entry_loaded(0);
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
        // Ctrl+C always quits, whatever is on screen.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        match self.mode {
            Mode::NotRepo | Mode::Error => self.handle_message_key(key.code),
            Mode::Normal if self.save_popup.is_some() => self.handle_save_key(key.code),
            Mode::Normal if self.branch_popup.is_some() => self.handle_branch_key(key.code),
            Mode::Normal if self.filter_editing => self.handle_filter_key(key.code, key.modifiers),
            Mode::Normal if self.popup.is_some() => match key.code {
                KeyCode::Esc
                | KeyCode::Enter
                | KeyCode::Char('?')
                | KeyCode::Char('q')
                | KeyCode::Char('Q') => self.popup = None,
                _ => {}
            },
            Mode::Normal => self.handle_normal_key(key.code),
        }
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
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('?') => self.popup = Some(Popup::Help),
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
        entry.filtered_for = filter.clone();
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.handle_click(mouse.column, mouse.row),
            MouseEventKind::ScrollUp => self.wheel(-1),
            MouseEventKind::ScrollDown => self.wheel(1),
            _ => {}
        }
    }

    fn wheel(&mut self, dir: i32) {
        if self.mode != Mode::Normal || self.popup.is_some() || self.save_popup.is_some() {
            return;
        }
        if self.branch_popup.is_some() {
            self.branch_scroll(dir * WHEEL_STEP);
            return;
        }
        match self.focus {
            Focus::Repos => {
                self.repos.move_by(dir);
                self.ensure_entry_loaded(self.repos.selected());
                self.ensure_repos_visible();
            }
            Focus::Commits => {
                self.commit_scroll(|s| {
                    for _ in 0..WHEEL_STEP {
                        s.move_by(dir);
                    }
                });
            }
        }
    }

    fn handle_click(&mut self, col: u16, row: u16) {
        // Save popup: buttons act, clicks inside the body are ignored,
        // clicks outside close it (Cancel never applies anything).
        if self.mode == Mode::Normal && self.save_popup.is_some() {
            match self.hit_test(col, row) {
                Some(Hit::SaveApply) => {
                    if matches!(self.save_popup, Some(SaveStage::Confirm(_))) {
                        self.apply_saves();
                    } else {
                        self.save_popup = None;
                    }
                }
                Some(Hit::SaveCancel) | Some(Hit::SaveOk) => self.save_popup = None,
                Some(Hit::SavePopup) => {}
                _ => self.save_popup = None,
            }
            return;
        }
        // Branch popup: clicking a row selects and switches; anything else
        // just closes it.
        if self.mode == Mode::Normal && self.branch_popup.is_some() {
            match self.hit_test(col, row) {
                Some(Hit::BranchRow(i)) => {
                    if let Some(bp) = self.branch_popup.as_mut() {
                        bp.scroll.select(i);
                    }
                    self.switch_selected_branch();
                }
                _ => self.branch_popup = None,
            }
            return;
        }
        // While a popup is open, any click dismisses it and goes no further.
        if self.mode == Mode::Normal && self.popup.is_some() {
            self.popup = None;
            return;
        }
        match self.hit_test(col, row) {
            Some(Hit::RepoRow(i)) => {
                self.focus = Focus::Repos;
                self.repos.select(i);
                self.ensure_entry_loaded(i);
                self.ensure_repos_visible();
            }
            Some(Hit::CommitRow(r)) => {
                self.focus = Focus::Commits;
                let idx = self.repos.selected();
                self.ensure_entry_loaded(idx);
                if let Some(entry) = self.entries.get_mut(idx) {
                    entry.scroll.select(r);
                }
                self.ensure_commits_ahead();
            }
            Some(Hit::PaneRepos) if self.has_submodules() => self.focus = Focus::Repos,
            Some(Hit::PaneCommits) => self.focus = Focus::Commits,
            Some(Hit::OkButton) => self.should_quit = true,
            _ => {}
        }
    }

    /// Select entry `idx` in the repos pane and make sure its history is
    /// loaded (the public selection entry point used by keys, clicks and tests).
    pub fn select_repo(&mut self, idx: usize) {
        self.repos.select(idx);
        self.ensure_entry_loaded(self.repos.selected());
    }

    /// Most specific hit wins: the smallest registered region containing
    /// the click (a row inside a pane, not the pane).
    fn hit_test(&self, col: u16, row: u16) -> Option<Hit> {
        self.hit_zones
            .iter()
            .filter(|(rect, _)| {
                col >= rect.x
                    && col < rect.x + rect.width
                    && row >= rect.y
                    && row < rect.y + rect.height
            })
            .min_by_key(|(rect, _)| rect.width * rect.height)
            .map(|(_, hit)| *hit)
    }

    // ---- branch switcher -------------------------------------------------

    /// Rows per page inside the branch popup.
    const BRANCH_PAGE: i32 = 8;

    fn handle_branch_key(&mut self, code: KeyCode) {
        match code {
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
        let Some((idx, name)) = self.branch_popup.as_ref().and_then(|bp| {
            bp.branches
                .get(bp.scroll.selected())
                .map(|b| (bp.entry_idx, b.name.clone()))
        }) else {
            return;
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
        match gitops::switch_branch(repo, &name) {
            Ok(()) => {
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
        if self.mode != Mode::Normal {
            return;
        }
        self.refresh();
        let changes = self.pending_saves();
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

        let mut outcome = ApplyOutcome::default();
        let repo = self.repo.take();
        for (idx, name, path, pick, current) in plan {
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
            match result {
                Ok(()) => {
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
                let h = self.pane_heights.commits.max(1);
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
            f(&mut entry.scroll);
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
    }
}

fn short_oid(oid: git2::Oid) -> String {
    oid.to_string()[..7].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, BASE_TIME, SubSpec, TempDir};
    use crossterm::event::MouseButton;
    use std::fs;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
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
        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Enter));
        assert!(app.should_quit);
    }

    #[test]
    fn ctrl_c_quits_even_with_popup_open() {
        let (mut app, _t) = normal_app();
        app.popup = Some(Popup::Help);
        app.handle_key(ctrl_c());
        assert!(app.should_quit);
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
    fn click_selects_repo_row_and_focuses_pane() {
        let (mut app, _t) = normal_app();
        app.focus = Focus::Commits;
        app.hit_zones = vec![
            (Rect::new(2, 2, 20, 1), Hit::RepoRow(2)),
            (Rect::new(0, 1, 30, 20), Hit::PaneRepos),
            (Rect::new(31, 1, 40, 20), Hit::PaneCommits),
        ];

        app.handle_mouse(click(5, 2));
        assert_eq!(app.focus, Focus::Repos);
        assert_eq!(app.repos.selected(), 2);
        assert!(app.entries[2].loaded, "clicked entry loads immediately");

        // Clicking the commits pane (not a row) just moves focus.
        app.handle_mouse(click(40, 5));
        assert_eq!(app.focus, Focus::Commits);
        assert_eq!(app.repos.selected(), 2);
    }

    #[test]
    fn click_with_help_open_just_dismisses() {
        let (mut app, _t) = normal_app();
        app.popup = Some(Popup::Help);
        app.hit_zones = vec![(Rect::new(2, 2, 20, 1), Hit::RepoRow(1))];

        app.handle_mouse(click(5, 2));
        assert_eq!(app.popup, None);
        assert_eq!(app.repos.selected(), 0, "selection must not change");
    }

    #[test]
    fn clicking_ok_button_quits() {
        let mut app = App::not_repo(Path::new("/x"));
        app.hit_zones = vec![(Rect::new(36, 13, 8, 1), Hit::OkButton)];
        app.handle_mouse(click(40, 13));
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
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.entries[0].pick, None);

        // Move to the older commit and pick it.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        let picked = app.entries[0].pick.expect("pick recorded");
        assert_eq!(app.entries[0].log.get(1).map(|c| c.oid), Some(picked));
        assert_ne!(Some(picked), app.current_oid(0));

        // Enter again un-picks.
        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.filter_editing);
        assert_eq!(app.entries[0].visible.len(), 11);
        app.handle_key(key(KeyCode::End));
        let original = *app.entries[0].visible.last().unwrap();
        assert_eq!(app.entries[0].log.get(original).unwrap().title, "commit 3");
        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Enter));
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
    fn wheel_scrolls_the_focused_pane() {
        let (mut app, _t) = normal_app();
        app.focus = Focus::Commits;

        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        // Main history has 2 commits, so 3 notches clamp to the last row.
        assert_eq!(app.entries[0].scroll.selected(), 1);

        app.handle_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.entries[0].scroll.selected(), 0);
    }

    #[test]
    fn clicking_a_commit_row_selects_it() {
        let (mut app, _t) = normal_app();
        app.hit_zones = vec![(Rect::new(40, 5, 30, 1), Hit::CommitRow(1))];

        app.handle_mouse(click(45, 5));
        assert_eq!(app.focus, Focus::Commits);
        assert_eq!(app.entries[0].scroll.selected(), 1);
    }

    #[test]
    fn refresh_preserves_picks_and_reloads_state() {
        let (mut app, _t) = normal_app();
        app.handle_key(key(KeyCode::Tab)); // commits
        app.handle_key(key(KeyCode::Down)); // row 1
        app.handle_key(key(KeyCode::Enter)); // pick
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
        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Enter)); // switch to `feature` (row 0)

        assert!(app.branch_popup.is_none(), "popup closes on success");
        let ov = app.overview.as_ref().unwrap();
        assert_eq!(ov.head, crate::gitx::HeadInfo::Branch("feature".into()));
        assert!(
            app.entries[0].loaded,
            "selected entry reloads after refresh"
        );
        // History still shows every commit (HEAD + branches).
        assert_eq!(app.entries[0].log.len(), 2);
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
        app.handle_key(key(KeyCode::Enter)); // row 0 = feature → refused

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
        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Enter));

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
        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Enter));
        let pick = app.entries[0].pick.expect("pick set");

        app.handle_key(key(KeyCode::Tab)); // back to repos
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Enter)); // switch to feature

        assert_eq!(app.entries[0].pick, Some(pick), "pick survives the switch");
    }

    #[test]
    fn wheel_and_click_work_in_branch_popup() {
        let (mut app, _t) = branchy_app();
        app.handle_key(key(KeyCode::Char('b')));

        app.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(
            app.branch_popup.as_ref().unwrap().scroll.selected(),
            1,
            "wheel moves the popup selection"
        );

        app.hit_zones = vec![(Rect::new(10, 10, 20, 1), Hit::BranchRow(0))];
        app.handle_mouse(click(15, 10));
        assert!(
            app.branch_popup.is_none(),
            "click on a row switches and closes"
        );
        assert_eq!(
            app.overview.as_ref().unwrap().head,
            crate::gitx::HeadInfo::Branch("feature".into())
        );

        // A click outside any row just closes the popup.
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_mouse(click(70, 2));
        assert!(app.branch_popup.is_none());
    }

    // ---- step 7: save flow ----------------------------------------------

    /// Pick the older commit of subA (a1) via keys, leaving the app ready
    /// to save.
    fn pick_sub_a_older(app: &mut App) -> git2::Oid {
        app.handle_key(key(KeyCode::Down)); // repos → subA
        app.handle_key(key(KeyCode::Tab)); // commits
        app.handle_key(key(KeyCode::Down)); // row 1 = a1
        app.handle_key(key(KeyCode::Enter)); // pick
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
        app.handle_key(key(KeyCode::Enter)); // apply

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
    }

    #[test]
    fn save_applies_main_repo_pick_as_detached_checkout() {
        let (mut app, temp) = normal_app();
        // Pick the root commit of the main repo.
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Down)); // row 1 = root
        app.handle_key(key(KeyCode::Enter));
        let pick = app.entries[0].pick.expect("pick set");

        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Enter));

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
        app.handle_key(key(KeyCode::Enter)); // pick a1
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
        app.handle_key(key(KeyCode::Enter)); // apply → refused

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
        app.handle_key(key(KeyCode::Enter));
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
        app.handle_key(key(KeyCode::Enter)); // apply empty plan
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
    fn save_click_buttons_apply_and_cancel() {
        let (mut app, _t) = normal_app();
        let pick = pick_sub_a_older(&mut app);

        // Cancel button: nothing happens.
        app.handle_key(key(KeyCode::Char('s')));
        app.hit_zones = vec![
            (Rect::new(5, 5, 60, 10), Hit::SavePopup),
            (Rect::new(10, 14, 9, 1), Hit::SaveApply),
            (Rect::new(30, 14, 10, 1), Hit::SaveCancel),
        ];
        app.handle_mouse(click(35, 14)); // Cancel
        assert!(app.save_popup.is_none());
        assert_eq!(app.entries[1].pick, Some(pick));

        // A click inside the popup body does nothing.
        app.handle_key(key(KeyCode::Char('s')));
        app.handle_mouse(click(20, 8)); // SavePopup zone
        assert!(app.save_popup.is_some(), "body clicks are ignored");

        // Apply button: applies and shows the result.
        app.handle_mouse(click(12, 14)); // Apply
        match &app.save_popup {
            Some(SaveStage::Result(outcome)) => assert_eq!(outcome.applied.len(), 1),
            other => panic!("expected Result, got {other:?}"),
        }
        let ov = app.overview.as_ref().unwrap();
        assert_eq!(ov.submodules[0].index_oid, Some(pick));

        // OK button on the result popup closes it.
        app.hit_zones = vec![
            (Rect::new(5, 5, 60, 10), Hit::SavePopup),
            (Rect::new(30, 14, 6, 1), Hit::SaveOk),
        ];
        app.handle_mouse(click(32, 14));
        assert!(app.save_popup.is_none());
    }
}
