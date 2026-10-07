//! The event loop: block on terminal input, feed the app, redraw.

use anyhow::{Context, Result};
use crossterm::event::{self, Event};
use std::time::Duration;

use crate::app::App;
use crate::term::Tui;
use crate::ui;

pub fn run(app: &mut App, terminal: &mut Tui) -> Result<()> {
    // Crossterm installs its input source/SIGWINCH handler lazily. Establish it
    // before drawing the first frame, so a resize immediately after startup is
    // observed just like subsequent resizes.
    event::poll(Duration::ZERO).context("initializing terminal input")?;
    let mut drawn_once = false;
    loop {
        app.poll_network();
        terminal
            .draw(|f| ui::draw(f, app))
            .context("rendering the interface")?;

        if !drawn_once {
            drawn_once = true;
            // Test hook (debug builds only): lets the pty integration tests
            // verify that the panic hook restores the terminal before the
            // panic message becomes visible.
            if cfg!(debug_assertions) && std::env::var_os("TAID_PANIC_TEST").is_some() {
                panic!("TAID_PANIC_TEST requested");
            }
        }

        if !event::poll(Duration::from_millis(100)).context("polling terminal input")? {
            continue;
        }
        match event::read().context("reading terminal input")? {
            Event::Key(key) => app.handle_key(key),
            Event::Mouse(_) => {} // keyboard-only, including unsolicited terminal mouse reports
            Event::Resize(_, _) => {} // redrawn on the next iteration
            Event::FocusGained | Event::FocusLost => {}
            Event::Paste(text) => app.handle_paste(&text),
        }

        if app.should_quit {
            return Ok(());
        }
    }
}
