//! Application state for the TUI: modes, focus, selection, popups, and the
//! translation of terminal input into state changes.
//!
//! The state machine is deliberately terminal-free — `ui` renders it and
//! `event` feeds it — so it can be unit-tested without a tty.

use std::path::{Path, PathBuf};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::gitx::{RepoOverview, SubmoduleEntry};

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

/// The whole application state, rendered by `ui` and mutated by input.
pub struct App {
    pub mode: Mode,
    /// Message shown by the NotRepo/Error popup.
    pub message: String,
    /// Directory context shown in popups (cwd or repo root).
    pub context: PathBuf,
    /// Present only in `Mode::Normal`.
    pub overview: Option<RepoOverview>,
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
            focus: Focus::Repos,
            repos: ScrollState::new(0),
            popup: None,
            hit_zones: Vec::new(),
            pane_heights: PaneHeights::default(),
            should_quit: false,
        }
    }

    pub fn normal(overview: RepoOverview) -> Self {
        Self {
            mode: Mode::Normal,
            message: String::new(),
            context: overview.root.clone(),
            repos: ScrollState::new(1 + overview.submodules.len()),
            overview: Some(overview),
            focus: Focus::Repos,
            popup: None,
            hit_zones: Vec::new(),
            pane_heights: PaneHeights::default(),
            should_quit: false,
        }
    }

    /// The entry under the left pane's cursor.
    pub fn selected_entry(&self) -> Option<Entry<'_>> {
        let ov = self.overview.as_ref()?;
        entry_at(ov, self.repos.selected())
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
            KeyCode::Tab | KeyCode::BackTab => self.focus = self.focus.toggle(),
            KeyCode::Left => self.focus = Focus::Repos,
            KeyCode::Right => self.focus = Focus::Commits,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_repos(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_repos(1),
            KeyCode::PageUp => self.page_repos(-1),
            KeyCode::PageDown => self.page_repos(1),
            KeyCode::Home => {
                self.repos.first();
                self.ensure_repos_visible();
            }
            KeyCode::End => {
                self.repos.last();
                self.ensure_repos_visible();
            }
            _ => {}
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.handle_click(mouse.column, mouse.row),
            // Wheel scrolling arrives together with the commit browser (step 5).
            _ => {}
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
                self.ensure_repos_visible();
            }
            Some(Hit::PaneRepos) => self.focus = Focus::Repos,
            Some(Hit::PaneCommits) => self.focus = Focus::Commits,
            Some(Hit::OkButton) => self.should_quit = true,
            None => {}
        }
    }

    fn hit_test(&self, col: u16, row: u16) -> Option<Hit> {
        self.hit_zones
            .iter()
            .find(|(rect, _)| {
                col >= rect.x
                    && col < rect.x + rect.width
                    && row >= rect.y
                    && row < rect.y + rect.height
            })
            .map(|(_, hit)| *hit)
    }

    fn scroll_repos(&mut self, delta: i32) {
        self.repos.move_by(delta);
        self.ensure_repos_visible();
    }

    fn page_repos(&mut self, delta: i32) {
        let h = self.pane_heights.repos.max(1);
        self.repos.page_by(h, delta);
        self.ensure_repos_visible();
    }

    fn ensure_repos_visible(&mut self) {
        self.repos.ensure_visible(self.pane_heights.repos.max(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, SubSpec, TempDir};
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
        (App::normal(ov), temp)
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
}
