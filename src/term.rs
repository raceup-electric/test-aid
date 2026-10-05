//! Terminal lifecycle: alternate screen, raw mode, mouse capture, and a
//! panic hook that puts the terminal back before a panic message prints.

use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
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
    std::panic::set_hook(Box::new(move |info| {
        // Best effort: leave raw mode and the alternate screen first.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        default_hook(info);
    }));
}

pub fn init() -> Result<Tui> {
    enable_raw_mode().context("taid needs an interactive terminal")?;
    let result = (|| {
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
            .context("preparing the terminal")?;
        let mut terminal =
            Terminal::new(CrosstermBackend::new(stdout)).context("preparing the terminal")?;
        terminal.hide_cursor().context("preparing the terminal")?;
        Ok(terminal)
    })();
    if result.is_err() {
        // EnterAlternateScreen may have succeeded before a later setup step
        // failed, so restore both terminal modes on every partial failure.
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
    result
}

pub fn restore(terminal: &mut Tui) {
    let _ = terminal.show_cursor();
    let _ = execute!(
        terminal.backend_mut(),
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
