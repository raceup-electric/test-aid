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

    /// Wait for the complete first frame, rather than terminal setup bytes.
    ///
    /// Input must not be sent earlier: before taid enables raw mode, the
    /// pty is in canonical mode with ICRNL, which would translate our
    /// `\r` into `\n` — and crossterm parses `\n` in raw mode as Ctrl+J,
    /// not Enter. Waiting for the first frame proves raw mode is on.
    fn wait_until_started(&mut self) -> Result<(), BoxError> {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        // hide_cursor is emitted once during init, then at the end of a frame.
        // Both are needed: setup output alone does not prove the event loop is ready.
        while self
            .early_output
            .windows(b"\x1b[?25l".len())
            .filter(|w| *w == b"\x1b[?25l")
            .count()
            < 2
        {
            match self.output.try_recv() {
                Ok(chunk) => self.early_output.extend_from_slice(&chunk),
                Err(mpsc::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return Err(
                            "taid did not complete its first frame before the deadline".into()
                        );
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
        while let Ok(chunk) = self.output.try_recv() {
            self.early_output.extend_from_slice(&chunk);
        }
        let start = self.early_output.len();
        self._master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let deadline = Instant::now() + CHILD_TIMEOUT;
        loop {
            let output = &self.early_output[start..];
            // A resize clears the screen and then flushes a complete new frame.
            if let Some(clear) = output.windows(4).position(|w| w == b"\x1b[2J") {
                if output[clear + 4..].windows(6).any(|w| w == b"\x1b[?25l") {
                    break;
                }
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    format!(
                        "taid did not redraw at {cols}x{rows}; output: {:?}",
                        String::from_utf8_lossy(output)
                    )
                })?;
            let chunk = self.output.recv_timeout(remaining).map_err(|error| {
                format!(
                    "taid did not redraw at {cols}x{rows}: {error}; output: {:?}",
                    String::from_utf8_lossy(output)
                )
            })?;
            self.early_output.extend_from_slice(&chunk);
        }
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
    session.send("\u{10}p\u{1b}")?;
    // ESC followed immediately by q is an Alt+q key in terminal protocols.
    std::thread::sleep(Duration::from_millis(100));
    session.send("q")?;
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
fn selected_submodule_pull_updates_only_that_checkout_and_records_its_note() -> TestResult {
    let (temp, root) = super_repo("pty-selected-pull")?;
    let parent = git2::Repository::open(&root)?;
    let parent_head = parent.head()?.target();
    // Pulling the whole stack would attempt this invalid remote and report failure.
    parent.remote(
        "origin",
        temp.path().join("unselected-main").to_str().unwrap(),
    )?;
    let local = git2::Repository::open(root.join("subA"))?;
    let old = local.head()?.target().unwrap();
    let remote_path = temp.path().join("remote-subA");
    let remote = git2::Repository::clone(root.join("subA").to_str().unwrap(), &remote_path)?;
    local.remote("origin", remote_path.to_str().unwrap())?;
    local.reference("refs/remotes/origin/main", old, false, "fixture upstream")?;
    local
        .find_branch("main", git2::BranchType::Local)?
        .set_upstream(Some("origin/main"))?;
    let new = taid::fixture::commit_file(
        &remote,
        "latest.txt",
        "new trial\n",
        "Latest steering trial",
        taid::fixture::BASE_TIME + 10_000,
    )?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    session.send("jpSteering update only\r\r")?;
    let deadline = Instant::now() + CHILD_TIMEOUT;
    while local.head()?.target() != Some(new) {
        assert!(
            Instant::now() < deadline,
            "selected submodule was not pulled"
        );
        std::thread::sleep(POLL);
    }
    // Quit retries are harmless while the worker finishes its durable outcome.
    while session.child.try_wait()?.is_none() {
        assert!(Instant::now() < deadline, "pull worker did not finish");
        session.send("\u{11}")?;
        std::thread::sleep(POLL);
    }
    let (status, raw) = session.finish();
    assert!(status.expect("selected pull exits").success(), "{raw:?}");
    let screen = String::from_utf8_lossy(&raw);
    assert!(screen.contains("Pull only subA?"), "{screen}");
    assert!(!screen.contains("Pull the main repository"));
    assert_eq!(parent.head()?.target(), parent_head);
    assert_eq!(
        taid::gitx::overview(&parent)?.submodules[0].index_oid,
        Some(old)
    );
    let browser = taid::history::Browser::open(&root)?;
    assert_eq!(browser.repository_events.len(), 1);
    let event = &browser.repository_events[0];
    assert_eq!(event.intent.subject, "subA");
    assert_eq!(event.intent.note, "Steering update only");
    assert!(event.outcome.as_ref().unwrap().success);
    assert_eq!(browser.notes().len(), 1);
    Ok(())
}

#[test]
fn config_history_survives_restart_and_restore_requires_confirmation() -> TestResult {
    let (_temp, root) = super_repo("pty-config-history")?;
    let path = root.join("subA/config/params.yaml");
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, "gain: 1\n")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    session.send("\u{3}c\t\r\r2\r\u{13}Tuning\r\r\u{11}")?;
    let (status, _) = session.finish();
    assert!(status.expect("first session exits").success());
    assert_eq!(std::fs::read_to_string(&path)?, "gain: 2\n");
    assert_eq!(
        taid::history::Store::open_in(&path, &root)?.list()?[0].note,
        "Tuning"
    );
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    // Global history opens directly from the default repository view, without
    // first locating the submodule or opening its config form.
    session.send("h\r\rcbr\u{13}Restore baseline\r\r\u{11}")?;
    let (status, raw) = session.finish();
    assert!(status.expect("restored session exits").success());
    let screen = String::from_utf8_lossy(&raw);
    assert!(screen.contains("Historical values loaded"), "{screen}");
    assert!(screen.contains("Review config save"), "{screen}");
    assert_eq!(std::fs::read_to_string(&path)?, "gain: 1\n");
    assert_eq!(
        taid::history::Store::open_in(&path, &root)?.list()?.len(),
        2
    );
    Ok(())
}

#[test]
fn session_groups_multiple_config_saves_and_restores_the_whole_session_after_restart() -> TestResult
{
    let (_temp, root) = super_repo("pty-session-history")?;
    let first = root.join("subA/config/a.yaml");
    let second = root.join("subA/config/b.yaml");
    std::fs::create_dir_all(first.parent().unwrap())?;
    std::fs::write(&first, "gain: 1\n")?;
    std::fs::write(&second, "gain: 1\n")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    // Two saves to a.yaml, one to b.yaml, all in the same process/session.
    session.send("\u{3}c\t\r\r2\r\u{13}First tuning\r\r\r3\r\u{13}Second tuning\r\r\u{12}r\u{3}c\tj\r\r4\r\u{13}Other config\r\r\u{12}rhnDry track tuning\r\u{11}")?;
    let (status, raw) = session.finish();
    assert!(
        status.expect("save session exits").success(),
        "{}",
        String::from_utf8_lossy(&raw)
    );
    assert_eq!(std::fs::read_to_string(&first)?, "gain: 3\n");
    assert_eq!(std::fs::read_to_string(&second)?, "gain: 4\n");
    let browser = taid::history::Browser::open(&root)?;
    assert_eq!(browser.entries.len(), 3);
    assert_eq!(browser.visible.len(), 1);
    assert!(browser.selected_label().contains("closed"));
    assert!(browser.selected_label().contains("Dry track tuning"));
    assert_eq!(browser.targets()?.len(), 2);
    let original_id = browser.entries[0].session.clone().unwrap();
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    // Root session row: b chooses its baseline, r opens the ALL-file review.
    session.send("hcbrUndo dry tuning\r\r\r\u{11}")?;
    let (status, raw) = session.finish();
    assert!(status.expect("restore session exits").success());
    let screen = String::from_utf8_lossy(&raw);
    // Incremental ANSI repainting can split title words; rendering unit tests
    // check the full title while this test checks real persisted outcomes.
    assert!(screen.contains("Confirm ALL"), "{screen}");
    assert_eq!(std::fs::read_to_string(&first)?, "gain: 1\n");
    assert_eq!(std::fs::read_to_string(&second)?, "gain: 1\n");
    let browser = taid::history::Browser::open(&root)?;
    assert_eq!(browser.visible.len(), 2);
    assert_eq!(browser.entries.len(), 5);
    assert_ne!(
        browser.entries[0].session.as_deref(),
        Some(original_id.as_str())
    );
    assert_eq!(browser.entries[0].note, "Undo dry tuning");
    assert!(taid::session::recoveries(&root)?.0.is_empty());
    Ok(())
}

#[test]
fn killing_taid_retains_immediate_saves_and_marks_the_session_interrupted() -> TestResult {
    let (_temp, root) = super_repo("pty-session-crash")?;
    let path = root.join("subA/config/gains.yaml");
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, "gain: 1\n")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    session.send("\u{3}c\t\r\r2\r\u{13}Interrupted test\r\r")?;
    let deadline = Instant::now() + CHILD_TIMEOUT;
    while std::fs::read_to_string(&path)? != "gain: 2\n" {
        assert!(Instant::now() < deadline, "save never reached disk");
        std::thread::sleep(POLL);
    }
    session.child.kill()?;
    let (status, _) = session.finish();
    assert!(!status.expect("killed child exits").success());
    let browser = taid::history::Browser::open(&root)?;
    assert_eq!(browser.visible.len(), 1);
    assert_eq!(browser.entries.len(), 1);
    assert!(browser.selected_label().contains("interrupted"));
    assert_eq!(browser.targets()?[0].before, "gain: 1\n");
    assert_eq!(browser.targets()?[0].after, "gain: 2\n");
    assert_eq!(
        std::fs::read_to_string(&path)?,
        "gain: 2\n",
        "opening interrupted history never rolls it back automatically"
    );
    Ok(())
}

#[test]
fn branch_only_history_survives_restart_and_reviews_restore_in_real_terminal() -> TestResult {
    let (_temp, root) = super_repo("pty-repository-history")?;
    let repo = git2::Repository::open(&root)?;
    repo.branch("feature", &repo.head()?.peel_to_commit()?, false)?;
    let mut first = PtySession::spawn(&root, None)?;
    first.wait_until_started()?;
    first.send("bnBranch trial\r\rh\u{11}")?;
    let (status, raw) = first.finish();
    assert!(status.expect("first session exits").success());
    assert!(
        String::from_utf8_lossy(&raw).contains("session history"),
        "{}",
        String::from_utf8_lossy(&raw)
    );
    assert_eq!(repo.head()?.shorthand(), Some("feature"));
    let records = taid::repository_history::events(&root)?.0;
    assert_eq!(records.len(), 1);
    assert_eq!(
        taid::session::info(&root, &records[0].intent.session)?.state,
        "closed"
    );
    let mut second = PtySession::spawn(&root, None)?;
    second.wait_until_started()?;
    // Expand the session, select before, review the action, explicitly confirm.
    second.send("h\rbrRestore main\r\r\u{11}")?;
    let (status, _raw) = second.finish();
    assert!(status.expect("restoring session exits").success());
    assert_eq!(repo.head()?.shorthand(), Some("main"));
    let records = taid::repository_history::events(&root)?.0;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].intent.kind, "restore repository");
    assert_ne!(records[0].intent.session, records[1].intent.session);
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
fn mouse_ok_is_ignored_outside_a_repo() -> TestResult {
    let temp = TempDir::new("pty-notrepo-click")?;
    let mut session = PtySession::spawn(temp.path(), None)?;
    session.wait_until_started()?;
    session.click(40, 13)?;
    std::thread::sleep(Duration::from_millis(100));
    assert!(session.child.try_wait()?.is_none(), "mouse cannot quit");
    session.send("\r")?;
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
    session.send("bnBranch test\r\rq")?;
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
fn branch_rows_ignore_mouse_and_switch_with_keyboard() -> TestResult {
    let (_temp, root) = super_repo("pty-branch-click")?;
    let repo = taid::gitx::discover(&root)?.expect("repo");
    let head = repo.head()?.peel_to_commit()?;
    repo.branch("feature", &head, false)?;
    drop(head);
    drop(repo);

    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    session.send("b")?;
    session.click(20, 11)?; // mouse is ignored
    session.send("\rBranch keyboard test\r\rq")?;
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
    session.send("j\tj\rsReplay commit\r\rqq")?;
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
fn mouse_rows_are_ignored_and_keyboard_save_works_end_to_end() -> TestResult {
    let (_temp, root) = super_repo("pty-mouse-save")?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;

    // At 80×24: subA is the second left row; the older commit is the
    // second history row on the right. Mouse clicks select/focus those rows.
    session.click(5, 4)?;
    session.click(40, 6)?;
    session.send("j\tj\rs")?; // pick selected commit, open Save
    // The confirmation popup is 60×5 and centered; Apply is on its last row.
    session.click(33, 13)?;
    session.send("Replay commit\r\rqq")?; // note, finish, confirm, close result, quit
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
    // Also keep the original rapid-resize/quit case: queued resize events
    // must not swallow the following key, even when no redraw separates them.
    for (cols, rows) in [(60, 16), (120, 36)] {
        session._master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
    }
    session.send("q")?;
    let (status, raw) = session.finish();

    let status = status.expect("taid should exit after resizing");
    assert!(status.success(), "raw output: {raw:?}");
    assert!(String::from_utf8_lossy(&raw).contains("subA"));
    Ok(())
}

#[test]
fn session_notes_collect_config_branch_and_commit_changes_after_restart() -> TestResult {
    let (_temp, root) = super_repo("pty-session-notes")?;
    let path = root.join("subA/config/params.yaml");
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, "gain: 1\n")?;
    let repo = git2::Repository::open(&root)?;
    repo.branch("feature", &repo.head()?.peel_to_commit()?, false)?;
    let mut first = PtySession::spawn(&root, None)?;
    first.wait_until_started()?;
    first.send("\u{3}c\t\r\r2\r\u{13}Tune steering\r\r\u{1b}\u{12}rbnUse test branch\r\rj\tj\rsReplay sensor data\r\rq\u{11}")?;
    let (status, raw) = first.finish();
    assert!(status.expect("noted session exits").success(), "{raw:?}");
    assert_eq!(std::fs::read_to_string(&path)?, "gain: 2\n");
    assert_eq!(repo.head()?.shorthand(), Some("feature"));
    let mut browser = taid::history::Browser::open(&root)?;
    let notes = browser.notes();
    assert_eq!(
        notes
            .iter()
            .map(|(_, note)| note.as_str())
            .collect::<Vec<_>>(),
        vec!["Replay sensor data", "Use test branch", "Tune steering"]
    );
    browser.key(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
        10,
    );
    assert_eq!(
        browser.notes(),
        notes,
        "drilling down retains the entire session's notes"
    );

    let mut second = PtySession::spawn(&root, None)?;
    second.wait_until_started()?;
    second.resize(140, 40)?;
    second.send("h")?;
    // Append two overall notes; older notes stay visible and searchable.
    second.send("nFirst session note\rnSecond session note\r\u{11}")?;
    let (status, raw) = second.finish();
    assert!(status.expect("history session exits").success());
    let screen = String::from_utf8_lossy(&raw);
    for text in ["Granular changes", "Session notes"] {
        assert!(screen.contains(text), "{text}: {screen}");
    }
    let mut browser = taid::history::Browser::open(&root)?;
    let notes = browser.notes();
    assert_eq!(notes[0].1, "Second session note");
    assert_eq!(notes[1].1, "First session note");
    assert_eq!(notes.len(), 5);
    browser.key(
        crossterm::event::KeyCode::Char('/'),
        crossterm::event::KeyModifiers::NONE,
        10,
    );
    browser.paste("First session note");
    assert_eq!(
        browser.visible.len(),
        1,
        "older overall notes remain searchable"
    );
    Ok(())
}

#[test]
fn one_note_documents_three_repositories_saved_together_after_restart() -> TestResult {
    let temp = TempDir::new("pty-three-repository-note")?;
    let root = temp.path().join("super");
    let stack = taid::fixture::make_super(
        &root,
        &[
            SubSpec {
                name: "subA",
                msgs: &["a1", "a2"],
                recorded: None,
                drop_workdir: false,
            },
            SubSpec {
                name: "subB",
                msgs: &["b1", "b2"],
                recorded: None,
                drop_workdir: false,
            },
            SubSpec {
                name: "subC",
                msgs: &["c1", "c2"],
                recorded: None,
                drop_workdir: false,
            },
        ],
    )?;
    let mut session = PtySession::spawn(&root, None)?;
    session.wait_until_started()?;
    // Select an older commit in each submodule, then save once. No n key.
    session.send("j\tj\r\tj\tj\r\tj\tj\rsReplay wet track\r\rqq")?;
    let (status, raw) = session.finish();
    assert!(status.expect("batch save should exit").success(), "{raw:?}");
    let browser = taid::history::Browser::open(&root)?;
    assert_eq!(browser.repository_events.len(), 3);
    assert_eq!(browser.notes().len(), 1);
    assert_eq!(browser.notes()[0].1, "Replay wet track");
    for name in ["subA", "subB", "subC"] {
        assert!(browser.notes()[0].0.contains(name));
        assert_eq!(
            git2::Repository::open(root.join(name))?.head()?.target(),
            Some(stack.sub_oids[name][0])
        );
    }
    let screen = String::from_utf8_lossy(&raw);
    for enable_mouse in ["\x1b[?1000h", "\x1b[?1002h", "\x1b[?1003h", "\x1b[?1006h"] {
        assert!(
            !screen.contains(enable_mouse),
            "mouse capture must never be enabled"
        );
    }
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
    // Ctrl+S/Enter → review/confirm; Esc → files; Ctrl+R, R → repositories; q → exit.
    session.send("\u{3}c\t\r\r2\r\u{13}Config test\r\r\u{1b}\u{12}rq")?;
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
