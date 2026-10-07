//! Rendering: turn [`App`] state into widgets.
//!
//! The renderer registers control regions (`Hit` zones) for layout tests
//! every frame without duplicating
//! layout math.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::{self, App, Entry, Focus, Hit, Mode, Popup, SaveStage, View};
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
/// OK button (Enter) that exits.
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
        Paragraph::new("Enter to exit")
            .dim()
            .alignment(Alignment::Center),
        rows[2],
    );
}

/// The main screen. Two panes when the repository has submodules (repo list
/// + commit browser); a single full-width commit browser otherwise.
fn draw_normal(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let footer = footer_lines(app, area.width as usize);
    // Keep room for a summary and at least one commit on compact terminals.
    let footer_height = (footer.len() as u16).min(area.height.saturating_sub(7));
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(footer_height),
    ])
    .split(area);
    draw_header(f, app, rows[0]);
    draw_footer(f, app, rows[3]);
    let status = app.shortcut_hint().unwrap_or_else(|| {
        if let Some(browser) = &app.global_history {
            return browser.notice.clone();
        }
        app.form
            .as_ref()
            .map(|e| e.notice.clone())
            .unwrap_or_else(|| app.status.clone())
    });
    f.render_widget(
        Paragraph::new(ellipsize(&safe_text(&status), rows[2].width as usize)).fg(CURRENT),
        rows[2],
    );

    if app.view == View::Configurations {
        draw_configs(f, app, rows[1]);
    } else if app.has_submodules() {
        let panes = Layout::horizontal([Constraint::Max(34), Constraint::Min(24)]).split(rows[1]);
        draw_repos_pane(f, app, panes[0]);
        draw_commits_pane(f, app, panes[1]);
    } else {
        draw_commits_pane(f, app, rows[1]);
    }

    if app.popup == Some(Popup::Help) {
        draw_help(f, app, area);
    }
    if app.popup == Some(Popup::Network) {
        draw_network_report(f, app, area);
    }
    if app.popup == Some(Popup::PullConfirm) {
        draw_pull_confirmation(f, app, area);
    }
    if app.branch_popup.is_some() {
        draw_branch_popup(f, app);
    }
    if app.save_popup.is_some() {
        draw_save_popup(f, app);
    }
    if app.global_history.is_some() {
        draw_global_history(f, app, rows[1]);
    }
    if app.batch_review.is_some() {
        draw_batch_review(f, app, rows[1]);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let Some(ov) = &app.overview else {
        return;
    };
    let head_text = format!("· {}", ov.head.describe());
    let title = format!(
        " taid · {}",
        if app.view == View::Repositories {
            "Repositories"
        } else {
            "Configurations"
        }
    );
    let dirty_text = if ov.dirty { "  ● dirty" } else { "" };
    let path_width = (area.width as usize).saturating_sub(
        UnicodeWidthStr::width(title.as_str())
            + UnicodeWidthStr::width(head_text.as_str())
            + UnicodeWidthStr::width(dirty_text)
            + 2,
    );
    let mut spans = vec![
        Span::styled(title, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
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
    f.render_widget(
        Paragraph::new(footer_lines(app, area.width as usize).join("\n")).dim(),
        area,
    );
}

fn footer_lines(app: &App, width: usize) -> Vec<String> {
    if app.action_note_editing {
        return wrap_footer(&["Note: type/paste · Backspace remove · Ctrl+U clear · Enter finish · Esc cancel · Ctrl+Q quit".into()], width);
    }
    if let Some(review) = &app.batch_review {
        return wrap_footer(
            &[
                if review.note_editing {
                    "Restore note: type/paste · Backspace · Ctrl+U clear · Enter finish · Esc cancel"
                } else if review.outcome.is_some() {
                    "Enter/Esc/q close result · snapshots retained in local history"
                } else {
                    "n revise note · Enter confirm ALL displayed files · Esc/q cancel"
                }
                .into(),
                "↑↓/kj · PgUp/PgDn · Home/End scroll review · Ctrl+Q quit".into(),
            ],
            width,
        );
    }
    if let Some(browser) = &app.global_history {
        let groups = if browser.description_input.is_some() {
            vec!["Append session note: type/paste · Enter save · Esc cancel · Backspace · Ctrl+U clear".into()]
        } else if browser.filtering {
            vec![format!(
                "Find: /{}▌ · Enter keep · Esc clear · Backspace remove · Ctrl+U clear",
                browser.filter
            )]
        } else {
            vec![
                "Enter expand · Backspace up · ↑↓/kj/PgUp/PgDn/Home/End · Tab/Shift+Tab list/changes/notes".into(),
                "/ find · f file/all · n append note · R reload · w warnings · u recovery".into(),
                "c current diff · b before/after · r WHOLE row restore · h/Esc/q close".into(),
                "Ctrl+Q quit · Ctrl+R,R repos · Ctrl+C,C configs · Ctrl+P,P pull all".into(),
            ]
        };
        return wrap_footer(&groups, width);
    }
    let global = if app.network_job.is_some() {
        "Ctrl+X cancel remote operation · Views: Ctrl+R,R repositories · Ctrl+C,C configs"
    } else {
        "Views: Ctrl+R,R repositories · Ctrl+C,C configs · Ctrl+P,P pull all"
    };
    let groups: Vec<String> = if app.popup == Some(Popup::PullConfirm) {
        vec![
            format!(
                "Enter/y pull {} · n revise note · Esc/q cancel · Ctrl+Q quit",
                if app.pull_target.is_some() {
                    "selected"
                } else {
                    "all"
                }
            ),
            global.into(),
        ]
    } else if app.save_popup.is_some() {
        vec![
            if matches!(app.save_popup, Some(SaveStage::Confirm(_))) {
                "n revise note · Enter apply · Esc/q cancel"
            } else {
                "Enter/Esc/q close results"
            }
            .into(),
            "Ctrl+Q quit".into(),
            global.into(),
        ]
    } else if app.branch_popup.is_some() {
        vec![
            "↑↓/kj select · PgUp/PgDn page · Home/End ends · f fetch branches · p pull repo · Enter switch & note · Esc/q close"
                .into(),
            "Ctrl+Q quit".into(),
            global.into(),
        ]
    } else if app.popup.is_some() {
        vec![
            "↑↓/kj scroll · Enter/Esc/q close · Ctrl+Q quit".into(),
            global.into(),
        ]
    } else if let Some(form) = &app.form {
        if let Some(overlay) = &form.overlay {
            let text = match overlay {
                crate::config::Overlay::Save {
                    note_editing: true, ..
                } => "Note: type/paste · Ctrl+U clear · Enter finish · Esc cancel note",
                crate::config::Overlay::Save { .. } => {
                    "n revise note · Enter confirm save · Esc/q return to edits · ↑↓/PgUp/PgDn scroll"
                }
                crate::config::Overlay::History { .. } => {
                    "History: ↑↓/PgUp/PgDn/Home/End · Tab entries/diff · c compare · b before/after · r load restore · Esc/q close"
                }
            };
            return wrap_footer(&[text.into(), "Ctrl+Q quit".into()], width);
        }
        let mut lines = if form.editing.is_some() {
            vec![
                "Value: type/paste replaces · ←→/Home/End cursor · Ctrl+U clear".into(),
                "Enter accept value · Esc cancel value · Backspace/Delete remove".into(),
            ]
        } else if form.filtering {
            vec![format!(
                "Find: /{}▌ · Enter keep · Esc clear · Ctrl+U clear",
                form.filter
            )]
        } else {
            vec![
                "↑↓/kj choose · PgUp/PgDn page · Home/End ends · / find parameter".into(),
                "Enter/Space edit or toggle · r reset value".into(),
            ]
        };
        lines.push(
            "Ctrl+S save preview · h global history · Esc close · Ctrl+D discard & close · Ctrl+Q quit"
                .into(),
        );
        lines.push(global.into());
        lines
    } else if app.filter_editing {
        vec![
            format!(
                "/{}▌ · Enter keep · Esc clear · Ctrl+U clear · Ctrl+Q quit",
                app.filter
            ),
            global.into(),
        ]
    } else {
        vec![
            global.into(),
            "Move: ↑↓/kj · PgUp/PgDn · Home/End · Tab/Shift+Tab/←→ panes".into(),
            if app.view == View::Configurations {
                "Enter edit config · p pull module · h global history · r refresh · n remote results · ? help · q/Esc/Ctrl+Q quit"
                    .into()
            } else {
                "Enter pick · / filter · b branch · s save · p pull selected · r refresh".into()
            },
            if app.view == View::Repositories {
                "h global history · n remote results · ? help · q/Esc/Ctrl+Q quit".into()
            } else {
                "".into()
            },
        ]
    };
    wrap_footer(&groups, width)
}

fn wrap_footer(groups: &[String], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for group in groups.iter().filter(|s| !s.is_empty()) {
        let mut line = String::new();
        for word in group.split_whitespace() {
            if !line.is_empty()
                && UnicodeWidthStr::width(line.as_str()) + UnicodeWidthStr::width(word) + 1 > width
            {
                lines.push(line);
                line = String::new();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines
}

fn draw_configs(f: &mut Frame, app: &mut App, area: Rect) {
    if app.form.is_some() {
        draw_form(f, app, area);
        return;
    }
    let panes =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).split(area);
    let block = Block::bordered()
        .title(" Modules with config ")
        .border_style(pane_border(app.focus == Focus::Repos));
    let inner = block.inner(panes[0]);
    app.pane_heights.repos = inner.height as usize;
    app.config_repos.ensure_visible(inner.height as usize);
    f.render_widget(block, panes[0]);
    let (offset, count) = app.config_repos.visible(inner.height as usize);
    for row in 0..count {
        let idx = offset + row;
        let module = &app.config_modules[idx];
        let badge = app
            .entries
            .get(module.entry_idx)
            .and_then(|e| e.updates.as_ref())
            .map(|u| u.row_badge())
            .unwrap_or_default();
        let name_width = (inner.width as usize).saturating_sub(
            UnicodeWidthStr::width(badge.as_str()) + usize::from(!badge.is_empty()),
        );
        let mut spans = vec![Span::raw(ellipsize(&module.name, name_width))];
        if !badge.is_empty() {
            spans.push(Span::styled(format!(" {badge}"), Style::new().fg(CURRENT)));
        }
        let rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        let style = if idx == app.config_repos.selected() {
            selected_style(app.focus == Focus::Repos)
        } else {
            Style::new()
        };
        f.render_widget(Paragraph::new(Line::from(spans).style(style)), rect);
        app.hit_zones.push((rect, Hit::ConfigModule(idx)));
    }
    let block = Block::bordered()
        .title(" Config files · Enter to edit ")
        .border_style(pane_border(app.focus == Focus::Commits));
    let inner = block.inner(panes[1]);
    app.pane_heights.commits = inner.height as usize;
    app.config_files.ensure_visible(inner.height as usize);
    f.render_widget(block, panes[1]);
    if let Some(module) = app.config_modules.get(app.config_repos.selected()) {
        let (offset, count) = app.config_files.visible(inner.height as usize);
        for row in 0..count {
            let idx = offset + row;
            let label = module.files[idx]
                .strip_prefix(&module.root)
                .unwrap_or(&module.files[idx])
                .display()
                .to_string();
            let rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
            let style = if idx == app.config_files.selected() {
                selected_style(app.focus == Focus::Commits)
            } else {
                Style::new()
            };
            f.render_widget(
                Paragraph::new(ellipsize(&label, inner.width as usize)).style(style),
                rect,
            );
            app.hit_zones.push((rect, Hit::ConfigFile(idx)));
        }
    } else {
        f.render_widget(
            Paragraph::new("No initialized submodules contain config files.")
                .dim()
                .wrap(Wrap { trim: false }),
            inner,
        );
    }
}

fn draw_form(f: &mut Frame, app: &mut App, area: Rect) {
    let form = app.form.as_mut().unwrap();
    let relative = form.path.strip_prefix(&app.context).unwrap_or(&form.path);
    let title = format!(
        " Parameters · {}{} ",
        relative.display(),
        if form.dirty() { " *" } else { "" }
    );
    let block = Block::bordered()
        .title(ellipsize(&title, area.width.saturating_sub(2) as usize))
        .border_style(pane_border(true));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width < 8 {
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .split(inner);
    let group = form.selected().map(|p| p.group.as_str()).unwrap_or("");
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "{} parameters · {} unsaved · existing values only",
                form.parameters.len(),
                form.changed_count()
            ))
            .fg(ACCENT),
            Line::from(ellipsize(group, rows[0].width as usize)).dim(),
        ]),
        rows[0],
    );
    let list = rows[1];
    let name_width = (list.width as usize * 55 / 100).saturating_sub(2);
    let type_width = 10.min((list.width as usize).saturating_sub(name_width + 2));
    let value_width = (list.width as usize).saturating_sub(name_width + type_width + 2);
    let header = format!("  {:<name_width$}{:<type_width$}Value", "Parameter", "Type");
    f.render_widget(
        Paragraph::new(header).dim(),
        Rect::new(list.x, list.y, list.width, list.height.min(1)),
    );
    let height = list.height.saturating_sub(1) as usize;
    app.pane_heights.commits = height;
    form.scroll.ensure_visible(height);
    let (offset, count) = form.scroll.visible(height);
    for row in 0..count {
        let idx = offset + row;
        let p = &form.parameters[form.visible[idx]];
        let name = p
            .name
            .split_once(".ros__parameters.")
            .map(|(_, name)| name)
            .unwrap_or(&p.name);
        let name = ellipsize(name, name_width);
        let padding = name_width.saturating_sub(UnicodeWidthStr::width(name.as_str()));
        let value = if p.kind == crate::config::Kind::Boolean {
            format!(
                "[{}] {}",
                if p.value == "true" { "x" } else { " " },
                p.value
            )
        } else {
            p.value.replace('\n', "\\n")
        };
        let mut spans = vec![
            Span::styled(
                if p.changed() { "* " } else { "  " },
                Style::new().fg(PICKED),
            ),
            Span::raw(format!("{name}{}", " ".repeat(padding))),
            Span::styled(
                format!("{:<type_width$}", ellipsize(p.kind.label(), type_width)),
                Style::new().fg(MUTED),
            ),
            Span::styled(
                ellipsize(&value, value_width),
                Style::new().fg(if p.readonly.is_some() {
                    MUTED
                } else if p.changed() {
                    PICKED
                } else {
                    CURRENT
                }),
            ),
        ];
        if idx == form.scroll.selected() {
            for span in &mut spans {
                span.style = span.style.patch(selected_style(true));
            }
        }
        let rect = Rect::new(list.x, list.y + row as u16 + 1, list.width, 1);
        f.render_widget(Paragraph::new(Line::from(spans)), rect);
        app.hit_zones.push((rect, Hit::ParameterRow(idx)));
    }
    if let Some(p) = form.selected() {
        let help = p.readonly.as_deref().unwrap_or(&p.description);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(ellipsize(&p.name, rows[2].width as usize)).fg(ACCENT),
                Line::from(help.to_owned()).dim(),
            ])
            .wrap(Wrap { trim: false }),
            rows[2],
        );
    } else {
        f.render_widget(
            Paragraph::new("No matching parameters. Press / to change the search.").dim(),
            rows[2],
        );
    }
    let mut x = rows[3].x;
    for (label, hit) in [
        ("[ Save · Ctrl+S ]", Hit::ParameterSave),
        ("[ Back · Esc ]", Hit::ParameterBack),
        ("[ History · h ]", Hit::ParameterHistory),
    ] {
        let width = (UnicodeWidthStr::width(label) as u16).min(rows[3].right().saturating_sub(x));
        if width > 0 && rows[3].height > 0 {
            let rect = Rect::new(x, rows[3].y, width, 1);
            f.render_widget(Paragraph::new(label).style(selected_style(true)), rect);
            app.hit_zones.push((rect, hit));
        }
        x = x.saturating_add(width + 2);
    }
    if let Some(input) = &form.editing {
        let p = form.selected().unwrap();
        let popup = centered_fixed(72, 10, area);
        f.render_widget(Clear, popup);
        let block = Block::bordered()
            .title(format!(" {} · {} ", p.label, p.kind.label()))
            .border_style(pane_border(true));
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);
        f.render_widget(
            Paragraph::new(ellipsize(&p.group, rows[0].width as usize)).dim(),
            rows[0],
        );
        f.render_widget(
            Paragraph::new(p.description.clone()).wrap(Wrap { trim: false }),
            rows[1],
        );
        let cursor_width = |start: usize| {
            UnicodeWidthStr::width(
                input
                    .text
                    .chars()
                    .skip(start)
                    .take(input.cursor - start)
                    .collect::<String>()
                    .as_str(),
            )
        };
        let mut start = 0;
        while start < input.cursor && cursor_width(start) >= rows[2].width.max(1) as usize {
            start += 1;
        }
        let value = input.text.chars().skip(start).collect::<String>();
        let style = if input.replace {
            selected_style(true)
        } else {
            Style::new().bg(Color::DarkGray)
        };
        f.render_widget(
            Paragraph::new(take_cells(&value, rows[2].width as usize)).style(style),
            rows[2],
        );
        if rows[2].width > 0 && rows[2].height > 0 {
            f.set_cursor_position((rows[2].x + cursor_width(start) as u16, rows[2].y));
        }
        f.render_widget(
            Paragraph::new(form.notice.clone())
                .fg(CURRENT)
                .wrap(Wrap { trim: false }),
            rows[3],
        );
    }
    if form.overlay.is_some() {
        draw_config_overlay(f, app, area);
    }
}

