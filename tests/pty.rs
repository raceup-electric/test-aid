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

    /// SGR mouse coordinates are one-based, unlike the app's hit zones.
    fn click(&mut self, column: u16, row: u16) -> Result<(), BoxError> {
        self.send(&format!(
            "\u{1b}[<0;{column};{row}M\u{1b}[<0;{column};{row}m"
        ))
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), BoxError> {
        self._master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
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
fn startup_and_cancelled_pull_never_invoke_git() -> TestResult {
    let (temp, root) = super_repo("pty-manual-pull")?;
    let repo = git2::Repository::open(&root)?;
    // A real local remote ensures the old startup fetch would execute Git.
    repo.remote("origin", root.to_str().unwrap())?;
    let trace = temp.path().join("git-trace.log");
    let mut session = PtySession::spawn(&root, Some(("GIT_TRACE", trace.to_str().unwrap())))?;
    session.wait_until_started()?;
    std::thread::sleep(Duration::from_millis(250));
    session.send("\u{10}pnq")?;
    let (status, raw) = session.finish();
    assert!(status.expect("cancel then q should exit").success());
    let screen = String::from_utf8_lossy(&raw);
    assert!(screen.contains("Confirm pull"), "{screen}");
    assert!(screen.contains("[ Cancel ]"), "{screen}");
    assert!(
        !trace.exists() || std::fs::read(&trace)?.is_empty(),
        "startup and cancellation must not execute a Git fetch or pull"
    );
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
fn clicking_ok_exits_outside_a_repo() -> TestResult {
    let temp = TempDir::new("pty-notrepo-click")?;
    let mut session = PtySession::spawn(temp.path(), None)?;
    session.wait_until_started()?;
    session.click(40, 13)?;
    let (status, raw) = session.finish();

    let status = status.expect("clicking OK should exit");
    assert!(status.success(), "raw output: {raw:?}");
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
fn slash_filter_finds_and_picks_a_submodule_commit() -> TestResult {
    let (_temp, root) = super_repo("pty-filter")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    // Select subA, focus commits, filter to its older commit, and pick it.
    session.send("j\t/a1\r\rq")?;
    let (status, raw) = session.finish();

    assert!(status.expect("taid exits after q").success());
    let screen = String::from_utf8_lossy(&raw);
    assert!(
        screen.contains("matching /a"),
        "filtered list rendered: {screen:?}"
    );
    assert!(
        screen.contains("✓ pick"),
        "matching commit picked: {screen:?}"
    );
    Ok(())
}

#[test]
fn branch_switch_flow_end_to_end() -> TestResult {
    let (_temp, root) = super_repo("pty-branch")?;
    // A second branch to switch to.
    let repo = taid::gitx::discover(&root)?.expect("repo");
    {
        let head = repo.head()?.peel_to_commit()?;
        repo.branch("feature", &head, false)?;
    }
    drop(repo);

    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    // b → popup, Enter → switch to `feature` (row 0), q → quit.
    session.send("b\rq")?;
    let (status, raw) = session.finish();

    let status = status.expect("taid should exit on its own after q");
    assert!(status.success(), "raw output: {raw:?}");
    let screen = String::from_utf8_lossy(&raw);
    assert!(
        screen.contains("Branches — super"),
        "popup rendered: {screen:?}"
    );
    assert!(
        screen.contains("feature"),
        "branch listed/switched: {screen:?}"
    );
    Ok(())
}

#[test]
fn clicking_branch_row_switches_branch() -> TestResult {
    let (_temp, root) = super_repo("pty-branch-click")?;
    let repo = taid::gitx::discover(&root)?.expect("repo");
    let head = repo.head()?.peel_to_commit()?;
    repo.branch("feature", &head, false)?;
    drop(head);
    drop(repo);

    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    session.send("b")?;
    session.click(20, 11)?; // first branch row in the centered popup
    session.send("q")?;
    let (status, raw) = session.finish();

    let status = status.expect("taid should exit after branch click");
    assert!(status.success(), "raw output: {raw:?}");
    let repo = taid::gitx::discover(&root)?.expect("repo");
    assert_eq!(
        taid::gitx::head_info(&repo)?,
        taid::gitx::HeadInfo::Branch("feature".into())
    );
    Ok(())
}

#[test]
fn save_flow_end_to_end() -> TestResult {
    let (_temp, root) = super_repo("pty-save")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    // j → subA, Tab → commits, j → older commit, Enter → pick,
    // s → confirm, Enter → apply, q → close result, q → quit.
    session.send("j\tj\rs\rqq")?;
    let (status, raw) = session.finish();

    let status = status.expect("taid should exit on its own");
    assert!(status.success(), "raw output: {raw:?}");
    let screen = String::from_utf8_lossy(&raw);
    assert!(
        screen.contains("Save — apply 1 change?"),
        "confirm: {screen:?}"
    );
    assert!(screen.contains("Save result"), "result: {screen:?}");
    assert!(screen.contains('✓'), "success line: {screen:?}");

    // On-disk verification (the user's acceptance criteria):
    let super_repo = taid::gitx::discover(&root)?.expect("super repo");
    let ov = taid::gitx::overview(&super_repo)?;
    let sm = &ov.submodules[0];
    let sub = taid::gitx::discover(&root.join("subA"))?.expect("sub repo");
    // The branch still points at the newest commit; HEAD moved to its parent.
    let sub_main = sub
        .find_branch("main", git2::BranchType::Local)?
        .get()
        .target()
        .expect("sub main branch");
    let a2 = sub.find_commit(sub_main)?;
    let a1 = a2.parent(0)?;
    assert_eq!(
        sub_main,
        a2.id(),
        "newer commits still reachable in the submodule"
    );
    assert_eq!(sm.recorded_oid, Some(a2.id()), "HEAD tree unchanged");
    assert_eq!(
        sm.index_oid,
        Some(a1.id()),
        "pointer staged at the older commit"
    );
    assert_eq!(
        sub.head()?.target(),
        Some(a1.id()),
        "submodule workdir at a1"
    );
    Ok(())
}

#[test]
fn mouse_rows_and_apply_button_work_end_to_end() -> TestResult {
    let (_temp, root) = super_repo("pty-mouse-save")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;

    // At 80×24: subA is the second left row; the older commit is the
    // second history row on the right. Mouse clicks select/focus those rows.
    session.click(5, 4)?;
    session.click(40, 6)?;
    session.send("\rs")?; // pick selected commit, open Save
    // The confirmation popup is 60×5 and centered; Apply is on its last row.
    session.click(33, 13)?;
    session.send("qq")?; // close result, quit
    let (status, raw) = session.finish();

    let status = status.expect("taid should exit after clicking Apply");
    assert!(status.success(), "raw output: {raw:?}");
    let super_repo = taid::gitx::discover(&root)?.expect("super repo");
    let ov = taid::gitx::overview(&super_repo)?;
    let sub = taid::gitx::discover(&root.join("subA"))?.expect("sub repo");
    let staged = ov.submodules[0].index_oid.expect("staged pointer");
    assert_ne!(Some(staged), ov.submodules[0].recorded_oid);
    assert_eq!(sub.head()?.target(), Some(staged));
    Ok(())
}

#[test]
fn resize_redraws_and_quits_cleanly() -> TestResult {
    let (_temp, root) = super_repo("pty-resize")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    session.resize(40, 12)?;
    session.resize(100, 30)?;
    session.send("q")?;
    let (status, raw) = session.finish();

    let status = status.expect("taid should exit after resizing");
    assert!(status.success(), "raw output: {raw:?}");
    assert!(String::from_utf8_lossy(&raw).contains("subA"));
    Ok(())
}

#[test]
fn config_chord_edits_and_saves_a_real_file() -> TestResult {
    let (_temp, root) = super_repo("pty-config")?;
    let path = root.join("subA/ros_package/config/params.yaml");
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, "gain: 1\n")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    // Ctrl+C, C → configs; Tab/Enter → form; Enter/2/Enter → typed value;
    // Ctrl+S → disk; Esc → files; Ctrl+R, R → repositories; q → exit.
    session.send("\u{3}c\t\r\r2\r\u{13}\u{1b}\u{12}rq")?;
    let (status, raw) = session.finish();
    assert!(
        status.expect("config workflow should exit").success(),
        "{raw:?}"
    );
    assert_eq!(std::fs::read_to_string(&path)?, "gain: 2\n");
    let screen = String::from_utf8_lossy(&raw);
    assert!(screen.contains("Configurations"), "{screen}");
    assert!(
        screen.contains("integer"),
        "typed parameter input: {screen}"
    );
    assert!(screen.contains("Ctrl+S save"), "{screen}");
    Ok(())
}

#[test]
fn quitting_without_save_changes_nothing() -> TestResult {
    let (_temp, root) = super_repo("pty-nosave")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    // Pick a commit but quit without saving.
    session.send("j\tj\rq")?;
    let (status, raw) = session.finish();

    let status = status.expect("taid should exit on its own");
    assert!(status.success(), "raw output: {raw:?}");
    let screen = String::from_utf8_lossy(&raw);
    assert!(screen.contains('✓'), "pick was visible: {screen:?}");

    let super_repo = taid::gitx::discover(&root)?.expect("super repo");
    let ov = taid::gitx::overview(&super_repo)?;
    let sm = &ov.submodules[0];
    let sub = taid::gitx::discover(&root.join("subA"))?.expect("sub repo");
    let a2 = sub.head()?.peel_to_commit()?;
    assert_eq!(sm.recorded_oid, Some(a2.id()));
    assert_eq!(sm.index_oid, Some(a2.id()), "index untouched without save");
    assert_eq!(sub.head()?.target(), Some(a2.id()), "workdir untouched");
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
