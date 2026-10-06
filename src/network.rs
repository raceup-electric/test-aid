//! Background remote operations use system Git for the team's SSH aliases,
//! credential helpers and transport configuration. No interactive prompts.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use git2::Repository;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Check,
    Pull,
}

pub struct Target {
    pub name: String,
    pub path: Option<PathBuf>,
}

pub enum Event {
    Progress(String),
    Done(Vec<(String, Result<String, String>)>),
}

pub fn start(targets: Vec<Target>, operation: Operation) -> Receiver<Event> {
    let (tx, rx) = mpsc::channel();
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
            let result = target
                .path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("not initialized; initialize this submodule first"))
                .and_then(|path| update(path, operation))
                .map_err(|e| format!("{e:#}"));
            outcomes.push((target.name, result));
        }
        let _ = tx.send(Event::Done(outcomes));
    });
    rx
}

pub fn update(path: &Path, operation: Operation) -> Result<String> {
    let repo = Repository::open(path)?;
    if repo.remotes()?.is_empty() {
        return Ok("no remotes configured".into());
    }
    git(
        path,
        &["fetch", "--all", "--prune", "--no-recurse-submodules"],
    )?;
    if operation == Operation::Check {
        return Ok("remote references refreshed".into());
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
    let (_, behind) = repo.graph_ahead_behind(head_oid, upstream_oid)?;
    if behind == 0 {
        return Ok("already up to date".into());
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
    git(
        path,
        &[
            "-c",
            "merge.autoStash=false",
            "merge",
            "--ff-only",
            "--no-edit",
            reference,
        ],
    )?;
    Ok(format!("updated {name} by {behind} commit(s)"))
}

fn git(path: &Path, args: &[&str]) -> Result<()> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["-c", "submodule.recurse=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting system Git (ensure git is installed)")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let read = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut output = String::new();
            let _ = pipe.read_to_string(&mut output);
            output
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Git timed out after 60 seconds; check remote access and credentials");
        }
        thread::sleep(Duration::from_millis(50));
    };
    let output = format!(
        "{}{}",
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default()
    );
    if !status.success() {
        bail!("{}", output.trim());
    }
    Ok(())
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
}