fn change_lines(changes: &[crate::history::Change], offset: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for change in changes.iter().skip(offset) {
        lines.push(Line::from(safe_text(&change.name)).fg(ACCENT));
        lines.push(Line::from(format!("  − {}", safe_text(&change.before))).fg(WARNING));
        lines.push(Line::from(format!("  + {}", safe_text(&change.after))).fg(PICKED));
    }
    if changes.is_empty() {
        lines.push(Line::from("No parameter differences."));
    }
    lines
}

fn safe_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn draw_global_history(f: &mut Frame, app: &mut App, area: Rect) {
    app.hit_zones.clear();
    let popup = centered_rect(98, 98, area);
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Global session history · opened checkout · never pushed ")
        .border_style(pane_border(true));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let browser = app.global_history.as_mut().unwrap();
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(browser.visible.len().clamp(1, 5) as u16),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    f.render_widget(
        Paragraph::new(format!(
            "{} / {} rows · find: {} · file: {} · {} focus · {}",
            browser.visible.len(),
            browser.rows.len(),
            safe_text(&browser.filter),
            safe_text(browser.file_filter.as_deref().unwrap_or("all")),
            if browser.notes_focus {
                "notes"
            } else if browser.diff_focus {
                "diff"
            } else {
                "entries"
            },
            browser.location()
        ))
        .fg(ACCENT),
        rows[0],
    );
    browser.scroll.ensure_visible(rows[1].height as usize);
    let (offset, count) = browser.scroll.visible(rows[1].height as usize);
    for row in 0..count {
        let idx = offset + row;
        let text = safe_text(&browser.row_text(browser.visible[idx]));
        let rect = Rect::new(rows[1].x, rows[1].y + row as u16, rows[1].width, 1);
        f.render_widget(
            Paragraph::new(ellipsize(&text, rect.width as usize)).style(
                if idx == browser.scroll.selected() {
                    selected_style(!browser.diff_focus && !browser.notes_focus)
                } else {
                    Style::default()
                },
            ),
            rect,
        );
        app.hit_zones.push((rect, Hit::HistoryRow(idx)));
    }
    if browser.visible.is_empty() {
        f.render_widget(
            Paragraph::new(
                "No matching history. All modules use the checkout's central journal; u opens unfinished restores.",
            ),
            rows[1],
        );
    }
    let title = if browser.compare_current {
        "Current → selected"
    } else {
        if matches!(
            browser.selected_row(),
            Some(crate::history::HistoryRow::Save(_))
        ) {
            "Selected save: previous → saved"
        } else {
            "Session/action/file: first before → last after"
        }
    };
    let target = if browser.before { "before" } else { "after" };
    let context = if matches!(
        browser.selected_row(),
        Some(crate::history::HistoryRow::Save(_) | crate::history::HistoryRow::File(_))
    ) {
        browser
            .selected()
            .map(|e| format!("{} · {} · {} · {}", e.module, e.path, e.branch, e.commit))
            .unwrap_or_default()
    } else {
        browser.selected_label()
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "{title} · {target} snapshot · {} warning(s)",
                browser.warnings.len()
            ))
            .fg(ACCENT),
            Line::from(safe_text(&context)).dim(),
        ]),
        rows[2],
    );
    let panes = if rows[3].width >= 60 {
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).split(rows[3])
    } else {
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[3])
    };
    let changes_block = Block::bordered()
        .title(" Granular changes ")
        .border_style(pane_border(browser.diff_focus));
    let changes_area = changes_block.inner(panes[0]);
    f.render_widget(changes_block, panes[0]);
    let notes_block = Block::bordered()
        .title(" Session notes · newest first ")
        .border_style(pane_border(
            browser.notes_focus || browser.description_input.is_some(),
        ));
    let notes_area = notes_block.inner(panes[1]);
    f.render_widget(notes_block, panes[1]);
    app.hit_zones.push((panes[0], Hit::HistoryChanges));
    app.hit_zones.push((panes[1], Hit::HistoryNotes));
    app.pane_heights.commits = changes_area.height as usize;
    let mut notes = Vec::new();
    if let Some(input) = &browser.description_input {
        notes.extend(wrapped_note(
            "New session note · Enter saves",
            &format!("{input}▌"),
            notes_area.width,
        ));
        browser.notes_scroll = notes
            .len()
            .saturating_sub(notes_area.height.max(1) as usize);
    }
    for (title, note) in browser.notes() {
        notes.extend(wrapped_note(&title, &note, notes_area.width));
    }
    if notes.is_empty() {
        notes.extend(wrapped_note(
            "No notes yet",
            "Add a note with n when saving a change or while viewing this session.",
            notes_area.width,
        ));
    }
    browser.notes_scroll = browser.notes_scroll.min(
        notes
            .len()
            .saturating_sub(notes_area.height.max(1) as usize),
    );
    f.render_widget(
        Paragraph::new(
            notes
                .into_iter()
                .skip(browser.notes_scroll)
                .collect::<Vec<_>>(),
        ),
        notes_area,
    );
    if browser.show_warnings {
        let mut warnings = browser.warnings.clone();
        if let Some(crate::history::HistoryRow::Recovery(i)) = browser.selected_row() {
            warnings.extend(browser.recoveries[*i].details.clone());
        }
        browser.diff_scroll = browser.diff_scroll.min(warnings.len().saturating_sub(1));
        let lines: Vec<_> = warnings
            .iter()
            .skip(browser.diff_scroll)
            .map(|w| Line::from(safe_text(w)).fg(WARNING))
            .collect();
        f.render_widget(
            Paragraph::new(if lines.is_empty() {
                vec![Line::from("No warnings.")]
            } else {
                lines
            })
            .wrap(Wrap { trim: false }),
            changes_area,
        );
    } else {
        match browser.display_comparison() {
            Ok(changes) => {
                browser.diff_scroll = browser.diff_scroll.min(changes.len().saturating_sub(1));
                f.render_widget(
                    Paragraph::new(change_lines(&changes, browser.diff_scroll))
                        .wrap(Wrap { trim: false }),
                    changes_area,
                );
            }
            Err(e) => f.render_widget(
                Paragraph::new(format!("Cannot compare: {e:#}"))
                    .fg(WARNING)
                    .wrap(Wrap { trim: false }),
                changes_area,
            ),
        }
    }
    f.render_widget(
        Paragraph::new(safe_text(&browser.notice)).fg(CURRENT),
        rows[4],
    );
    let buttons = [
        ("[ Compare c ]", Hit::HistoryCompare),
        ("[ Before/after b ]", Hit::HistoryVersion),
        ("[ Review r ]", Hit::HistoryRestore),
        ("[ Close ]", Hit::ConfigCancel),
    ];
    let columns =
        Layout::horizontal(std::iter::repeat_n(Constraint::Ratio(1, 4), 4)).split(rows[5]);
    for (i, (label, hit)) in buttons.into_iter().enumerate() {
        draw_button(f, app, columns[i], label, hit, ButtonPos::Center);
    }
}

