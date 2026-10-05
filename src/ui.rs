//! Rendering: turn [`App`] state into widgets.
//!
//! The renderer also registers clickable regions (`Hit` zones) with the app
//! every frame, so the event loop can route mouse clicks without duplicating
//! layout math.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::{self, App, Entry, Focus, Hit, Mode, Popup, SaveStage};
use crate::gitx::HeadInfo;

// The same colors carry the same meaning in every pane and popup.
const ACCENT: Color = Color::Cyan;
const CURRENT: Color = Color::Yellow;
const PICKED: Color = Color::Green;
const WARNING: Color = Color::Red;
const MUTED: Color = Color::DarkGray;

fn selected_style(focused: bool) -> Style {
    if focused {
        Style::new().bg(ACCENT).fg(Color::Black)
    } else {
        Style::new().bg(MUTED)
    }
}

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
        Style::new().fg(WARNING)
    } else {
        Style::new().fg(ACCENT)
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
        ("Cannot open this repository:", app.message.clone(), WARNING)
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
                    .bg(ACCENT)
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

/// The main screen. Two panes when the repository has submodules (repo list
/// + commit browser); a single full-width commit browser otherwise.
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

    if app.has_submodules() {
        let panes = Layout::horizontal([Constraint::Max(34), Constraint::Min(24)]).split(rows[1]);
        draw_repos_pane(f, app, panes[0]);
        draw_commits_pane(f, app, panes[1]);
    } else {
        draw_commits_pane(f, app, rows[1]);
    }

    if app.popup == Some(Popup::Help) {
        draw_help(f, area);
    }
    if app.branch_popup.is_some() {
        draw_branch_popup(f, app);
    }
    if app.save_popup.is_some() {
        draw_save_popup(f, app);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let Some(ov) = &app.overview else {
        return;
    };
    let head_text = format!("· {}", ov.head.describe());
    let dirty_text = if ov.dirty { "  ● dirty" } else { "" };
    let path_width = (area.width as usize).saturating_sub(
        UnicodeWidthStr::width(" taid")
            + UnicodeWidthStr::width(head_text.as_str())
            + UnicodeWidthStr::width(dirty_text)
            + 2,
    );
    let mut spans = vec![
        Span::styled(
            " taid",
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " {} ",
            ellipsize(&ov.root.display().to_string(), path_width)
        )),
    ];
    spans.push(match &ov.head {
        HeadInfo::Branch(_) => Span::styled(head_text, Style::new().fg(PICKED)),
        HeadInfo::Detached(_) => Span::styled(head_text, Style::new().fg(CURRENT)),
        HeadInfo::Unborn(_) => Span::styled(head_text, Style::new().fg(MUTED)),
    });
    if ov.dirty {
        spans.push(Span::styled(dirty_text, Style::new().fg(WARNING)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let text = if app.save_popup.is_some() {
        " Enter apply · Esc cancel "
    } else if app.branch_popup.is_some() {
        " ↑↓ select · Enter switch · Esc close "
    } else if app.popup.is_some() {
        " Esc — close "
    } else if app.filter_editing {
        let prompt = format!(" /{}▌  Enter keep · Esc clear · Ctrl+U clear ", app.filter);
        f.render_widget(
            Paragraph::new(ellipsize(&prompt, area.width as usize)).fg(ACCENT),
            area,
        );
        return;
    } else if app.has_submodules() {
        " ↑↓ move · Tab panes · Enter pick · / filter · b branch · s save · ? help · q quit "
    } else {
        " ↑↓ move · Enter pick · / filter · b branch · s save · ? help · q quit "
    };
    f.render_widget(Paragraph::new(text).dim(), area);
}

fn pane_border(focused: bool) -> Style {
    if focused {
        Style::new().fg(ACCENT)
    } else {
        Style::new().fg(MUTED)
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
        let pick = app.entries.get(idx).and_then(|e| e.pick);
        let line = match app::entry_at(ov, idx) {
            Some(Entry::Main(m)) => {
                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled(head_short(&m.head), Style::new().fg(MUTED)),
                ];
                if pick.is_some() {
                    spans.push(Span::styled(
                        " ✓",
                        Style::new().fg(PICKED).add_modifier(Modifier::BOLD),
                    ));
                }
                spans.push(dirty_span(m.dirty));
                let name_width =
                    (inner.width as usize).saturating_sub(Line::from(spans.clone()).width());
                spans.insert(
                    0,
                    Span::styled(
                        ellipsize(&root_name(&m.root), name_width),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                );
                Line::from(spans)
            }
            Some(Entry::Sub(sm)) => {
                let mut spans = vec![Span::raw("  ")];
                if !sm.initialized {
                    spans.push(Span::styled("not initialized", Style::new().fg(WARNING)));
                } else {
                    match sm.recorded_oid {
                        Some(r) => spans.push(Span::styled(short_oid(r), Style::new().fg(CURRENT))),
                        None => spans.push(Span::styled("?", Style::new().fg(MUTED))),
                    }
                    if sm.recorded_oid != sm.workdir_oid {
                        if let Some(w) = sm.workdir_oid {
                            spans.push(dim(" ≠"));
                            spans.push(Span::styled(short_oid(w), Style::new().fg(ACCENT)));
                        }
                    }
                }
                if pick.is_some() {
                    spans.push(Span::styled(
                        " ✓",
                        Style::new().fg(PICKED).add_modifier(Modifier::BOLD),
                    ));
                }
                if sm.dirty == Some(true) {
                    spans.push(dirty_span(true));
                }
                let name_width =
                    (inner.width as usize).saturating_sub(Line::from(spans.clone()).width());
                spans.insert(0, Span::raw(ellipsize(&sm.name, name_width)));
                Line::from(spans)
            }
            None => continue,
        };
        let selected = app.repos.selected() == idx;
        let style = if selected {
            selected_style(focused)
        } else {
            Style::new()
        };
        f.render_widget(Paragraph::new(line.style(style)), rect);
        app.hit_zones.push((rect, Hit::RepoRow(idx)));
    }
    app.hit_zones.push((area, Hit::PaneRepos));
}

/// The commit browser: a two-line summary of the selected entry plus its
/// commit history, newest first.
fn draw_commits_pane(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width < 8 || area.height < 3 {
        return;
    }
    let focused = app.focus == Focus::Commits;
    let selected = app.repos.selected();

    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    let summary: Vec<_> = summary_lines(app)
        .into_iter()
        .map(|line| ellipsize_line(line, chunks[0].width as usize))
        .collect();
    f.render_widget(Paragraph::new(summary), chunks[0]);

    let (title, empty_msg) = list_meta(app, selected);
    let block = Block::bordered()
        .title(ellipsize(
            &title,
            (chunks[1].width as usize).saturating_sub(2),
        ))
        .border_style(pane_border(focused));
    let inner = block.inner(chunks[1]);

    // Gather rows (immutable borrow of app), then render and register zones.
    struct RowDraw {
        line: Line<'static>,
        selected: bool,
        index: usize,
    }
    let rows: Vec<RowDraw> = match app.entries.get(selected) {
        None => Vec::new(),
        Some(entry) => {
            let recorded = app.current_oid(selected);
            let workdir = app.workdir_oid(selected);
            let (offset, count) = entry.scroll.visible(inner.height as usize);
            (offset..offset + count)
                .map(|i| {
                    let is_selected = entry.scroll.selected() == i;
                    match entry
                        .visible
                        .get(i)
                        .and_then(|&original| entry.log.get(original))
                    {
                        Some(c) => {
                            let (marker, color) = if entry.pick == Some(c.oid) {
                                ('✓', PICKED)
                            } else if recorded == Some(c.oid) {
                                ('●', CURRENT)
                            } else if selected > 0 && Some(c.oid) == workdir {
                                ('○', MUTED)
                            } else {
                                (' ', Color::Reset)
                            };
                            let mut spans = vec![
                                Span::styled(marker.to_string(), Style::new().fg(color)),
                                Span::raw(" "),
                                Span::styled(c.short_sha.clone(), Style::new().fg(CURRENT)),
                                Span::raw(" "),
                                Span::styled(c.time_str(), Style::new().fg(MUTED)),
                                Span::raw(" "),
                            ];
                            let title_width = (inner.width as usize)
                                .saturating_sub(Line::from(spans.clone()).width());
                            spans.push(Span::raw(ellipsize(&c.title, title_width)));
                            let line = Line::from(spans);
                            RowDraw {
                                line,
                                selected: is_selected,
                                index: i,
                            }
                        }
                        None => RowDraw {
                            line: Line::from("…").dim(),
                            selected: is_selected,
                            index: i,
                        },
                    }
                })
                .collect()
        }
    };

    f.render_widget(block, chunks[1]);
    app.pane_heights.commits = inner.height as usize;
    app.hit_zones.push((chunks[1], Hit::PaneCommits));

    if let Some(msg) = empty_msg {
        f.render_widget(Paragraph::new(msg).dim().wrap(Wrap { trim: true }), inner);
        return;
    }

    for (row, r) in rows.into_iter().enumerate() {
        let rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        let style = if r.selected {
            selected_style(focused)
        } else {
            Style::new()
        };
        f.render_widget(Paragraph::new(r.line).style(style), rect);
        app.hit_zones.push((rect, Hit::CommitRow(r.index)));
    }
}

/// Title and, when there is nothing to browse, an explanatory message.
fn list_meta(app: &App, idx: usize) -> (String, Option<String>) {
    let Some(ov) = &app.overview else {
        return (String::new(), None);
    };
    let total = app.entries.get(idx).map(|e| e.log.len()).unwrap_or(0);
    let shown = app.entries.get(idx).map(|e| e.visible.len()).unwrap_or(0);
    let count = if app.filter.is_empty() {
        format!("{total}")
    } else {
        format!("{shown}/{total} matching /{}", app.filter)
    };
    match app::entry_at(ov, idx) {
        Some(Entry::Main(m)) => (
            format!(" History — {} ({count}) ", root_name(&m.root)),
            (shown == 0).then(|| {
                if total == 0 {
                    "no commits yet"
                } else {
                    "no matching commits"
                }
                .to_string()
            }),
        ),
        Some(Entry::Sub(sm)) if !sm.initialized => (
            format!(" History — {} ({count}) ", sm.name),
            Some(format!(
                "submodule not initialized — nothing to browse\nrun: git submodule update --init {}",
                sm.path
            )),
        ),
        Some(Entry::Sub(sm)) => (
            format!(" History — {} ({count}) ", sm.name),
            (shown == 0).then(|| {
                if total == 0 {
                    "no commits yet"
                } else {
                    "no matching commits"
                }
                .to_string()
            }),
        ),
        None => (String::new(), None),
    }
}

/// Two summary lines for the selected entry: identity/state, then the pick.
fn summary_lines(app: &App) -> Vec<Line<'static>> {
    let Some(ov) = &app.overview else {
        return vec![Line::from(""), Line::from("")];
    };
    let idx = app.repos.selected();

    let first: Vec<Span<'static>> = match app::entry_at(ov, idx) {
        Some(Entry::Main(m)) => vec![
            Span::styled(
                root_name(&m.root),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            dim(" · "),
            head_span(&m.head),
            dim(" · "),
            status_span(m.dirty),
        ],
        Some(Entry::Sub(sm)) if sm.initialized => {
            let mut spans = vec![
                Span::styled(sm.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
                dim(" (submodule) · "),
            ];
            if let Some(h) = &sm.head {
                spans.push(head_span(h));
                spans.push(dim(" · "));
            }
            let current = app.current_oid(idx);
            let workdir = app.workdir_oid(idx);
            if current != workdir {
                if let Some(c) = current {
                    spans.push(dim("rec "));
                    spans.push(Span::styled(short_oid(c), Style::new().fg(CURRENT)));
                }
                if let Some(w) = workdir {
                    spans.push(dim(" ≠ wd "));
                    spans.push(Span::styled(short_oid(w), Style::new().fg(ACCENT)));
                }
                spans.push(dim(" · "));
            } else if let Some(c) = current {
                spans.push(Span::styled(short_oid(c), Style::new().fg(CURRENT)));
                spans.push(dim(" · "));
            }
            spans.push(status_span(sm.dirty.unwrap_or(false)));
            spans
        }
        Some(Entry::Sub(sm)) => vec![
            Span::styled(sm.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
            dim(" (submodule) · "),
            Span::styled("not initialized", Style::new().fg(WARNING)),
            dim(" · "),
            dim(format!(
                "recorded {}",
                sm.recorded_oid
                    .map(short_oid)
                    .unwrap_or_else(|| "—".to_string())
            )),
        ],
        None => Vec::new(),
    };
    vec![Line::from(first), pick_line(app, idx)]
}

/// The second summary line: the pending pick, or blank.
fn pick_line(app: &App, idx: usize) -> Line<'static> {
    let Some(oid) = app.entries.get(idx).and_then(|e| e.pick) else {
        return Line::from("");
    };
    let mut spans = vec![Span::styled(
        "✓ pick",
        Style::new().fg(PICKED).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled(
        format!(" {}", short_oid(oid)),
        Style::new().fg(CURRENT),
    ));
    match app.entries.get(idx).and_then(|e| e.log.find(oid)) {
        Some(c) => {
            spans.push(Span::styled(
                format!("  {}", c.time_str()),
                Style::new().fg(MUTED),
            ));
            spans.push(Span::raw(format!("  {}", c.title)));
        }
        None => spans.push(dim("  (beyond loaded history)")),
    }
    Line::from(spans)
}

/// The branch switcher (`b`): local branches of the selected entry, with the
/// current one marked. On submodules it carries a footnote: switching their
/// branch does not change what the main repository records.
fn draw_branch_popup(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let Some(bp) = app.branch_popup.as_ref() else {
        return;
    };
    let Some(ov) = app.overview.as_ref() else {
        return;
    };

    let (title, head_desc, is_sub) = match app::entry_at(ov, bp.entry_idx) {
        Some(Entry::Main(m)) => (
            format!(" Branches — {} ", root_name(&m.root)),
            m.head.describe(),
            false,
        ),
        Some(Entry::Sub(sm)) => (
            format!(" Branches — {} ", sm.name),
            sm.head
                .as_ref()
                .map(|h| h.describe())
                .unwrap_or_else(|| "not initialized".to_string()),
            true,
        ),
        None => return,
    };

    let list_h = (bp.branches.len() as u16).clamp(1, 12);
    let mut h = 3 + 1 + list_h + 1; // borders + header + list + hint
    if bp.notice.is_some() {
        h += 2;
    }
    if is_sub {
        h += 2; // the footnote wraps to two lines at this width
    }
    let popup = centered_fixed(58, h, area);
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(ellipsize(&title, (popup.width as usize).saturating_sub(2)))
        .border_style(Style::new().fg(ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let mut constraints = vec![Constraint::Length(1)];
    if bp.notice.is_some() {
        constraints.push(Constraint::Length(2));
    }
    constraints.push(Constraint::Length(list_h));
    if is_sub {
        constraints.push(Constraint::Length(2));
    }
    constraints.push(Constraint::Length(1));
    let chunks = Layout::vertical(constraints).split(inner);
    let mut section = 0;
    let header_area = chunks[section];
    section += 1;
    let notice_area = bp.notice.is_some().then(|| {
        let a = chunks[section];
        section += 1;
        a
    });
    let list_area = chunks[section];
    section += 1;
    let footnote_area = is_sub.then(|| {
        let a = chunks[section];
        section += 1;
        a
    });
    let hint_area = chunks[section];

    f.render_widget(
        Paragraph::new(ellipsize(
            &format!("HEAD: {head_desc}"),
            header_area.width as usize,
        ))
        .style(Style::new().fg(MUTED)),
        header_area,
    );
    if let (Some(a), Some(n)) = (notice_area, bp.notice.as_ref()) {
        f.render_widget(
            Paragraph::new(n.clone())
                .style(Style::new().fg(WARNING))
                .wrap(Wrap { trim: false }),
            a,
        );
    }

    let (offset, count) = bp.scroll.visible(list_h as usize);
    for r in 0..count {
        let i = offset + r;
        let Some(b) = bp.branches.get(i) else {
            continue;
        };
        let rect = Rect::new(list_area.x, list_area.y + r as u16, list_area.width, 1);
        let selected = bp.scroll.selected() == i;
        let mut spans = vec![Span::styled(
            if b.is_head { "● " } else { "  " },
            Style::new().fg(CURRENT),
        )];
        spans.push(if b.is_head {
            Span::styled(b.name.clone(), Style::new().fg(PICKED))
        } else {
            Span::raw(b.name.clone())
        });
        let style = if selected {
            selected_style(true)
        } else {
            Style::new()
        };
        f.render_widget(
            Paragraph::new(ellipsize_line(Line::from(spans), rect.width as usize)).style(style),
            rect,
        );
        app.hit_zones.push((rect, Hit::BranchRow(i)));
    }

    if let Some(a) = footnote_area {
        f.render_widget(
            Paragraph::new(
                "switching a submodule's branch does not change what the main repo records",
            )
            .style(Style::new().fg(MUTED))
            .wrap(Wrap { trim: false }),
            a,
        );
    }
    f.render_widget(
        Paragraph::new(" Enter switch · Esc close ").style(Style::new().fg(MUTED)),
        hint_area,
    );
}

/// The save popup (`s`): confirmation listing every planned change, then the
/// apply result. Clicking Apply/OK/Cancel works; clicks inside the body are
/// ignored so a stray click cannot cancel or apply anything.
fn draw_save_popup(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 20 || area.height < 7 {
        return;
    }

    // Owned view of the popup data, so hit zones can be registered after.
    struct ChangeView {
        name: String,
        old: String,
        new: String,
        title: Option<String>,
        warn: Option<String>,
    }
    enum View {
        Confirm(Vec<ChangeView>),
        Result {
            applied: Vec<String>,
            failed: Vec<(String, String)>,
        },
    }
    let view = match &app.save_popup {
        Some(SaveStage::Confirm(changes)) => View::Confirm(
            changes
                .iter()
                .map(|c| ChangeView {
                    name: c.name.clone(),
                    old: c.current.map(short_oid).unwrap_or_else(|| "—".into()),
                    new: short_oid(c.pick),
                    title: c.title.clone(),
                    warn: c.dirty_warning.clone(),
                })
                .collect(),
        ),
        Some(SaveStage::Result(outcome)) => View::Result {
            applied: outcome.applied.clone(),
            failed: outcome.failed.clone(),
        },
        None => return,
    };

    match &view {
        View::Confirm(changes) => {
            let n = changes.len();
            let shown = n.min(8);
            let extra = n - shown;
            let warns: Vec<&str> = changes.iter().filter_map(|c| c.warn.as_deref()).collect();

            let mut constraints: Vec<Constraint> = Vec::new();
            constraints.extend(std::iter::repeat_n(Constraint::Length(1), shown));
            if extra > 0 {
                constraints.push(Constraint::Length(1));
            }
            constraints.extend(std::iter::repeat_n(Constraint::Length(1), warns.len()));
            constraints.push(Constraint::Length(1)); // note
            constraints.push(Constraint::Length(1)); // buttons

            let title = if n == 1 {
                " Save — apply 1 change? ".to_string()
            } else {
                format!(" Save — apply {n} changes? ")
            };
            let popup = centered_fixed(60, constraints.len() as u16 + 2, area);
            f.render_widget(Clear, popup);
            let block = Block::bordered()
                .title(title)
                .border_style(Style::new().fg(ACCENT));
            let inner = block.inner(popup);
            f.render_widget(block, popup);
            app.hit_zones.push((popup, Hit::SavePopup));

            let rows = Layout::vertical(constraints).split(inner);
            let mut r = 0;
            for c in changes.iter().take(shown) {
                let reserved = UnicodeWidthStr::width("  ")
                    + UnicodeWidthStr::width(c.old.as_str())
                    + UnicodeWidthStr::width(" → ")
                    + UnicodeWidthStr::width(c.new.as_str());
                let name_width = (rows[r].width as usize).saturating_sub(reserved);
                let mut spans = vec![
                    Span::styled(
                        ellipsize(&c.name, name_width),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                    dim("  "),
                    Span::styled(c.old.clone(), Style::new().fg(CURRENT)),
                    dim(" → "),
                    Span::styled(c.new.clone(), Style::new().fg(PICKED)),
                ];
                if let Some(t) = &c.title {
                    spans.push(dim(format!("  {t}")));
                }
                f.render_widget(
                    Paragraph::new(ellipsize_line(Line::from(spans), rows[r].width as usize)),
                    rows[r],
                );
                r += 1;
            }
            if extra > 0 {
                f.render_widget(Paragraph::new(format!("… and {extra} more")).dim(), rows[r]);
                r += 1;
            }
            for w in warns {
                f.render_widget(
                    Paragraph::new(ellipsize(&format!("⚠ {w}"), rows[r].width as usize))
                        .style(Style::new().fg(CURRENT)),
                    rows[r],
                );
                r += 1;
            }
            f.render_widget(
                Paragraph::new("nothing is committed or pushed — pointers are only staged").dim(),
                rows[r],
            );
            r += 1;

            if rows[r].height > 0 {
                if n == 0 {
                    f.render_widget(
                        Paragraph::new("nothing picked yet — pick commits with Enter").dim(),
                        rows[0],
                    );
                    draw_button(
                        f,
                        app,
                        rows[r],
                        "[ Cancel ]",
                        Hit::SaveCancel,
                        ButtonPos::Center,
                    );
                } else {
                    draw_button(
                        f,
                        app,
                        rows[r],
                        "[ Apply ]",
                        Hit::SaveApply,
                        ButtonPos::Left,
                    );
                    draw_button(
                        f,
                        app,
                        rows[r],
                        "[ Cancel ]",
                        Hit::SaveCancel,
                        ButtonPos::Right,
                    );
                }
            }
        }
        View::Result { applied, failed } => {
            let shown_applied = applied.len().min(8);
            let shown_failed = failed.len().min(3);
            let empty = applied.is_empty() && failed.is_empty();
            let mut lines = 0;
            lines += shown_applied;
            if applied.len() > shown_applied {
                lines += 1;
            }
            lines += shown_failed * 3;
            if failed.len() > shown_failed {
                lines += 1;
            }
            if empty {
                lines += 1;
            }

            let popup = centered_fixed(60, lines as u16 + 3, area);
            f.render_widget(Clear, popup);
            let block =
                Block::bordered()
                    .title(" Save result ")
                    .border_style(if failed.is_empty() {
                        Style::new().fg(PICKED)
                    } else {
                        Style::new().fg(WARNING)
                    });
            let inner = block.inner(popup);
            f.render_widget(block, popup);
            app.hit_zones.push((popup, Hit::SavePopup));

            let mut constraints: Vec<Constraint> =
                std::iter::repeat_n(Constraint::Length(1), shown_applied).collect();
            if applied.len() > shown_applied {
                constraints.push(Constraint::Length(1));
            }
            constraints.extend(std::iter::repeat_n(Constraint::Length(3), shown_failed));
            if failed.len() > shown_failed {
                constraints.push(Constraint::Length(1));
            }
            if empty {
                constraints.push(Constraint::Length(1));
            }
            constraints.push(Constraint::Length(1)); // buttons
            let rows = Layout::vertical(constraints).split(inner);

            let mut r = 0;
            for msg in applied.iter().take(shown_applied) {
                f.render_widget(
                    Paragraph::new(ellipsize(&format!("✓ {msg}"), rows[r].width as usize))
                        .style(Style::new().fg(PICKED)),
                    rows[r],
                );
                r += 1;
            }
            if applied.len() > shown_applied {
                f.render_widget(
                    Paragraph::new(format!("… and {} more", applied.len() - shown_applied)).dim(),
                    rows[r],
                );
                r += 1;
            }
            for (name, err) in failed.iter().take(shown_failed) {
                f.render_widget(
                    Paragraph::new(vec![
                        Line::from(format!("✗ {name}")).style(Style::new().fg(WARNING)),
                        Line::from(format!("  {err}")).dim(),
                    ])
                    .wrap(Wrap { trim: false }),
                    rows[r],
                );
                r += 1;
            }
            if failed.len() > shown_failed {
                f.render_widget(
                    Paragraph::new(format!("… and {} more", failed.len() - shown_failed)).dim(),
                    rows[r],
                );
                r += 1;
            }
            if empty {
                f.render_widget(Paragraph::new("nothing to apply").dim(), rows[r]);
                r += 1;
            }
            if rows[r].height > 0 {
                draw_button(f, app, rows[r], "[ OK ]", Hit::SaveOk, ButtonPos::Center);
            }
        }
    }
}

/// Horizontal placement of a popup button inside its row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonPos {
    Left,
    Center,
    Right,
}

/// Render a button inside `row` and register its hit zone.
fn draw_button(
    f: &mut Frame,
    app: &mut App,
    row: Rect,
    label: &str,
    hit: Hit,
    pos: ButtonPos,
) -> Rect {
    let w = (label.len() as u16).min(row.width);
    if w == 0 || row.height == 0 {
        return Rect::new(row.x, row.y, 0, 0);
    }
    let x = match pos {
        ButtonPos::Center => row.x + (row.width - w) / 2,
        ButtonPos::Left => (row.x + row.width / 2)
            .saturating_sub(w)
            .saturating_sub(2)
            .max(row.x),
        ButtonPos::Right => (row.x + row.width / 2 + 2).min(row.x + row.width - w),
    };
    let rect = Rect::new(x, row.y, w, 1);
    f.render_widget(
        Paragraph::new(label).style(
            Style::new()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        rect,
    );
    app.hit_zones.push((rect, hit));
    rect
}

fn draw_help(f: &mut Frame, area: Rect) {
    let popup = centered_rect(70, 90, area);
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Help ")
        .border_style(Style::new().fg(ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let keys: &[(&str, &str)] = &[
        ("↑ ↓ / k j", "move selection"),
        ("PgUp PgDn / Home End", "scroll by page / first-last"),
        ("Tab / Shift+Tab", "switch pane (repos ↔ commits)"),
        ("Enter", "pick / un-pick recorded commit"),
        ("b", "switch branch of repo/submodule"),
        ("s", "save: apply picked commits"),
        ("r", "refresh repository state"),
        ("/", "filter commits by title or SHA"),
        ("click / wheel", "select rows, focus panes, scroll"),
        ("?", "toggle this help"),
        ("q / Esc", "quit (or close a popup)"),
    ];
    let mut lines = vec![
        Line::from("Keys").add_modifier(Modifier::BOLD),
        Line::from(""),
    ];
    for (k, d) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<22}"), Style::new().fg(ACCENT)),
            Span::raw(*d),
        ]));
    }
    lines.push(Line::from("Markers"));
    lines.push(Line::from("  ● recorded / current commit"));
    lines.push(Line::from("  ○ where the submodule's working directory is"));
    lines.push(Line::from("  ✓ your pick"));
    lines.push(Line::from(
        "  taid is strictly local: nothing is fetched or pushed.".dim(),
    ));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Clip by terminal cells, keeping Unicode grapheme clusters intact.
fn take_cells(text: &str, max_width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width > max_width {
            break;
        }
        result.push_str(grapheme);
        used += width;
    }
    result
}

fn ellipsize(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    format!("{}…", take_cells(text, max_width - 1))
}

fn ellipsize_line(line: Line<'static>, max_width: usize) -> Line<'static> {
    if line.width() <= max_width {
        return line;
    }
    if max_width == 0 {
        return Line::from("");
    }
    let mut remaining = max_width - 1;
    let mut spans = Vec::new();
    for span in line.spans {
        let prefix = take_cells(&span.content, remaining);
        remaining -= UnicodeWidthStr::width(prefix.as_str());
        if !prefix.is_empty() {
            spans.push(Span::styled(prefix, span.style));
        }
        if remaining == 0 {
            break;
        }
    }
    spans.push(Span::styled("…", Style::new().fg(MUTED)));
    Line::from(spans)
}

fn dim(s: impl Into<Cow<'static, str>>) -> Span<'static> {
    Span::styled(s, Style::new().fg(MUTED))
}

fn head_span(h: &HeadInfo) -> Span<'static> {
    match h {
        HeadInfo::Branch(b) => Span::styled(b.clone(), Style::new().fg(PICKED)),
        HeadInfo::Detached(oid) => Span::styled(
            format!("{} (detached)", short_oid(*oid)),
            Style::new().fg(CURRENT),
        ),
        HeadInfo::Unborn(_) => Span::styled("no commits yet", Style::new().fg(MUTED)),
    }
}

fn status_span(dirty: bool) -> Span<'static> {
    if dirty {
        Span::styled("● dirty", Style::new().fg(WARNING))
    } else {
        Span::styled("clean", Style::new().fg(PICKED))
    }
}

fn dirty_span(dirty: bool) -> Span<'static> {
    if dirty {
        Span::styled(" ●", Style::new().fg(WARNING))
    } else {
        Span::raw("")
    }
}

fn head_short(head: &HeadInfo) -> String {
    match head {
        HeadInfo::Branch(b) => b.clone(),
        HeadInfo::Detached(oid) => short_oid(*oid),
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
    use crate::app::{App, ApplyOutcome, Focus, Hit, Popup, SaveChange, SaveStage};
    use crate::fixture::{self, SubSpec, TempDir};
    use crate::gitx;
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
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
        (App::normal(ov, sup.repo), temp)
    }

    /// Like `make_normal_app`, plus a `feature` branch on the main repo and
    /// an `old` branch on subA (for branch-switcher tests).
    fn make_branchy_app() -> (App, TempDir) {
        let temp = TempDir::new("ui-branch").unwrap();
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
        let sub = gitx::discover(&temp.path().join("super/subA"))
            .unwrap()
            .unwrap();
        let a1 = sup.sub_oids["subA"][0];
        let c1 = sub.find_commit(a1).unwrap();
        sub.branch("old", &c1, false).unwrap();

        let ov = gitx::overview(&sup.repo).unwrap();
        (App::normal(ov, sup.repo), temp)
    }

    fn key(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)
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
        assert!(text.contains("History"), "commit pane title");
    }

    #[test]
    fn commit_rows_render_with_markers() {
        let temp = TempDir::new("ui-markers").unwrap();
        let sup = fixture::make_super(
            &temp.path().join("super"),
            &[SubSpec {
                name: "subX",
                msgs: &["x1", "x2"],
                recorded: Some(0), // recorded is the OLDER commit
                drop_workdir: false,
            }],
        )
        .unwrap();
        let ov = gitx::overview(&sup.repo).unwrap();
        let mut app = App::normal(ov, sup.repo);
        app.select_repo(1); // subX

        let buf = render(&mut app, 100, 30);
        let text = screen_text(&buf);

        assert!(text.contains("x1") && text.contains("x2"), "titles: {text}");
        assert!(text.contains('●'), "recorded marker on x1");
        assert!(text.contains('○'), "workdir marker on x2");
        assert!(
            text.contains("rec") && text.contains("≠"),
            "drift in summary"
        );
        assert!(text.contains("2023-11-14 23:13"), "x1 timestamp: {text}");
        assert!(text.contains("2023-11-15 00:13"), "x2 timestamp: {text}");
    }

    #[test]
    fn pick_marker_and_summary_render() {
        let (mut app, _t) = make_normal_app();
        app.select_repo(1); // subA
        app.focus = Focus::Commits;
        app.handle_key(key(KeyCode::Down)); // older commit (a1)
        app.handle_key(key(KeyCode::Enter)); // pick it

        let buf = render(&mut app, 100, 30);
        let text = screen_text(&buf);
        assert!(text.contains('✓'), "pick marker: {text}");
        assert!(text.contains("pick"), "pick summary line");
        assert!(text.contains("a1"), "picked title in summary");
    }

    #[test]
    fn single_pane_without_submodules() {
        let temp = TempDir::new("ui-single").unwrap();
        let (repo, _) = fixture::make_history(&temp.path().join("r"), &["c1"]).unwrap();
        let ov = gitx::overview(&repo).unwrap();
        let mut app = App::normal(ov, repo);

        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);
        assert!(!text.contains("Repos"), "no repo pane: {text}");
        assert!(text.contains("History — r (1)"), "history title: {text}");
        assert!(text.contains("c1"));

        // Small terminals stay safe in single-pane mode too.
        render(&mut app, 12, 5);
        render(&mut app, 8, 3);
    }

    #[test]
    fn not_initialized_submodule_shows_hint() {
        let (mut app, _t) = make_normal_app();
        app.select_repo(2); // subC
        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);
        assert!(text.contains("not initialized"));
        assert!(
            text.contains("git submodule update --init subC"),
            "hint: {text}"
        );
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
        assert_eq!(app.focus, Focus::Repos);
    }

    #[test]
    fn clicking_rendered_commit_row_selects_it() {
        let (mut app, _t) = make_normal_app();
        render(&mut app, 80, 24);

        let (rect, _) = app
            .hit_zones
            .iter()
            .find(|(_, h)| *h == Hit::CommitRow(1))
            .copied()
            .unwrap();
        app.handle_mouse(click(rect.x + 2, rect.y));

        assert_eq!(app.focus, Focus::Commits);
        assert_eq!(app.entries[0].scroll.selected(), 1);
    }

    #[test]
    fn help_popup_renders_keys_and_dismisses_on_click() {
        let (mut app, _t) = make_normal_app();
        app.popup = Some(Popup::Help);
        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);

        assert!(text.contains("move selection"));
        assert!(text.contains("pick / un-pick"));
        assert!(text.contains("strictly local"));

        app.handle_mouse(click(40, 12));
        assert_eq!(app.popup, None);
    }

    #[test]
    fn branch_popup_renders_branches_current_and_hints() {
        let (mut app, _t) = make_branchy_app();
        app.handle_key(key(KeyCode::Char('b')));

        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);
        assert!(text.contains("Branches — super"), "title: {text}");
        assert!(text.contains("feature") && text.contains("main"));
        assert!(text.contains('●'), "current-branch marker");
        assert!(text.contains("Enter switch"), "hint line");
        assert!(!text.contains("does not change what the main repo records"));
    }

    #[test]
    fn branch_popup_on_submodule_shows_footnote() {
        let (mut app, _t) = make_branchy_app();
        app.select_repo(1); // subA
        app.handle_key(key(KeyCode::Char('b')));

        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);
        assert!(text.contains("Branches — subA"), "title: {text}");
        assert!(
            text.contains("does not change what the") && text.contains("main repo records"),
            "footnote (may wrap): {text}"
        );
    }

    #[test]
    fn clicking_rendered_branch_row_switches() {
        let (mut app, _t) = make_branchy_app();
        app.handle_key(key(KeyCode::Char('b')));
        render(&mut app, 80, 24);

        // Row 0 is `feature`.
        let (rect, _) = app
            .hit_zones
            .iter()
            .find(|(_, h)| *h == Hit::BranchRow(0))
            .copied()
            .unwrap();
        app.handle_mouse(click(rect.x + 3, rect.y));

        assert!(app.branch_popup.is_none(), "popup closed after switch");
        assert_eq!(
            app.overview.as_ref().unwrap().head,
            crate::gitx::HeadInfo::Branch("feature".into())
        );
    }

    #[test]
    fn save_confirm_renders_changes_warning_and_buttons() {
        let (mut app, temp) = make_normal_app();
        // Pick subA's older commit, then dirty subA to get a warning.
        app.select_repo(1);
        app.focus = Focus::Commits;
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        std::fs::write(temp.path().join("super/subA/untracked.txt"), "x").unwrap();
        app.handle_key(key(KeyCode::Char('s')));

        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);
        assert!(text.contains("Save — apply 1 change?"), "title: {text}");
        assert!(text.contains("subA"));
        assert!(text.contains('→'), "old → new: {text}");
        assert!(text.contains("a1"), "picked title: {text}");
        assert!(text.contains('⚠'), "dirty warning: {text}");
        assert!(text.contains("[ Apply ]"));
        assert!(text.contains("[ Cancel ]"));
        assert!(text.contains("nothing is committed or pushed"));

        // Both buttons registered as hit zones.
        assert!(app.hit_zones.iter().any(|(_, h)| *h == Hit::SaveApply));
        assert!(app.hit_zones.iter().any(|(_, h)| *h == Hit::SaveCancel));
    }

    #[test]
    fn clicking_apply_button_applies_and_shows_result() {
        let (mut app, _t) = make_normal_app();
        app.select_repo(1);
        app.focus = Focus::Commits;
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        let pick = app.entries[1].pick.unwrap();
        app.handle_key(key(KeyCode::Char('s')));
        render(&mut app, 80, 24);

        let (rect, _) = app
            .hit_zones
            .iter()
            .find(|(_, h)| *h == Hit::SaveApply)
            .copied()
            .unwrap();
        app.handle_mouse(click(rect.x + 2, rect.y));

        match &app.save_popup {
            Some(SaveStage::Result(outcome)) => {
                assert_eq!(outcome.applied.len(), 1);
            }
            other => panic!("expected Result, got {other:?}"),
        }
        assert_eq!(
            app.overview.as_ref().unwrap().submodules[0].index_oid,
            Some(pick),
            "pointer staged"
        );

        // Result renders ✓ and an OK button; clicking OK closes.
        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);
        assert!(text.contains("Save result"));
        assert!(text.contains('✓'));
        assert!(text.contains("[ OK ]"));
        let (ok, _) = app
            .hit_zones
            .iter()
            .find(|(_, h)| *h == Hit::SaveOk)
            .copied()
            .unwrap();
        app.handle_mouse(click(ok.x + 2, ok.y));
        assert!(app.save_popup.is_none());
    }

    #[test]
    fn save_result_renders_failure_with_error() {
        let (mut app, _t) = make_normal_app();
        app.save_popup = Some(SaveStage::Result(ApplyOutcome {
            applied: vec!["subA: 1111111 → 2222222".to_string()],
            failed: vec![("subC".to_string(), "cannot update: dirty".to_string())],
        }));
        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);
        assert!(text.contains('✓'));
        assert!(text.contains('✗'));
        assert!(text.contains("subC"));
        assert!(text.contains("cannot update: dirty"));
    }

    #[test]
    fn ellipsis_respects_cell_width_and_graphemes() {
        assert_eq!(ellipsize("abcdefgh", 5), "abcd…");
        assert_eq!(ellipsize("abcdefgh", 1), "…");
        assert_eq!(ellipsize("abcdefgh", 0), "");
        assert_eq!(ellipsize("東京大阪", 5), "東京…");
        assert_eq!(ellipsize("e\u{301}clair", 2), "e\u{301}…");
        assert!(UnicodeWidthStr::width(ellipsize("東京大阪", 5).as_str()) <= 5);
    }

    #[test]
    fn long_paths_and_titles_show_ellipsis_without_hiding_state() {
        let temp = TempDir::new("ui-long-title").unwrap();
        let title = "a very long commit title that runs beyond the terminal width";
        let (repo, _) = fixture::make_history(&temp.path().join("repo"), &[title]).unwrap();
        let ov = gitx::overview(&repo).unwrap();
        let mut app = App::normal(ov, repo);
        app.overview.as_mut().unwrap().root =
            std::path::PathBuf::from(format!("/{}", "very-long-directory-name/".repeat(7)));

        let buf = render(&mut app, 50, 15);
        let text = screen_text(&buf);
        let header = text.lines().next().unwrap();
        let commit = text.lines().nth(4).unwrap();
        assert!(header.contains('…') && header.contains("main"), "{header}");
        assert!(commit.contains('…'), "{commit}");
        assert!(!commit.contains(title), "{commit}");
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
        // The branch popup must tolerate tiny screens too.
        let (mut bapp, _t) = make_branchy_app();
        bapp.handle_key(key(KeyCode::Char('b')));
        for (w, h) in [(0, 0), (2, 2), (10, 4), (20, 6), (80, 24)] {
            render(&mut bapp, w, h);
        }
        // So must the save popups (confirm and result).
        let (mut sapp, _t) = make_branchy_app();
        sapp.save_popup = Some(SaveStage::Confirm(vec![SaveChange {
            idx: 1,
            name: "subA".to_string(),
            current: None,
            pick: git2::Oid::zero(),
            title: Some("t".to_string()),
            dirty_warning: Some("w".to_string()),
        }]));
        for (w, h) in [(0, 0), (2, 2), (10, 4), (19, 6), (80, 24)] {
            render(&mut sapp, w, h);
        }
        sapp.save_popup = Some(SaveStage::Result(ApplyOutcome {
            applied: vec!["a: 1 → 2".to_string()],
            failed: vec![("b".to_string(), "err".to_string())],
        }));
        for (w, h) in [(0, 0), (2, 2), (10, 4), (19, 6), (80, 24)] {
            render(&mut sapp, w, h);
        }
    }
}
