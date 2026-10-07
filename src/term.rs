//! Terminal lifecycle: alternate screen, raw mode, keyboard input, and a
//! panic hook that puts the terminal back before a panic message prints.

use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Install a panic hook that restores the terminal, then runs the previous
/// (default) hook so panics still print their message readably.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    let ui_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        // Background operations catch/report their failures through the UI.
        // A caught worker panic must not reset raw mode or print over the TUI.
        if std::thread::current().id() != ui_thread {
            return;
        }
        // Best effort: leave raw mode and the alternate screen first.
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        default_hook(info);
    }));
}

pub fn init() -> Result<Tui> {
    enable_raw_mode().context("taid needs an interactive terminal")?;
    let result = (|| {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            DisableMouseCapture,
            EnableBracketedPaste
        )
        .context("preparing the terminal")?;
        let mut terminal =
            Terminal::new(CrosstermBackend::new(stdout)).context("preparing the terminal")?;
        terminal.hide_cursor().context("preparing the terminal")?;
        Ok(terminal)
    })();
    if result.is_err() {
        // EnterAlternateScreen may have succeeded before a later setup step
        // failed, so restore both terminal modes on every partial failure.
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
    result
}

pub fn restore(terminal: &mut Tui) {
    let _ = terminal.show_cursor();
    let _ = execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_hook_runs_and_returns_control() {
        // A silent base hook keeps test output clean; taid's hook wraps it.
        std::panic::set_hook(Box::new(|_| {}));
        install_panic_hook();
        let result = std::panic::catch_unwind(|| panic!("hook exercise"));
        let _ = std::panic::take_hook();
        assert!(result.is_err());
    }
}