/// Pre-wrap note lines so scrolling counts visible lines, including long tokens.
fn wrapped_note(title: &str, note: &str, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    for (text, heading) in [(title, true), (note, false)] {
        for text in wrap_footer(&[safe_text(text)], width) {
            let mut line = String::new();
            let mut cells = 0;
            for grapheme in text.graphemes(true) {
                let size = UnicodeWidthStr::width(grapheme);
                if cells + size > width && !line.is_empty() {
                    lines.push(if heading {
                        Line::from(line).fg(ACCENT)
                    } else {
                        Line::from(line)
                    });
                    line = String::new();
                    cells = 0;
                }
                line.push_str(grapheme);
                cells += size;
            }
            lines.push(if heading {
                Line::from(line).fg(ACCENT)
            } else {
                Line::from(line)
            });
        }
    }
    lines.push(Line::from(""));
    lines
}

fn draw_batch_review(f: &mut Frame, app: &mut App, area: Rect) {
    app.hit_zones.clear();
    let review_root = app.overview.as_ref().unwrap().root.clone();
    let popup = centered_rect(98, 98, area);
    f.render_widget(Clear, popup);
    let review = app.batch_review.as_mut().unwrap();
    let block = Block::bordered()
        .title(if review.outcome.is_some() {
            " Session restore result · recovery snapshots retained "
        } else {
            " Review session restore · no changes until confirmed "
        })
        .border_style(pane_border(true));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(inner);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "{} file(s) · {} repository state(s) · {} blocking issue(s) · {} warning(s)",
                review.files.len(),
                review
                    .repositories
                    .iter()
                    .filter(|p| !p.before.checkout_eq(&p.after))
                    .count(),
                review.errors.len(),
                review.warnings.len()
            ))
            .fg(ACCENT),
            Line::from(safe_text(&review.label)).dim(),
        ]),
        rows[0],
    );
    let mut lines = Vec::new();
    if let Some(outcome) = &review.outcome {
        lines.push(
            Line::from(safe_text(outcome)).fg(if outcome.starts_with("Restore stopped:") {
                WARNING
            } else {
                CURRENT
            }),
        );
    }
    for error in &review.errors {
        lines.push(Line::from(format!("BLOCKED: {}", safe_text(error))).fg(WARNING));
    }
    for warning in &review.warnings {
        lines.push(Line::from(format!("WARNING: {}", safe_text(warning))).fg(WARNING));
    }
    if review.files.is_empty()
        && review
            .repositories
            .iter()
            .all(|p| p.before.checkout_eq(&p.after))
        && review.errors.is_empty()
    {
        lines.push(Line::from(
            "All selected values are already on disk. No config save entries will be added.",
        ));
    }
    for repository in &review.repositories {
        lines.extend(change_lines(
            &crate::repository_history::display_changes(
                &review_root,
                &repository.before,
                &repository.after,
                false,
            ),
            0,
        ));
    }
    for file in &review.files {
        lines.push(
            Line::from(safe_text(&file.path))
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD),
        );
        lines.extend(change_lines(&file.changes, 0));
    }
    review.scroll = review.scroll.min(lines.len().saturating_sub(1));
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(review.scroll).collect::<Vec<_>>())
            .wrap(Wrap { trim: false }),
        rows[1],
    );
    f.render_widget(Paragraph::new(vec![
        Line::from(format!("Required note{}: {}{}", if review.note_editing { " (editing)" } else { " · n edits" }, safe_text(&review.note), if review.note_editing { "▌" } else { "" })),
        Line::from("Multi-target writes are not atomic. A partial failure stops further writes and retains a reviewed resume/rollback plan.").fg(WARNING),
    ]).wrap(Wrap { trim: false }),rows[2]);
    let completed = review.outcome.is_some();
    let blocked = !review.errors.is_empty();
    let columns =
        Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(rows[3]);
    if completed {
        draw_button(
            f,
            app,
            columns[0],
            "[ Close result · Enter ]",
            Hit::ConfigCancel,
            ButtonPos::Center,
        );
    } else if !blocked {
        draw_button(
            f,
            app,
            columns[0],
            "[ Confirm ALL · Enter ]",
            Hit::ConfigConfirm,
            ButtonPos::Center,
        );
    }
    draw_button(
        f,
        app,
        columns[1],
        if completed {
            "[ Close · Esc ]"
        } else {
            "[ Cancel · Esc ]"
        },
        Hit::ConfigCancel,
        ButtonPos::Center,
    );
}

