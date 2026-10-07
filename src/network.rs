//! Background remote operations use system Git for the team's SSH aliases,
//! credential helpers and transport configuration. No interactive prompts.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use git2::Repository;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Check,
    Pull,
}

#[derive(Clone)]
pub struct Target {
    pub name: String,
    pub path: Option<PathBuf>,
}

pub enum Event {
    Progress(String),
    Done(Vec<(String, Result<String, String>)>),
}

pub fn start(targets: Vec<Target>, operation: Operation) -> Receiver<Event> {
    start_cancellable(targets, operation).0
}

pub fn start_cancellable(
    targets: Vec<Target>,
    operation: Operation,
) -> (Receiver<Event>, Arc<AtomicBool>) {
    start_recorded(targets, operation, None)
}

pub struct Recording {
    pub root: PathBuf,
    pub session: String,
    pub note: String,
    pub note_group: String,
}

pub fn start_recorded(
    targets: Vec<Target>,
    operation: Operation,
    recording: Option<Recording>,
) -> (Receiver<Event>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let requested = cancel.clone();
    thread::spawn(move || {
        let mut outcomes = Vec::new();
        for target in targets {
            let verb = if operation == Operation::Pull {
                "Pulling"
            } else {
                "Checking"
            };
            if tx
                .send(Event::Progress(format!("{verb} {}…", target.name)))
                .is_err()
            {
                return;
            }
            let action = || {
                if requested.load(Ordering::Relaxed) {
                    Err(anyhow::anyhow!(
                        "cancelled before this repository; no checkout attempted"
                    ))
                } else {
                    target
                .path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("not initialized; initialize this submodule first"))
                .and_then(|path| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| update_cancellable(path, operation, &requested)))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("remote worker failed unexpectedly; inspect this repository before retrying"))))
                }
            };
            let result = if let Some(recording) = &recording {
                let subject = target
                    .path
                    .as_ref()
                    .and_then(|p| p.strip_prefix(&recording.root).ok())
                    .and_then(|p| p.to_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(if target.path.is_none() {
                        &target.name
                    } else {
                        "."
                    });
                crate::repository_history::run_grouped(
                    &recording.root,
                    &recording.session,
                    "pull",
                    subject,
                    &target.name,
                    &crate::repository_history::Annotation {
                        note: &recording.note,
                        group: Some(&recording.note_group),
                    },
                    action,
                )
            } else {
                action()
            }
            .map_err(|e| format!("{e:#}"));
            outcomes.push((target.name, result));
        }
        let _ = tx.send(Event::Done(outcomes));
    });
    (rx, cancel)
}

pub fn update(path: &Path, operation: Operation) -> Result<String> {
    update_cancellable(path, operation, &AtomicBool::new(false))
}

