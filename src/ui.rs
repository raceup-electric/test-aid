//! Rendering: turn [`App`] state into widgets.
//!
//! The renderer also registers clickable regions (`Hit` zones) with the app
//! every frame, so the event loop can route mouse clicks without duplicating
//! layout math.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::{self, App, Entry, Focus, Hit, Mode, Popup};
use crate::gitx::HeadInfo;

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hit_zones.clear();
    // Degenerate terminals (e.g. a zero-sized pty) can render nothing safely.
    if f.area().width == 0 || f.area().height == 0 {
        return;
    }
    match app.mode {
        Mode::NotRepo | Mode::Error => draw_message_screen(f, app),
        Mode::Normal => draw_normal(f, app),
    }
}

/// Full-screen popup for the "not a repository" and error cases, with an
/// OK button (click or Enter) that exits.
fn draw_message_screen(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(Paragraph::new(" taid").dim(), area);

    let popup = centered_fixed(46, 7, area);
    f.render_widget(Clear, popup);
    let border = if app.mode == Mode::Error {
        Style::new().fg(Color::Red)
    } else {
        Style::new().fg(Color::Cyan)
    };
    let block = Block::bordered().title(" taid ").border_style(border);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let (title, detail, title_style) = if app.mode == Mode::Error {
        (
            "Cannot open this repository:",
            app.message.clone(),
            Color::Red,
        )
    } else {
        (
            "This directory is not a git repository:",
            app.context.display().to_string(),
            Color::White,
        )
    };
    let message = Paragraph::new(vec![
        Line::from(title).fg(title_style),
        Line::from(detail).dim(),
    ])
    .alignment(Alignment::Center)
    .wrap(Wrap { trim: false });
    f.render_widget(message, rows[0]);

    let label = "[ OK ]";
    let row = rows[1];
    let w = (label.len() as u16).min(row.width);
    if w > 0 && row.height > 0 {
        let x = row.x + (row.width - w) / 2;
        f.render_widget(
            Paragraph::new(label).style(
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(x, row.y, w, 1),
        );
        let hit_w = (w + 4).min(row.width);
        app.hit_zones.push((
            Rect::new(x.saturating_sub(2), row.y, hit_w, 1),
            Hit::OkButton,
        ));
    }

    f.render_widget(
        Paragraph::new("Enter or click OK")
            .dim()
            .alignment(Alignment::Center),
        rows[2],
    );
}

/// The main two-pane screen: header, repo list, commit pane, footer.
fn draw_normal(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    draw_header(f, app, rows[0]);
    draw_footer(f, app, rows[2]);

    let panes = Layout::horizontal([Constraint::Max(34), Constraint::Min(24)]).split(rows[1]);
    draw_repos_pane(f, app, panes[0]);
    draw_commits_pane(f, app, panes[1]);

    if app.popup == Some(Popup::Help) {
        draw_help(f, area);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let Some(ov) = &app.overview else {
        return;
    };
    let mut spans = vec![
        Span::styled(
            " taid",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {} ", ov.root.display())),
    ];
    spans.push(match &ov.head {
        HeadInfo::Branch(_) => Span::styled(
            format!("· {}", ov.head.describe()),
            Style::new().fg(Color::Green),
        ),
        HeadInfo::Detached(_) => Span::styled(
            format!("· {}", ov.head.describe()),
            Style::new().fg(Color::Yellow),
        ),
        HeadInfo::Unborn(_) => Span::styled(
            format!("· {}", ov.head.describe()),
            Style::new().fg(Color::DarkGray),
        ),
    });
    if ov.dirty {
        spans.push(Span::styled("  ● dirty", Style::new().fg(Color::Red)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let text = if app.popup.is_some() {
        " Esc — close "
    } else {
        " ↑↓/jk select · Tab/←→ panes · ? help · q quit "
    };
    f.render_widget(Paragraph::new(text).dim(), area);
}

fn pane_border(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

fn draw_repos_pane(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Repos;
    let block = Block::bordered()
        .title(" Repos ")
        .border_style(pane_border(focused));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.pane_heights.repos = inner.height as usize;

    let Some(ov) = &app.overview else {
        return;
    };
    let (offset, count) = app.repos.visible(inner.height as usize);
    for row in 0..count {
        let idx = offset + row;
        let rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        let line = match app::entry_at(ov, idx) {
            Some(Entry::Main(m)) => Line::from(vec![
                Span::styled(
                    root_name(&m.root),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(head_short(&m.head), Style::new().fg(Color::DarkGray)),
                dirty_span(m.dirty),
            ]),
            Some(Entry::Sub(sm)) => {
                let status = if sm.initialized {
                    match &sm.head {
                        Some(h) => Span::styled(head_short(h), Style::new().fg(Color::DarkGray)),
                        None => Span::styled("?", Style::new().fg(Color::DarkGray)),
                    }
                } else {
                    Span::styled("not initialized", Style::new().fg(Color::Red))
                };
                let mut spans = vec![Span::raw(sm.name.clone()), Span::raw("  "), status];
                if sm.dirty == Some(true) {
                    spans.push(dirty_span(true));
                }
                Line::from(spans)
            }
            None => continue,
        };
        let selected = app.repos.selected() == idx;
        let style = if selected {
            if focused {
                Style::new().bg(Color::Cyan).fg(Color::Black)
            } else {
                Style::new().bg(Color::DarkGray)
            }
        } else {
            Style::new()
        };
        f.render_widget(Paragraph::new(line.style(style)), rect);
        app.hit_zones.push((rect, Hit::RepoRow(idx)));
    }
    app.hit_zones.push((area, Hit::PaneRepos));
}

/// Placeholder commit pane: summary of the selected entry until the commit
/// browser lands in step 5.
fn draw_commits_pane(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Commits;
    let block = Block::bordered()
        .title(" Commits ")
        .border_style(pane_border(focused));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.pane_heights.commits = inner.height as usize;
    app.hit_zones.push((area, Hit::PaneCommits));

    let Some(ov) = &app.overview else {
        return;
    };
    let mut lines = match app::entry_at(ov, app.repos.selected()) {
        Some(Entry::Main(m)) => vec![
            kv("repository", m.root.display().to_string()),
            kv("HEAD", m.head.describe()),
            kv(
                "status",
                if m.dirty { "dirty" } else { "clean" }.to_string(),
            ),
        ],
        Some(Entry::Sub(sm)) => {
            let head = if !sm.initialized {
                "not initialized".to_string()
            } else {
                sm.head
                    .as_ref()
                    .map(|h| h.describe())
                    .unwrap_or_else(|| "?".to_string())
            };
            vec![
                kv("submodule", sm.name.clone()),
                kv("path", sm.path.clone()),
                kv("HEAD", head),
                kv(
                    "recorded",
                    sm.recorded_oid.map(short_oid).unwrap_or_else(|| "—".into()),
                ),
                kv(
                    "staged",
                    sm.index_oid.map(short_oid).unwrap_or_else(|| "—".into()),
                ),
                kv(
                    "workdir",
                    sm.workdir_oid.map(short_oid).unwrap_or_else(|| "—".into()),
                ),
                kv(
                    "status",
                    match sm.dirty {
                        Some(true) => "dirty".to_string(),
                        Some(false) => "clean".to_string(),
                        None => "—".to_string(),
                    },
                ),
            ]
        }
        None => vec![],
    };
    lines.push(Line::from(""));
    lines.push(Line::from("commit browser lands in step 5 — press ? for keys").dim());

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let popup = centered_rect(62, 70, area);
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Help ")
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let keys: &[(&str, &str)] = &[
        ("↑ ↓ / k j", "move selection"),
        ("PgUp PgDn", "scroll by page"),
        ("Home End", "first / last row"),
        ("Tab / Shift+Tab", "switch pane (repos ↔ commits)"),
        ("mouse click", "focus panes, select rows, press buttons"),
        ("?", "toggle this help"),
        ("q / Esc", "quit (or close a popup)"),
    ];
    let mut lines = vec![
        Line::from("Keys").add_modifier(Modifier::BOLD),
        Line::from(""),
    ];
    for (k, d) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<18}"), Style::new().fg(Color::Cyan)),
            Span::raw(*d),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from("  ● marks a dirty working tree").dim());
    lines.push(
        Line::from("  taid is strictly local: nothing is fetched, pushed or committed.").dim(),
    );
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn kv(k: &str, v: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k:<10}"), Style::new().fg(Color::DarkGray)),
        Span::raw(v.into()),
    ])
}

fn dirty_span(dirty: bool) -> Span<'static> {
    if dirty {
        Span::styled(" ●", Style::new().fg(Color::Red))
    } else {
        Span::raw("")
    }
}

fn head_short(head: &HeadInfo) -> String {
    match head {
        HeadInfo::Branch(b) => b.clone(),
        HeadInfo::Detached(oid) => oid.to_string()[..7].to_string(),
        HeadInfo::Unborn(_) => "no commits".to_string(),
    }
}

fn root_name(root: &std::path::Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| root.display().to_string())
}

fn short_oid(oid: git2::Oid) -> String {
    oid.to_string()[..7].to_string()
}

fn centered_fixed(w: u16, h: u16, r: Rect) -> Rect {
    // Clamp to the screen instead of enforcing a minimum, so degenerate
    // terminals (e.g. a zero-sized pty) cannot underflow.
    let w = w.min(r.width);
    let h = h.min(r.height);
    let x = r.x + (r.width - w) / 2;
    let y = r.y + (r.height - h) / 2;
    Rect::new(x, y, w, h)
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Percentage(percent_y),
        Constraint::Fill(1),
    ])
    .split(r);
    Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Percentage(percent_x),
        Constraint::Fill(1),
    ])
    .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Hit, Popup};
    use crate::fixture::{self, SubSpec, TempDir};
    use crate::gitx;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::Path;

    fn render(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen_text(buf: &ratatui::buffer::Buffer) -> String {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn make_normal_app() -> (App, TempDir) {
        let temp = TempDir::new("ui").unwrap();
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
        let ov = gitx::overview(&sup.repo).unwrap();
        (App::normal(ov), temp)
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn not_repo_screen_shows_message_and_ok_button() {
        let mut app = App::not_repo(Path::new("/some/dir"));
        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);

        assert!(text.contains("This directory is not a git repository"));
        assert!(text.contains("/some/dir"));
        assert!(text.contains("[ OK ]"));
        assert!(text.contains("Enter or click OK"));
        assert!(app.hit_zones.iter().any(|(_, h)| *h == Hit::OkButton));
    }

    #[test]
    fn clicking_rendered_ok_button_quits() {
        let mut app = App::not_repo(Path::new("/x"));
        render(&mut app, 80, 24);
        let (rect, _) = app
            .hit_zones
            .iter()
            .find(|(_, h)| *h == Hit::OkButton)
            .copied()
            .unwrap();
        app.handle_mouse(click(rect.x + rect.width / 2, rect.y));
        assert!(app.should_quit);
    }

    #[test]
    fn error_screen_shows_error_text() {
        let mut app = App::error(Path::new("/x"), "something broke");
        let buf = render(&mut app, 80, 24);
        assert!(screen_text(&buf).contains("something broke"));
    }

    #[test]
    fn normal_screen_lists_repos_and_header() {
        let (mut app, _t) = make_normal_app();
        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);

        assert!(text.contains("super"), "main repo name in list: {text}");
        assert!(text.contains("subA"));
        assert!(text.contains("subC"));
        assert!(text.contains("not initialized"));
        assert!(text.contains("main"), "branch name shown");
        assert!(text.contains('●'), "dirty marker (subC is uninitialized)");
        assert!(text.contains("Commits"), "right pane title");
    }

    #[test]
    fn clicking_rendered_repo_row_selects_it() {
        let (mut app, _t) = make_normal_app();
        render(&mut app, 80, 24);

        let (rect, _) = app
            .hit_zones
            .iter()
            .find(|(_, h)| *h == Hit::RepoRow(2))
            .copied()
            .unwrap();
        app.handle_mouse(click(rect.x + 2, rect.y));

        assert_eq!(app.repos.selected(), 2);
        assert_eq!(app.focus, crate::app::Focus::Repos);
    }

    #[test]
    fn help_popup_renders_keys_and_dismisses_on_click() {
        let (mut app, _t) = make_normal_app();
        app.popup = Some(Popup::Help);
        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);

        assert!(text.contains("move selection"));
        assert!(text.contains("strictly local"));

        app.handle_mouse(click(40, 12));
        assert_eq!(app.popup, None);
    }

    #[test]
    fn small_terminal_does_not_panic() {
        let (mut app, _t) = make_normal_app();
        for (w, h) in [(0, 0), (1, 1), (2, 2), (10, 4), (20, 6), (80, 24)] {
            render(&mut app, w, h);
        }
        let mut nr = App::not_repo(Path::new("/x"));
        for (w, h) in [(0, 0), (2, 2), (10, 4)] {
            render(&mut nr, w, h);
        }
    }
}