fn draw_config_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    app.hit_zones.clear(); // Layout tests observe only the topmost modal's controls.
    let popup = centered_rect(96, 96, area);
    f.render_widget(Clear, popup);
    let form = app.form.as_mut().unwrap();
    let title = if matches!(form.overlay, Some(crate::config::Overlay::Save { .. })) {
        " Review config save "
    } else {
        " Local config history · never pushed "
    };
    let block = Block::bordered()
        .title(title)
        .border_style(pane_border(true));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    // Take the overlay so comparison helpers can read the form without conflicting borrows.
    let comparison = form.history_comparison();
    let mut overlay = form.overlay.take().unwrap();
    let mut buttons = Vec::new();
    let button_row;
    match &mut overlay {
        crate::config::Overlay::Save {
            changes,
            note,
            note_editing,
            scroll,
        } => {
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(1),
            ])
            .split(inner);
            f.render_widget(
                Paragraph::new(format!(
                    "{} parameter change(s) · {}",
                    changes.len(),
                    form.path.display()
                ))
                .fg(ACCENT),
                rows[0],
            );
            *scroll = (*scroll).min(changes.len().saturating_sub(1));
            f.render_widget(
                Paragraph::new(change_lines(changes, *scroll)).wrap(Wrap { trim: false }),
                rows[1],
            );
            let text = format!(
                "Required note: {note}{}",
                if *note_editing { "▌" } else { "" }
            );
            f.render_widget(
                Paragraph::new(text)
                    .fg(if note.trim().is_empty() {
                        WARNING
                    } else if *note_editing {
                        ACCENT
                    } else {
                        MUTED
                    })
                    .wrap(Wrap { trim: false }),
                rows[2],
            );
            f.render_widget(
                Paragraph::new(form.notice.clone())
                    .fg(CURRENT)
                    .wrap(Wrap { trim: false }),
                rows[3],
            );
            button_row = rows[4];
            buttons.extend([
                ("[ Confirm save ]", Hit::ConfigConfirm),
                ("[ Back to edits ]", Hit::ConfigCancel),
            ]);
        }
        crate::config::Overlay::History {
            entries,
            scroll,
            record,
            compare_current,
            before,
            diff_scroll,
            diff_focus,
        } => {
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(5),
                Constraint::Length(2),
                Constraint::Min(0),
                Constraint::Length(2),
                Constraint::Length(1),
            ])
            .split(inner);
            f.render_widget(
                Paragraph::new(format!(
                    "{} entries · {} · {} focus",
                    entries.len(),
                    form.path.display(),
                    if *diff_focus { "diff" } else { "history" }
                ))
                .fg(ACCENT),
                rows[0],
            );
            scroll.ensure_visible(rows[1].height as usize);
            let (offset, count) = scroll.visible(rows[1].height as usize);
            for row in 0..count {
                let idx = offset + row;
                let entry = &entries[idx];
                let time = chrono::DateTime::from_timestamp(entry.timestamp, 0)
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| "unknown time".into());
                let text = format!(
                    "{time} · {} · {} changes · {} · {}",
                    entry.branch, entry.changes, entry.state, entry.note
                );
                let rect = Rect::new(rows[1].x, rows[1].y + row as u16, rows[1].width, 1);
                f.render_widget(
                    Paragraph::new(ellipsize(&text, rect.width as usize)).style(
                        if idx == scroll.selected() {
                            selected_style(!*diff_focus)
                        } else {
                            Style::default()
                        },
                    ),
                    rect,
                );
                app.hit_zones.push((rect, Hit::HistoryRow(idx)));
            }
            if entries.is_empty() {
                f.render_widget(
                    Paragraph::new(
                        "No saved changes yet. The first save records the baseline too.",
                    ),
                    rows[1],
                );
            }
            let heading = format!(
                "{} · target: {} snapshot",
                if *compare_current {
                    "Restore preview: current → selected"
                } else {
                    "Selected save: previous → saved"
                },
                if *before { "before" } else { "after" }
            );
            let context = record
                .as_ref()
                .map(|r| {
                    format!(
                        "{} · {} · {}{}",
                        r.branch,
                        r.commit,
                        r.note,
                        r.restored_from
                            .as_ref()
                            .map(|s| format!(" · restored from {s}"))
                            .unwrap_or_default()
                    )
                })
                .unwrap_or_default();
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(heading).fg(ACCENT),
                    Line::from(context).dim(),
                ])
                .wrap(Wrap { trim: false }),
                rows[2],
            );
            match comparison {
                Ok(changes) => {
                    *diff_scroll = (*diff_scroll).min(changes.len().saturating_sub(1));
                    f.render_widget(
                        Paragraph::new(change_lines(&changes, *diff_scroll))
                            .wrap(Wrap { trim: false }),
                        rows[3],
                    );
                }
                Err(e) => f.render_widget(
                    Paragraph::new(format!("Cannot compare: {e:#}"))
                        .fg(WARNING)
                        .wrap(Wrap { trim: false }),
                    rows[3],
                ),
            }
            f.render_widget(
                Paragraph::new(form.notice.clone())
                    .fg(CURRENT)
                    .wrap(Wrap { trim: false }),
                rows[4],
            );
            button_row = rows[5];
            buttons.extend([
                ("[ Compare · c ]", Hit::HistoryCompare),
                ("[ Before/after · b ]", Hit::HistoryVersion),
                ("[ Load restore · r ]", Hit::HistoryRestore),
                ("[ Close ]", Hit::ConfigCancel),
            ]);
        }
    }
    form.overlay = Some(overlay);
    let columns = Layout::horizontal(std::iter::repeat_n(
        Constraint::Ratio(1, buttons.len() as u32),
        buttons.len(),
    ))
    .split(button_row);
    for (i, (label, hit)) in buttons.into_iter().enumerate() {
        draw_button(f, app, columns[i], label, hit, ButtonPos::Center);
    }
}