fn update_cancellable(path: &Path, operation: Operation, cancel: &AtomicBool) -> Result<String> {
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled before this repository; no remote operation attempted");
    }
    let repo = Repository::open(path)?;
    if repo
        .workdir()
        .map(Path::canonicalize)
        .transpose()?
        .as_deref()
        != Some(path.canonicalize()?.as_path())
    {
        bail!("target is not the expected Git working checkout; no remote operation attempted");
    }
    if operation == Operation::Pull && repo.state() != git2::RepositoryState::Clean {
        bail!(
            "merge/rebase/cherry-pick or another Git operation is in progress; resolve it outside taid first"
        );
    }
    let initial = repo
        .head()
        .ok()
        .map(|h| (h.name().map(str::to_owned), h.target()));
    if repo.remotes()?.is_empty() {
        return Ok("no remotes configured".into());
    }
    git_timed(
        path,
        &["fetch", "--all", "--prune", "--no-recurse-submodules"],
        Duration::from_secs(60),
        cancel,
    )
    .context("fetch failed; no checkout attempted")?;
    if operation == Operation::Check {
        return Ok("remote references refreshed".into());
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled after fetching; no checkout attempted");
    }
    if repo
        .head()
        .ok()
        .map(|h| (h.name().map(str::to_owned), h.target()))
        != initial
    {
        bail!("HEAD changed during fetch; no checkout attempted — refresh and retry");
    }
    if repo.head_detached()? {
        bail!("fetched; HEAD is detached — select a branch with b before pulling");
    }
    let head = repo.head()?;
    let name = head.shorthand().context("branch has no name")?;
    let branch = repo.find_branch(name, git2::BranchType::Local)?;
    let upstream = branch.upstream().context(
        "fetched; current branch has no upstream; configure it with git branch --set-upstream-to",
    )?;
    let reference = upstream.get().name().context("upstream has no name")?;
    let head_oid = head.target().context("branch has no commits")?;
    let upstream_oid = upstream.get().target().context("upstream has no commits")?;
    let (ahead, behind) = repo.graph_ahead_behind(head_oid, upstream_oid)?;
    if ahead > 0 && behind > 0 {
        bail!(
            "fetched; branch diverged ({ahead} ahead, {behind} behind) — resolve outside taid; no merge/rebase attempted"
        );
    }
    if behind == 0 {
        return Ok(if ahead > 0 {
            format!("ahead by {ahead} commit(s); nothing pulled or pushed")
        } else {
            "already up to date".into()
        });
    }
    // Ignore changed submodule pointers when pulling the superproject; each
    // submodule is updated separately, never recursively checked out.
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).exclude_submodules(true);
    if repo
        .statuses(Some(&mut opts))?
        .iter()
        .any(|s| s.status() != git2::Status::CURRENT)
    {
        bail!("fetched; working tree has local changes — commit or stash before pulling");
    }
    git_timed(
        path,
        &[
            "-c",
            "merge.autoStash=false",
            "merge",
            "--ff-only",
            "--no-edit",
            reference,
        ],
        // Cancellation cannot interrupt a working-tree replacement. Finish the
        // current checkout, then skip remaining targets; a timeout still reports
        // an uncertain outcome and never automatically resets the repository.
        Duration::from_secs(60), &AtomicBool::new(false),
    ).context("fast-forward failed/interrupted; inspect git status before retrying; no automatic rollback")?;
    Ok(format!("updated {name} by {behind} commit(s)"))
}

