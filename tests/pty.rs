//! Integration tests that run the real `taid` binary inside a
//! pseudo-terminal — the closest automation gets to "a user opened it".
//!
//! These tests superseded an earlier script/printf-based approach that was
//! fragile (no timeouts, zero-sized ptys) and, when interrupted, could leave
//! the host terminal with mouse tracking enabled.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem,
};

use taid::fixture::{SubSpec, TempDir};

type TestResult = Result<(), Box<dyn std::error::Error>>;
type BoxError = Box<dyn std::error::Error>;

const BIN: &str = env!("CARGO_BIN_EXE_taid");
const CHILD_TIMEOUT: Duration = Duration::from_secs(10);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(20);

/// Build a superproject fixture with one submodule. Keep the returned
/// `TempDir` alive for as long as the repository is used.
fn super_repo(tag: &str) -> Result<(TempDir, PathBuf), BoxError> {
    let temp = TempDir::new(tag)?;
    let root = temp.path().join("super");
    taid::fixture::make_super(
        &root,
        &[SubSpec {
            name: "subA",
            msgs: &["a1", "a2"],
            recorded: None,
            drop_workdir: false,
        }],
    )?;
    Ok((temp, root))
}

/// A taid process running in a fresh 80x24 pty.
struct PtySession {
    /// Kept alive so the pty stays open; never used directly.
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send>,
    output: mpsc::Receiver<Vec<u8>>,
    /// Output read before `finish` (e.g. while waiting for the first frame).
    early_output: Vec<u8>,
    /// Set once the child has been waited on, so Drop does not kill twice.
    finished: bool,
}

impl PtySession {
    fn spawn(cwd: &Path, env: Option<(&str, &str)>) -> Result<Self, BoxError> {
        let pty = NativePtySystem::default();
        let pair = pty.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(BIN);
        cmd.cwd(cwd);
        if let Some((key, value)) = env {
            cmd.env(key, value);
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave); // so EOF reaches the master once taid exits

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            _master: pair.master,
            writer,
            child,
            output: rx,
            early_output: Vec::new(),
            finished: false,
        })
    }

    /// Wait until taid has emitted output (its first frame).
    ///
    /// Input must not be sent earlier: before taid enables raw mode, the
    /// pty is in canonical mode with ICRNL, which would translate our
    /// `\r` into `\n` — and crossterm parses `\n` in raw mode as Ctrl+J,
    /// not Enter. Waiting for the first frame proves raw mode is on.
    fn wait_until_started(&mut self) -> Result<(), BoxError> {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        while self.early_output.is_empty() {
            match self.output.try_recv() {
                Ok(chunk) => self.early_output.extend_from_slice(&chunk),
                Err(mpsc::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return Err("taid produced no output before the deadline".into());
                    }
                    std::thread::sleep(POLL);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("the pty closed before taid drew anything".into());
                }
            }
        }
        Ok(())
    }

    fn send(&mut self, keys: &str) -> Result<(), BoxError> {
        self.writer.write_all(keys.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    /// Wait for taid to exit (killing it at the deadline), then collect
    /// everything it wrote. `status` is `None` when the deadline forced a
    /// kill — every test treats that as a failure.
    fn finish(mut self) -> (Option<ExitStatus>, Vec<u8>) {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        let status = loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break None;
            }
            std::thread::sleep(POLL);
        };
        self.finished = true;

        let mut out = std::mem::take(&mut self.early_output);
        let drain_by = Instant::now() + DRAIN_TIMEOUT;
        while let Some(remaining) = drain_by.checked_duration_since(Instant::now()) {
            match self.output.recv_timeout(remaining) {
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(_) => break,
            }
        }
        (status, out)
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
        }
    }
}

#[test]
fn in_a_repo_q_quits_cleanly() -> TestResult {
    let (_temp, root) = super_repo("pty-repo")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    session.send("q")?;
    let (status, raw) = session.finish();

    let status = status.expect("taid should exit on its own after q");
    assert!(status.success(), "raw output: {raw:?}");
    let screen = String::from_utf8_lossy(&raw);
    assert!(
        screen.contains("Repos"),
        "repo pane rendered; got: {screen:?}"
    );
    assert!(screen.contains("subA"), "submodule listed; got: {screen:?}");
    Ok(())
}

#[test]
fn outside_a_repo_shows_popup_and_ok_exits() -> TestResult {
    let temp = TempDir::new("pty-notrepo")?;
    let mut session = PtySession::spawn(temp.path(), None)?;
    session.wait_until_started()?;
    session.send("\r")?; // press OK with Enter
    let (status, raw) = session.finish();

    let Some(status) = status else {
        panic!(
            "taid did not exit after Enter; raw output: {:?}",
            String::from_utf8_lossy(&raw)
        );
    };
    assert!(status.success(), "raw output: {raw:?}");
    let screen = String::from_utf8_lossy(&raw);
    assert!(
        screen.contains("This directory is not a git repository"),
        "popup message; got: {screen:?}"
    );
    assert!(screen.contains("[ OK ]"), "OK button; got: {screen:?}");

    // The popup was rendered before the alternate screen was left.
    let popup = screen
        .find("This directory is not a git repository")
        .expect("popup text present");
    let leave = screen
        .find("\u{1b}[?1049l")
        .expect("leave-alternate-screen sequence present");
    assert!(popup < leave, "popup must render before leaving");
    Ok(())
}

#[test]
fn picking_a_commit_marks_it_and_quits() -> TestResult {
    let (_temp, root) = super_repo("pty-pick")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    // Tab → commits pane, j → older commit, Enter → pick it, q → quit.
    session.send("\tj\rq")?;
    let (status, raw) = session.finish();

    let status = status.expect("taid should exit on its own after q");
    assert!(status.success(), "raw output: {raw:?}");
    let screen = String::from_utf8_lossy(&raw);
    assert!(screen.contains('✓'), "pick marker rendered: {screen:?}");
    assert!(screen.contains("pick"), "pick summary rendered");
    Ok(())
}

#[test]
fn panic_restores_the_terminal_before_printing() -> TestResult {
    if !cfg!(debug_assertions) {
        // The TAID_PANIC_TEST hook only exists in debug builds.
        return Ok(());
    }
    let (_temp, root) = super_repo("pty-panic")?;
    let session = PtySession::spawn(&root, Some(("TAID_PANIC_TEST", "1")))?;
    let (status, raw) = session.finish();

    let status = status.expect("a panicking taid still terminates");
    assert!(!status.success(), "raw output: {raw:?}");
    let screen = String::from_utf8_lossy(&raw);

    // The panic hook must restore the terminal BEFORE the message prints:
    // the leave-alt-screen and mouse-off sequences precede the panic text.
    let leave = screen
        .find("\u{1b}[?1049l")
        .expect("alternate screen was left");
    let mouse_off = screen
        .find("\u{1b}[?1006l")
        .expect("mouse capture was disabled");
    let message = screen
        .find("TAID_PANIC_TEST requested")
        .expect("panic message was printed");
    assert!(leave < message, "alt screen left before message");
    assert!(mouse_off < message, "mouse disabled before message");
    Ok(())
}