fn draw_pull_confirmation(f: &mut Frame, app: &mut App, area: Rect) {
    let note_lines = action_note_lines(app, area.width.saturating_sub(4).min(64));
    let popup = centered_fixed(68, 14 + note_lines.len().saturating_sub(1) as u16, area);
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(if app.pull_target.is_some() {
            " Confirm pull selected repository "
        } else {
            " Confirm pull all repositories "
        })
        .border_style(pane_border(true));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    app.hit_zones.push((popup, Hit::PullPopup));
    let initialized = app
        .overview
        .as_ref()
        .map(|ov| {
            ov.submodules
                .iter()
                .filter(|sm| sm.workdir.is_some())
                .count()
        })
        .unwrap_or(0);
    let rows = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(note_lines.len() as u16),
        Constraint::Length(1),
    ])
    .split(inner);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(if let Some(target) = &app.pull_target {
                format!("Pull only {}?", safe_text(&target.name))
            } else {
                format!("Pull the main repository and {initialized} initialized submodules?")
            })
            .fg(ACCENT),
            Line::from(""),
            Line::from("This fetches remotes and fast-forwards each current branch."),
            Line::from("Dirty, detached or diverged repositories are not overwritten."),
            Line::from("No merges, commits or staged submodule pointers are created."),
            Line::from(""),
            Line::from(if app.pull_target.is_some() {
                "Other repositories will not be fetched or pulled."
            } else {
                "Results are reported separately for every repository."
            })
            .dim(),
        ])
        .wrap(Wrap { trim: false }),
        rows[0],
    );
    f.render_widget(Paragraph::new(note_lines).fg(CURRENT), rows[1]);
    let buttons =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[2]);
    draw_button(
        f,
        app,
        buttons[0],
        "[ Pull ]",
        Hit::PullConfirm,
        ButtonPos::Center,
    );
    draw_button(
        f,
        app,
        buttons[1],
        "[ Cancel ]",
        Hit::PullCancel,
        ButtonPos::Center,
    );
}