struct Running {
    child: Child,
    armed: bool,
}
impl Drop for Running {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            if let Ok(mut cleanup) = Command::new("taskkill")
                .args(["/PID", &self.child.id().to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                let end = Instant::now() + Duration::from_secs(2);
                loop {
                    if !matches!(cleanup.try_wait(), Ok(None)) {
                        break;
                    }
                    if Instant::now() >= end {
                        let _ = cleanup.kill();
                        let _ = cleanup.wait();
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn git_timed(path: &Path, args: &[&str], timeout: Duration, cancel: &AtomicBool) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(path)
        .args(["-c", "submodule.recurse=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .context("starting system Git (ensure git is installed)")?;
    run_command(child, timeout, cancel)
}

fn run_command(child: Child, timeout: Duration, cancel: &AtomicBool) -> Result<()> {
    let mut running = Running { child, armed: true };
    let stdout = running
        .child
        .stdout
        .take()
        .context("Git stdout is unavailable")?;
    let stderr = running
        .child
        .stderr
        .take()
        .context("Git stderr is unavailable")?;
    let (tx, rx) = mpsc::channel();
    let read = |mut pipe: Box<dyn Read + Send>| {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut output = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        output.extend_from_slice(&buf[..n]);
                        if output.len() > 65536 {
                            output.drain(..output.len() - 65536);
                        }
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&output).into_owned());
        });
    };
    read(Box::new(stdout));
    read(Box::new(stderr));
    drop(tx);
    let deadline = Instant::now() + timeout;
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("remote operation cancelled");
        }
        if let Some(status) = running.child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            bail!(
                "Git timed out after {} seconds; check remote access and credentials",
                timeout.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    let mut output = String::new();
    let mut received = 0;
    while received < 2 {
        if cancel.load(Ordering::Relaxed) {
            bail!("remote operation cancelled while waiting for a helper");
        }
        if Instant::now() >= deadline {
            bail!(
                "Git output did not close before the timeout; a credential/SSH helper may be stuck"
            );
        }
        match rx.recv_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        ) {
            Ok(text) => {
                output.push_str(&text);
                received += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let output = safe_diagnostic(&output);
    running.armed = false;
    if !status.success() {
        bail!("Git exited with {status}: {}", output.trim());
    }
    Ok(())
}

pub(crate) fn safe_diagnostic(text: &str) -> String {
    let mut result: String = text
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        let mut start = 0;
        while let Some(found) = result[start..].find(scheme) {
            let authority = start + found + scheme.len();
            let end = result[authority..]
                .find(['/', ' ', '\n', '\t'])
                .map(|i| authority + i)
                .unwrap_or(result.len());
            if let Some(at) = result[authority..end].rfind('@') {
                result.replace_range(authority..authority + at, "[redacted]");
                start = authority + "[redacted]@".len();
            } else {
                start = end;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, BASE_TIME, TempDir};
    use crate::gitx;
    use std::fs;

    fn remote_fixture(tag: &str) -> Result<(TempDir, Repository, Repository)> {
        let temp = TempDir::new(tag)?;
        let (remote, _) = fixture::make_history(&temp.path().join("remote"), &["first"])?;
        let local = Repository::clone(
            remote.workdir().unwrap().to_str().unwrap(),
            temp.path().join("local"),
        )?;
        local.config()?.set_str("user.name", "Test")?;
        local.config()?.set_str("user.email", "test@example.com")?;
        Ok((temp, remote, local))
    }

    #[test]
    fn check_fetches_without_checkout_then_pull_fast_forwards() -> Result<()> {
        let (_temp, remote, local) = remote_fixture("pull-ff")?;
        let old = local.head()?.target();
        let latest =
            fixture::commit_file(&remote, "gain.yaml", "gain: 2\n", "tune", BASE_TIME + 60)?;
        update(local.workdir().unwrap(), Operation::Check)?;
        assert_eq!(local.head()?.target(), old);
        assert_eq!(gitx::update_info(&local, None).unwrap().newer, 1);
        update(local.workdir().unwrap(), Operation::Pull)?;
        assert_eq!(local.head()?.target(), Some(latest));
        assert_eq!(gitx::update_info(&local, None).unwrap().newer, 0);
        assert_eq!(
            fs::read_to_string(local.workdir().unwrap().join("gain.yaml"))?,
            "gain: 2\n"
        );
        Ok(())
    }

    #[test]
    fn pull_preserves_dirty_files_and_detached_picks() -> Result<()> {
        let (_temp, remote, local) = remote_fixture("pull-protect")?;
        let old = local.head()?.target().unwrap();
        fixture::commit_file(
            &remote,
            "file0.txt",
            "remote tuning\n",
            "tune",
            BASE_TIME + 60,
        )?;
        let path = local.workdir().unwrap().join("file0.txt");
        fs::write(&path, "local tuning\n")?;
        let error = update(local.workdir().unwrap(), Operation::Pull)
            .unwrap_err()
            .to_string();
        assert!(error.contains("local changes"), "{error}");
        assert_eq!(local.head()?.target(), Some(old));
        assert_eq!(fs::read_to_string(&path)?, "local tuning\n");
        local.set_head_detached(old)?;
        let error = update(local.workdir().unwrap(), Operation::Pull)
            .unwrap_err()
            .to_string();
        assert!(error.contains("detached"), "{error}");
        assert_eq!(local.head()?.target(), Some(old));
        assert_eq!(gitx::update_info(&local, None).unwrap().newer, 1);
        Ok(())
    }

    #[test]
    fn diverged_branches_never_create_a_merge_or_rebase() -> Result<()> {
        let (_temp, remote, local) = remote_fixture("pull-diverged")?;
        let local_tip =
            fixture::commit_file(&local, "local.txt", "local", "local", BASE_TIME + 60)?;
        fixture::commit_file(&remote, "remote.txt", "remote", "remote", BASE_TIME + 120)?;
        assert!(update(local.workdir().unwrap(), Operation::Pull).is_err());
        assert_eq!(local.head()?.target(), Some(local_tip));
        assert_eq!(local.find_commit(local_tip)?.parent_count(), 1);
        assert_eq!(gitx::update_info(&local, None).unwrap().newer, 1);
        Ok(())
    }

    #[test]
    fn worker_continues_after_missing_submodule() -> Result<()> {
        let (_temp, remote, local) = remote_fixture("pull-batch")?;
        let latest = fixture::commit_file(&remote, "new.txt", "new", "new", BASE_TIME + 60)?;
        let rx = start(
            vec![
                Target {
                    name: "missing".into(),
                    path: None,
                },
                Target {
                    name: "local".into(),
                    path: Some(local.workdir().unwrap().to_owned()),
                },
            ],
            Operation::Pull,
        );
        loop {
            if let Event::Done(results) = rx.recv_timeout(Duration::from_secs(10))? {
                assert!(results[0].1.is_err());
                assert!(results[1].1.is_ok());
                break;
            }
        }
        assert_eq!(local.head()?.target(), Some(latest));
        Ok(())
    }

    #[test]
    fn cancellation_reports_every_target_without_changing_checkouts() -> Result<()> {
        let (_temp, remote, local) = remote_fixture("pull-cancel")?;
        let original = local.head()?.target();
        fixture::commit_file(&remote, "new.txt", "new", "new", BASE_TIME + 60)?;
        let cancelled = AtomicBool::new(true);
        assert!(update_cancellable(local.workdir().unwrap(), Operation::Pull, &cancelled).is_err());
        assert_eq!(local.head()?.target(), original);
        // A disconnected channel produces a reportable error, not a stuck busy state.
        let (rx, flag) = start_cancellable(
            vec![
                Target {
                    name: "local".into(),
                    path: Some(local.workdir().unwrap().into()),
                },
                Target {
                    name: "missing".into(),
                    path: None,
                },
            ],
            Operation::Pull,
        );
        flag.store(true, Ordering::Relaxed);
        loop {
            if let Event::Done(results) = rx.recv_timeout(Duration::from_secs(5))? {
                assert_eq!(results.len(), 2);
                assert!(results.iter().all(|(_, r)| r.is_err()));
                break;
            }
        }
        assert_eq!(local.head()?.target(), original);
        Ok(())
    }

    #[test]
    fn invalid_remote_and_existing_merge_are_safe_failures() -> Result<()> {
        let (temp, _remote, local) = remote_fixture("pull-failures")?;
        let before = local.head()?.target();
        local.remote_set_url(
            "origin",
            temp.path().join("does-not-exist").to_str().unwrap(),
        )?;
        let error = update(local.workdir().unwrap(), Operation::Pull)
            .unwrap_err()
            .to_string();
        assert!(error.contains("fetch failed"));
        assert_eq!(local.head()?.target(), before);
        fs::write(
            local.path().join("MERGE_HEAD"),
            format!("{}\n", before.unwrap()),
        )?;
        assert!(
            update(local.workdir().unwrap(), Operation::Pull)
                .unwrap_err()
                .to_string()
                .contains("in progress")
        );
        assert_eq!(local.head()?.target(), before);
        Ok(())
    }

    #[test]
    fn diagnostics_redact_url_credentials_and_terminal_controls() {
        let text = safe_diagnostic(
            "\x1bhttps://secret:password@example.org/repo\nhttp://token@host/path ssh://user:token@host/repo\x07",
        );
        assert!(!text.contains("secret") && !text.contains("password") && !text.contains("token"));
        assert!(!text.contains('\x1b') && !text.contains('\x07'));
        assert!(text.contains("https://[redacted]@example.org/repo"));
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Child {
        use std::os::unix::process::CommandExt;
        Command::new("/bin/sh")
            .args(["-c", script])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn timeouts_and_cancellation_do_not_hang_on_descendant_pipes() {
        for script in ["sleep 5", "(sleep 5) & exit 0"] {
            let started = Instant::now();
            assert!(
                run_command(
                    shell(script),
                    Duration::from_millis(200),
                    &AtomicBool::new(false)
                )
                .is_err()
            );
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "descendant pipes must not extend the deadline"
            );
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let error =
            run_command(shell("(sleep 5) & exit 0"), Duration::from_secs(5), &cancel).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn failed_commands_bound_output_memory_and_report_exit_status() {
        let error = run_command(
            shell("yes abc | head -c 262144; exit 7"),
            Duration::from_secs(3),
            &AtomicBool::new(false),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("7"));
        assert!(error.len() < 66000, "diagnostic output is bounded");
    }
}
