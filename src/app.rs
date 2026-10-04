//! Application state for the TUI: modes, focus, selection, popups, per-entry
//! commit browsing (lazily loaded histories, scroll positions, picks), and
//! the translation of terminal input into state changes.
//!
//! The state machine is deliberately terminal-free — `ui` renders it and
//! `event` feeds it — so it can be unit-tested without a tty.

use std::path::{Path, PathBuf};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use git2::Repository;
use ratatui::layout::Rect;

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

/// A clickable region, registered by the renderer each frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    PaneRepos,
    PaneCommits,
    RepoRow(usize),
    CommitRow(usize),
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
}

impl EntryState {
    fn new(key: EntryKey) -> Self {
        Self {
            key,
            log: CommitLog::empty(),
            scroll: ScrollState::new(0),
            pick: None,
            loaded: false,
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
    /// Clickable regions, refreshed by the renderer every frame.
    pub hit_zones: Vec<(Rect, Hit)>,
    pub pane_heights: PaneHeights,
    pub should_quit: bool,
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
            hit_zones: Vec::new(),
            pane_heights: PaneHeights::default(),
            should_quit: false,
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
            hit_zones: Vec::new(),
            pane_heights: PaneHeights::default(),
            should_quit: false,
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

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.handle_click(mouse.column, mouse.row),
            MouseEventKind::ScrollUp => self.wheel(-1),
            MouseEventKind::ScrollDown => self.wheel(1),
            _ => {}
        }
    }

    fn wheel(&mut self, dir: i32) {
        if self.mode != Mode::Normal || self.popup.is_some() {
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

    /// Load the history of entry `idx` (once); cheaper entries (submodules)
    /// are opened lazily and cached in `sub_repos`.
    fn ensure_entry_loaded(&mut self, idx: usize) {
        let App {
            overview,
            repo,
            sub_repos,
            entries,
            ..
        } = self;
        let Some(entry) = entries.get_mut(idx) else {
            return;
        };
        if entry.loaded {
            return;
        }
        entry.loaded = true;

        let entry_repo: Option<&Repository> = if idx == 0 {
            repo.as_ref()
        } else if let Some(cache) = sub_repos.get_mut(idx - 1) {
            if cache.is_none() {
                let workdir = overview
                    .as_ref()
                    .and_then(|ov| ov.submodules.get(idx - 1))
                    .and_then(|sm| sm.workdir.clone());
                // discover() resolves the submodule's `.git` file/link the
                // same way overview() decided it is initialized.
                *cache = workdir.and_then(|wd| gitx::discover(&wd).ok().flatten());
            }
            cache.as_ref()
        } else {
            None
        };

        if let Some(r) = entry_repo {
            let tips = gitx::history_tips(r);
            if let Ok(mut log) = CommitLog::load_from(r, &tips) {
                let total = log.len();
                let _ = log.ensure_loaded(r, INITIAL_WINDOW);
                entry.scroll.set_len(total);
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
            Some(entry) => entry.scroll.selected() + self.pane_heights.commits.max(1) + 2,
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
        let row = entry.scroll.selected();
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
        self.repos.set_len(1 + n);
        if n == 0 {
            self.focus = Focus::Commits;
        }
        self.ensure_entry_loaded(self.repos.selected());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, BASE_TIME, SubSpec, TempDir};
    use crossterm::event::MouseButton;

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
}