fn draw_network_report(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(90, 85, area);
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Remote results · ↑↓ scroll ")
        .border_style(pane_border(true));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let mut lines = Vec::new();
    for (name, result) in app.network_results.iter().skip(app.report_scroll) {
        let (message, color) = match result {
            Ok(text) => (text, PICKED),
            Err(text) => (text, WARNING),
        };
        lines.push(Line::from(name.clone()).fg(ACCENT));
        lines.push(Line::from(message.clone()).fg(color));
        lines.push(Line::from(""));
    }
    if lines.is_empty() {
        lines.push(Line::from("No remote results yet."));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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
                if let Some(badge) = branch_badge(app, idx) {
                    spans.push(badge);
                }
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
                    if let Some(head) = &sm.head {
                        spans.push(Span::styled(
                            ellipsize(&head_short(head), 8),
                            Style::new().fg(MUTED),
                        ));
                    }
                    if sm.recorded_oid != sm.workdir_oid {
                        spans.push(Span::styled(" ≠", Style::new().fg(ACCENT)));
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
                if let Some(badge) = branch_badge(app, idx) {
                    spans.push(badge);
                }
                let name_width =
                    (inner.width as usize).saturating_sub(Line::from(spans.clone()).width());
                spans.insert(
                    0,
                    Span::raw(ellipsize(
                        &root_name(std::path::Path::new(&sm.path)),
                        name_width,
                    )),
                );
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

    let mut first: Vec<Span<'static>> = match app::entry_at(ov, idx) {
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
    if let Some(badge) = branch_badge(app, idx) {
        first.push(badge);
    }
    let second = if app.entries.get(idx).is_some_and(|e| e.pick.is_some()) {
        pick_line(app, idx)
    } else if let Some(info) = app.entries.get(idx).and_then(|e| e.updates.as_ref()) {
        let text = if info.unknown || info.unborn {
            info.detail()
        } else if !info.has_upstream {
            format!(
                "No upstream · {} missing · {}{}",
                info.newer,
                info.branch,
                if info.detached { " · detached" } else { "" }
            )
        } else {
            format!(
                "+{} ahead / -{} behind · {} missing · {}{}",
                info.ahead,
                info.behind,
                info.newer,
                info.branch,
                if info.detached { " · detached" } else { "" }
            )
        };
        Line::from(text).fg(if info.ahead > 0 && info.behind > 0 {
            WARNING
        } else {
            CURRENT
        })
    } else {
        Line::from("")
    };
    vec![Line::from(first), second]
}

fn branch_badge(app: &App, idx: usize) -> Option<Span<'static>> {
    let info = app.entries.get(idx)?.updates.as_ref()?;
    let badge = info.row_badge();
    if badge.is_empty() {
        return None;
    }
    Some(Span::styled(
        format!(" {badge}"),
        Style::new().fg(if info.ahead > 0 && info.behind > 0 {
            WARNING
        } else {
            CURRENT
        }),
    ))
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
    let note_lines = if app.action_note.is_empty() && !app.action_note_editing {
        Vec::new()
    } else {
        action_note_lines(app, area.width.saturating_sub(4).min(54))
    };
    let mut h = 3 + 1 + list_h + 1 + note_lines.len() as u16;
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
    constraints.push(Constraint::Length(note_lines.len() as u16));
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
    let note_area = chunks[section];
    section += 1;
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
        if b.is_remote {
            spans.push(Span::styled(" [remote]", Style::new().fg(WARNING)));
        }
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
    f.render_widget(Paragraph::new(note_lines).fg(CURRENT), note_area);
    f.render_widget(
        Paragraph::new(" Enter switch · f fetch branches · Esc close ")
            .style(Style::new().fg(MUTED)),
        hint_area,
    );
}

/// The save popup (`s`): confirmation listing every planned change, then the
/// apply result. All controls are keyboard-only.
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
            let note_lines = action_note_lines(app, area.width.saturating_sub(4).min(56));
            constraints.push(Constraint::Length(note_lines.len() as u16));
            constraints.push(Constraint::Length(1)); // buttons

            let title = if n == 1 {
                " Save — apply 1 change? ".to_string()
            } else {
                format!(" Save — apply {n} changes? ")
            };
            let popup = centered_fixed(
                60,
                constraints.len() as u16 + 2 + note_lines.len().saturating_sub(1) as u16,
                area,
            );
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
            f.render_widget(Paragraph::new(note_lines).fg(CURRENT), rows[r]);
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

fn action_note_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    if app.action_note.is_empty() && !app.action_note_editing {
        return vec![
            Line::from("Note required · Enter opens input (1–500 characters)").fg(WARNING),
        ];
    }
    let text = format!(
        "Note: {}{}",
        app.action_note,
        if app.action_note_editing { "▌" } else { "" }
    );
    let mut lines = wrapped_note("", &text, width);
    lines.pop();
    lines
        .into_iter()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
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

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let popup = centered_rect(95, 95, area);
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Help · ↑↓ scroll ")
        .border_style(Style::new().fg(ACCENT));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let keys: &[(&str, &str)] = &[
        ("Ctrl+R,R / Ctrl+C,C", "repositories / configurations"),
        ("Ctrl+P,P", "confirm fetch and fast-forward all repos"),
        ("p", "pull only selected repository/module"),
        ("↑ ↓ / k j", "move selection"),
        ("PgUp PgDn / Home End", "scroll by page / first-last"),
        ("Tab / Shift+Tab", "switch pane (repos ↔ commits)"),
        ("Enter", "pick / un-pick recorded commit"),
        ("b", "local/remote branch picker (f fetches)"),
        ("s", "save: apply picked commits"),
        (
            "Notes open on save",
            "type; Enter finish, Enter confirm; Esc cancel; n revise",
        ),
        ("r", "refresh repository state"),
        ("n", "remote results (↑↓ to scroll)"),
        ("/", "filter commits by title or SHA"),
        ("?", "toggle this help"),
        ("q / Esc", "quit (or close a popup)"),
        ("Ctrl+Q", "quit (protects unsaved edits)"),
        ("YAML forms", "Enter value/toggle; / find; r reset"),
        (
            "Form save/close",
            "Ctrl+S review/confirm; Esc close; Ctrl+D discard",
        ),
        (
            "Global session history",
            "h opens; Enter expands session into actions/files; Backspace up",
        ),
        (
            "History browsing",
            "/ find; f file/all; n session note; R reload; w warnings; u recovery",
        ),
        (
            "History panes",
            "Tab/Shift+Tab entries/changes/notes; ↑↓ scroll",
        ),
        (
            "History restoring",
            "c current comparison; b before/after; r review selected scope",
        ),
        (
            "Session restore review",
            "Enter confirm all; n note; Esc cancel; ↑↓/Pg/Home/End scroll",
        ),
        ("Ctrl+X", "cancel fetch/remaining repos; finish checkout"),
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
        "  [new: N] commits missing from checkout; [ahead: N] local-only",
    ));
    lines.push(Line::from(
        "  [diverged] both sides differ; [detached] / [branch unknown]",
    ));
    lines.push(Line::from(
        "  [no upstream] / [unborn] cannot pull normally",
    ));
    lines.push(Line::from(
        "  Badges use stored refs; confirmed pulls refresh remotes.".dim(),
    ));
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.report_scroll as u16, 0)),
        inner,
    );
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
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::Path;

    fn render(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn repository_only_history_shows_branch_diff_and_safe_restore_review_at_all_sizes() {
        let (mut app, _temp) = make_branchy_app();
        app.handle_key(key(KeyCode::Char('b')));
        crate::fixture::confirm_action(&mut app);
        app.handle_key(key(KeyCode::Char('h')));
        let text = screen_text(&render(&mut app, 100, 32));
        assert!(text.contains("Global session history"));
        assert!(text.contains("branch / checkout commit"));
        assert!(text.contains("− main @") && text.contains("+ feature @"));
        assert!(app.global_history.as_ref().unwrap().entries.is_empty());
        crate::fixture::confirm_action(&mut app);
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Char('r')));
        let text = screen_text(&render(&mut app, 100, 32));
        assert!(text.contains("Review session restore"));
        assert!(text.contains("no changes until confirmed"));
        assert!(text.contains("1 repository state(s)"));
        assert!(text.contains("− feature @") && text.contains("+ main @"));
        for (w, h) in [(1, 1), (8, 3), (20, 10), (80, 24)] {
            render(&mut app, w, h);
        }
        assert_eq!(
            app.repo.as_ref().unwrap().head().unwrap().shorthand(),
            Some("feature")
        );
        app.handle_key(key(KeyCode::Esc));
        assert!(app.batch_review.is_none());
        assert_eq!(
            app.repo.as_ref().unwrap().head().unwrap().shorthand(),
            Some("feature")
        );
    }

    #[test]
    fn session_notes_have_a_separate_pane_and_scroll_without_moving_changes() {
        let (mut app, _temp) = make_branchy_app();
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_paste(&format!("{}TAILλ", "steering ".repeat(45)));
        crate::fixture::confirm_action(&mut app);
        crate::fixture::confirm_action(&mut app);
        app.handle_key(key(KeyCode::Char('h')));
        let text = screen_text(&render(&mut app, 100, 32));
        assert!(
            text.contains("Granular changes") && text.contains("Session notes"),
            "{text}"
        );
        assert!(text.contains("steering"), "{text}");
        assert!(text.contains("branch / checkout commit"), "{text}");
        let notes_rect = app
            .hit_zones
            .iter()
            .find(|(_, hit)| *hit == Hit::HistoryNotes)
            .unwrap()
            .0;
        let changes_rect = app
            .hit_zones
            .iter()
            .find(|(_, hit)| *hit == Hit::HistoryChanges)
            .unwrap()
            .0;
        assert!(changes_rect.x + changes_rect.width <= notes_rect.x);
        app.handle_mouse(click(notes_rect.x + 1, notes_rect.y + 1));
        assert!(!app.global_history.as_ref().unwrap().notes_focus);
        app.handle_key(key(KeyCode::BackTab));
        app.handle_key(key(KeyCode::End));
        let text = screen_text(&render(&mut app, 90, 26));
        assert!(
            text.contains("TAILλ"),
            "tail of wrapped note can be reached: {text}"
        );
        let browser = app.global_history.as_ref().unwrap();
        assert!(browser.notes_focus && browser.notes_scroll > 0);
        assert_eq!(browser.scroll.selected(), 0);
        assert_eq!(browser.diff_scroll, 0);
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.global_history.as_ref().unwrap().notes_scroll, 0);
        app.handle_key(key(KeyCode::BackTab));
        assert!(app.global_history.as_ref().unwrap().diff_focus);
        app.handle_key(key(KeyCode::Tab));
        assert!(app.global_history.as_ref().unwrap().notes_focus);
        for (w, h) in [(1, 1), (8, 3), (40, 12), (80, 24)] {
            render(&mut app, w, h);
        }
    }

    #[test]
    fn history_changes_and_restore_review_show_short_commit_hashes_and_titles() {
        let (mut app, _temp) = make_branchy_app();
        let commit = app
            .repo
            .as_ref()
            .unwrap()
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap();
        let full = commit.id().to_string();
        let description = format!("{} · {}", &full[..7], commit.summary().unwrap());
        drop(commit);
        app.handle_key(key(KeyCode::Char('b')));
        crate::fixture::confirm_action(&mut app);
        app.handle_key(key(KeyCode::Char('h')));
        let buffer = render(&mut app, 180, 40);
        let rect = app
            .hit_zones
            .iter()
            .find_map(|(rect, hit)| (*hit == Hit::HistoryChanges).then_some(*rect))
            .unwrap();
        let text = (rect.y..rect.bottom())
            .map(|y| {
                (rect.x..rect.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&description), "{text}");
        assert!(!text.contains(&full), "changes use abbreviated IDs: {text}");
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Char('r')));
        let text = screen_text(&render(&mut app, 180, 40));
        assert!(
            text.contains(&description),
            "restore preview uses the same display: {text}"
        );
        assert!(!text.contains(&full));
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
        assert!(text.contains("Enter to exit"));
        assert!(app.hit_zones.iter().any(|(_, h)| *h == Hit::OkButton));
    }

    #[test]
    fn mouse_ok_is_ignored_and_enter_quits() {
        let mut app = App::not_repo(Path::new("/x"));
        render(&mut app, 80, 24);
        let (rect, _) = app
            .hit_zones
            .iter()
            .find(|(_, h)| *h == Hit::OkButton)
            .copied()
            .unwrap();
        app.handle_mouse(click(rect.x + rect.width / 2, rect.y));
        assert!(!app.should_quit);
        app.handle_key(key(KeyCode::Enter));
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
        crate::fixture::confirm_action(&mut app); // pick it

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
        assert!(
            !app.hit_zones.iter().any(|(_, hit)| *hit == Hit::PaneRepos),
            "no repo pane: {text}"
        );
        assert!(text.contains("History — r (1)"), "history title: {text}");
        assert!(text.contains("c1"));

        // Small terminals stay safe in single-pane mode too.
        render(&mut app, 12, 5);
        render(&mut app, 8, 3);
    }

    #[test]
    fn config_view_keyboard_form_validation_and_wrapped_shortcuts_render() {
        let (mut app, temp) = make_normal_app();
        let path = temp.path().join("super/subA/controller/config/gains.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# controller\ngain: 1\n").unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('c')));
        let text = screen_text(&render(&mut app, 80, 24));
        assert!(text.contains("Configurations"));
        assert!(text.contains("controller/config/gains.yaml"));
        assert!(!text.contains("subC"));
        for shortcut in [
            "Ctrl+R,R",
            "Ctrl+C,C",
            "Ctrl+P,P",
            "PgUp/PgDn",
            "Home/End",
            "r refresh",
            "? help",
            "Ctrl+Q",
        ] {
            assert!(text.contains(shortcut), "missing {shortcut}: {text}");
        }
        let rect = app
            .hit_zones
            .iter()
            .find_map(|(rect, hit)| (*hit == Hit::ConfigFile(0)).then_some(*rect))
            .unwrap();
        app.handle_mouse(click(rect.x + 1, rect.y));
        assert!(app.form.is_none());
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.form.as_ref().unwrap().path,
            path.canonicalize().unwrap()
        );
        let text = screen_text(&render(&mut app, 80, 24));
        assert!(text.contains("gain") && text.contains("integer"));
        assert!(text.contains("Parameters"));
        assert!(!text.contains("gain: 1"), "the form never exposes raw YAML");
        assert!(text.contains("Ctrl+S save"));
        assert!(text.contains("Ctrl+D discard"));
        for (width, height) in [(1, 1), (10, 4), (20, 6), (50, 20)] {
            render(&mut app, width, height);
        }
        render(&mut app, 80, 24);
        let rect = app
            .hit_zones
            .iter()
            .find_map(|(rect, hit)| (*hit == Hit::ParameterRow(0)).then_some(*rect))
            .unwrap();
        app.handle_mouse(click(rect.x + 1, rect.y));
        app.handle_key(key(KeyCode::Enter));
        app.handle_paste("invalid number");
        crate::fixture::confirm_action(&mut app);
        assert!(app.form.as_ref().unwrap().editing.is_some());
        let text = screen_text(&render(&mut app, 80, 24));
        assert!(text.contains("whole number"), "{text}");
        app.handle_key(key(KeyCode::Esc));
        crate::fixture::confirm_action(&mut app);
        app.handle_key(key(KeyCode::Char('3')));
        render(&mut app, 80, 24);
        let save = app
            .hit_zones
            .iter()
            .find_map(|(rect, hit)| (*hit == Hit::ParameterSave).then_some(*rect))
            .unwrap();
        app.handle_mouse(click(save.x + 1, save.y));
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let text = screen_text(&render(&mut app, 80, 24));
        assert!(text.contains("Review config save"), "{text}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# controller\ngain: 1\n"
        );
        let confirm = app
            .hit_zones
            .iter()
            .find_map(|(rect, hit)| (*hit == Hit::ConfigConfirm).then_some(*rect))
            .unwrap();
        app.handle_mouse(click(confirm.x, confirm.y));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# controller\ngain: 1\n"
        );
        app.handle_paste("Config click test");
        app.handle_key(key(KeyCode::Enter)); // finish required note
        app.handle_mouse(click(confirm.x, confirm.y));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            std::fs::read_to_string(app.form.as_ref().unwrap().path.clone()).unwrap(),
            "# controller\ngain: 3\n"
        );
    }

    #[test]
    fn config_history_popup_compares_restores_and_tolerates_tiny_screens() {
        let (mut app, temp) = make_normal_app();
        let path = temp.path().join("super/subA/config/params.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "gain: 1\n").unwrap();
        app.view = View::Configurations;
        app.form = Some(crate::config::Form::open(path.clone()).unwrap());
        crate::fixture::confirm_action(&mut app);
        app.handle_paste("2");
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let text = screen_text(&render(&mut app, 100, 36));
        assert!(text.contains("Review config save"));
        assert!(text.contains("− 1") && text.contains("+ 2"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "gain: 1\n");
        for (w, h) in [(1, 1), (8, 3), (20, 10)] {
            render(&mut app, w, h);
        }
        crate::fixture::confirm_action(&mut app);
        app.handle_key(key(KeyCode::Char('h')));
        crate::fixture::confirm_action(&mut app); // session → files
        crate::fixture::confirm_action(&mut app); // file → individual saves
        let text = screen_text(&render(&mut app, 100, 36));
        assert!(text.contains("Global session history") && text.contains("never pushed"));
        assert!(text.contains("previous → saved"));
        assert!(text.contains("− 1") && text.contains("+ 2"));
        app.handle_key(key(KeyCode::Char('c')));
        app.handle_key(key(KeyCode::Char('b')));
        let text = screen_text(&render(&mut app, 100, 36));
        assert!(text.contains("Current → selected") && text.contains("before snapshot"));
        assert!(text.contains("− 2") && text.contains("+ 1"));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::End));
        render(&mut app, 100, 36);
        for (w, h) in [(1, 1), (8, 3), (20, 10)] {
            render(&mut app, w, h);
        }
        render(&mut app, 100, 36);
        let restore = app
            .hit_zones
            .iter()
            .find_map(|(rect, hit)| (*hit == Hit::HistoryRestore).then_some(*rect))
            .unwrap();
        app.handle_mouse(click(restore.x, restore.y));
        app.handle_key(key(KeyCode::Char('r')));
        assert!(app.form.as_ref().unwrap().dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "gain: 2\n");
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        crate::fixture::confirm_action(&mut app);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "gain: 1\n");
        assert_eq!(
            crate::history::Store::open_in(&path, &app.overview.as_ref().unwrap().root)
                .unwrap()
                .list()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn session_restore_review_is_modal_and_requires_keyboard_confirmation() {
        let (mut app, temp) = make_normal_app();
        let root = temp.path().join("super").canonicalize().unwrap();
        let paths = [
            root.join("config/main.yaml"),
            root.join("subA/config/gains.yaml"),
        ];
        for (path, value) in paths.iter().zip(["2", "4"]) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "gain: 1\n").unwrap();
            app.form = Some(crate::config::Form::open_in(path.clone(), &root).unwrap());
            crate::fixture::confirm_action(&mut app);
            app.handle_paste(value);
            app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
            crate::fixture::confirm_action(&mut app);
        }
        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Char('n')));
        app.handle_paste("Dry test");
        crate::fixture::confirm_action(&mut app);
        let text = screen_text(&render(&mut app, 100, 32));
        assert!(text.contains("Dry test"));
        assert!(text.contains("2 modules / 2 files / 2 saves"));
        app.handle_key(key(KeyCode::Char('b')));
        app.handle_key(key(KeyCode::Char('r')));
        let text = screen_text(&render(&mut app, 100, 32));
        assert!(text.contains("Review session restore"));
        assert!(text.contains("config/main.yaml") && text.contains("subA/config/gains.yaml"));
        assert!(text.contains("− 2") && text.contains("− 4") && text.contains("+ 1"));
        assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), "gain: 2\n");
        // Shortcuts cannot escape this confirmation or start pulling behind it.
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.network_job.is_none());
        assert!(app.batch_review.is_some());
        for (w, h) in [(1, 1), (8, 3), (20, 10), (80, 24)] {
            render(&mut app, w, h);
        }
        render(&mut app, 100, 32);
        let cancel = app
            .hit_zones
            .iter()
            .find_map(|(rect, hit)| (*hit == Hit::ConfigCancel).then_some(*rect))
            .unwrap();
        app.handle_mouse(click(cancel.x, cancel.y));
        assert!(app.batch_review.is_some());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.batch_review.is_none());
        assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), "gain: 2\n");
        app.handle_key(key(KeyCode::Char('r')));
        render(&mut app, 100, 32);
        let confirm = app
            .hit_zones
            .iter()
            .find_map(|(rect, hit)| (*hit == Hit::ConfigConfirm).then_some(*rect))
            .unwrap();
        app.handle_mouse(click(confirm.x, confirm.y));
        assert!(app.batch_review.as_ref().unwrap().note_editing);
        assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), "gain: 2\n");
        app.handle_paste("Restore click test");
        app.handle_key(key(KeyCode::Enter)); // finish required note
        app.handle_mouse(click(confirm.x, confirm.y));
        app.handle_key(key(KeyCode::Enter));
        assert!(
            app.batch_review
                .as_ref()
                .unwrap()
                .outcome
                .as_ref()
                .unwrap()
                .contains("Restored 2")
        );
        let text = screen_text(&render(&mut app, 100, 32));
        assert!(text.contains("Session restore result"));
        assert_eq!(std::fs::read_to_string(&paths[0]).unwrap(), "gain: 1\n");
        assert_eq!(std::fs::read_to_string(&paths[1]).unwrap(), "gain: 1\n");
        crate::fixture::confirm_action(&mut app);
        assert!(app.batch_review.is_none());
        assert_eq!(app.global_history.as_ref().unwrap().entries.len(), 4);
    }

    #[test]
    fn new_badge_is_yellow_when_checkout_is_behind_selected_branch() {
        let (mut app, _temp) = make_normal_app();
        let sub = app
            .repo
            .as_ref()
            .unwrap()
            .find_submodule("subA")
            .unwrap()
            .open()
            .unwrap();
        let old = sub
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent_id(0)
            .unwrap();
        crate::gitops::checkout_oid(&sub, old).unwrap();
        app.refresh();
        app.select_repo(1);
        let buffer = render(&mut app, 100, 30);
        let rect = app
            .hit_zones
            .iter()
            .find_map(|(rect, hit)| (*hit == Hit::RepoRow(1)).then_some(*rect))
            .unwrap();
        let row: String = (rect.x..rect.x + rect.width)
            .map(|x| buffer[(x, rect.y)].symbol())
            .collect();
        assert!(row.contains("[new: 1]"), "{row}");
        let position = row.find("[new: 1]").unwrap();
        let x = rect.x + UnicodeWidthStr::width(&row[..position]) as u16;
        assert_eq!(buffer[(x, rect.y)].fg, CURRENT);
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
    fn rendered_repo_row_ignores_clicks() {
        let (mut app, _t) = make_normal_app();
        render(&mut app, 80, 24);

        let (rect, _) = app
            .hit_zones
            .iter()
            .find(|(_, h)| *h == Hit::RepoRow(2))
            .copied()
            .unwrap();
        app.handle_mouse(click(rect.x + 2, rect.y));

        assert_eq!(app.repos.selected(), 0);
        assert_eq!(app.focus, Focus::Repos);
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.repos.selected(), 2);
    }

    #[test]
    fn rendered_commit_row_ignores_clicks() {
        let (mut app, _t) = make_normal_app();
        render(&mut app, 80, 24);

        let (rect, _) = app
            .hit_zones
            .iter()
            .find(|(_, h)| *h == Hit::CommitRow(1))
            .copied()
            .unwrap();
        app.handle_mouse(click(rect.x + 2, rect.y));

        assert_eq!(app.focus, Focus::Repos);
        assert_eq!(app.entries[0].scroll.selected(), 0);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.entries[0].scroll.selected(), 1);
    }

    #[test]
    fn help_popup_renders_keys_and_ignores_clicks() {
        let (mut app, _t) = make_normal_app();
        app.popup = Some(Popup::Help);
        let buf = render(&mut app, 80, 24);
        let text = screen_text(&buf);

        assert!(text.contains("move selection"));
        assert!(text.contains("pick / un-pick"));
        assert!(text.contains("Ctrl+P,P"));

        app.handle_mouse(click(40, 12));
        assert_eq!(app.popup, Some(Popup::Help));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.popup, None);
    }

    #[test]
    fn pull_confirmation_renders_and_mouse_cancel_does_not_start_network() {
        let (mut app, _temp) = make_normal_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('p')));
        let text = screen_text(&render(&mut app, 80, 24));
        assert!(text.contains("Confirm pull"));
        assert!(text.contains("fast-forwards"));
        assert!(text.contains("[ Pull ]") && text.contains("[ Cancel ]"));
        assert!(text.contains("Note: type/paste"));
        assert!(
            app.hit_zones
                .iter()
                .any(|(_, hit)| *hit == Hit::PullConfirm)
        );
        let cancel = app
            .hit_zones
            .iter()
            .find_map(|(rect, hit)| (*hit == Hit::PullCancel).then_some(*rect))
            .unwrap();
        app.handle_mouse(click(0, 0));
        assert_eq!(
            app.popup,
            Some(Popup::PullConfirm),
            "outside clicks cannot pull"
        );
        app.handle_mouse(click(cancel.x, cancel.y));
        assert_eq!(app.popup, Some(Popup::PullConfirm));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.popup, None);
        assert!(app.network_job.is_none());
        app.popup = Some(Popup::PullConfirm);
        for (width, height) in [(1, 1), (8, 3), (12, 5), (30, 10)] {
            render(&mut app, width, height);
        }
    }

    #[test]
    fn selected_pull_confirmation_names_its_target_and_all_pull_remains_available() {
        let (mut app, _temp) = make_normal_app();
        let text = screen_text(&render(&mut app, 100, 32));
        assert!(text.contains("p pull selected") && text.contains("Ctrl+P,P pull all"));
        app.select_repo(1);
        app.handle_key(key(KeyCode::Char('p')));
        let text = screen_text(&render(&mut app, 100, 32));
        assert!(text.contains("Confirm pull selected repository"));
        assert!(text.contains("Pull only subA?"));
        assert!(text.contains("Other repositories will not be fetched or pulled."));
        assert!(text.contains("Note: type/paste"));
        assert!(!text.contains("Pull the main repository"));
        app.handle_paste("Update just steering");
        app.handle_key(key(KeyCode::Enter));
        let text = screen_text(&render(&mut app, 100, 32));
        assert!(text.contains("Enter/y pull selected"));
        assert!(app.network_job.is_none());
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('p')));
        let text = screen_text(&render(&mut app, 100, 32));
        assert!(text.contains("Confirm pull all repositories"));
        assert!(text.contains("Pull the main repository"));
        assert!(app.action_note.is_empty() && app.action_note_editing);
        for (width, height) in [(1, 1), (8, 3), (12, 5), (30, 10)] {
            render(&mut app, width, height);
        }
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
    fn rendered_branch_row_ignores_click_and_enter_prompts_for_note() {
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
        assert!(!app.action_note_editing);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.action_note_editing);
        app.handle_paste("Branch click test");
        app.handle_key(key(KeyCode::Enter)); // finish required note
        app.handle_mouse(click(rect.x + 3, rect.y));
        app.handle_key(key(KeyCode::Enter));

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
        crate::fixture::confirm_action(&mut app);
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
        assert!(text.contains("Note: ▌"));

        // Both buttons registered as hit zones.
        assert!(app.hit_zones.iter().any(|(_, h)| *h == Hit::SaveApply));
        assert!(app.hit_zones.iter().any(|(_, h)| *h == Hit::SaveCancel));
    }

    #[test]
    fn keyboard_apply_button_applies_and_shows_result() {
        let (mut app, _t) = make_normal_app();
        app.select_repo(1);
        app.focus = Focus::Commits;
        app.handle_key(key(KeyCode::Down));
        crate::fixture::confirm_action(&mut app);
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
        assert!(app.action_note_editing);
        app.handle_paste("Commit click test");
        app.handle_key(key(KeyCode::Enter)); // finish required note
        app.handle_mouse(click(rect.x + 2, rect.y));
        app.handle_key(key(KeyCode::Enter));

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
        assert!(app.save_popup.is_some());
        app.handle_key(key(KeyCode::Enter));
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
