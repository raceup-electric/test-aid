//! Read-only git model for taid.
//!
//! This module answers "what does the repository look like right now?":
//! where it is, what HEAD points at, whether it is dirty, which submodules
//! it has (and what each side records for them), and how to walk a commit
//! history lazily. Nothing here mutates the repository — the operations
//! land in step 3.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use git2::{ErrorCode, Repository, Sort, Status, StatusOptions, Time};

/// File mode of a submodule ("gitlink") entry in a tree.
pub(crate) const GITLINK_MODE: i32 = 0o160000;

/// Discover the repository that contains `start`, walking up like git does.
///
/// Returns `Ok(None)` when `start` is not inside any repository, so callers
/// can tell "not a repo" (a UX state) apart from real failures.
pub fn discover(start: &Path) -> Result<Option<Repository>> {
    match Repository::discover(start) {
        Ok(repo) => Ok(Some(repo)),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(anyhow!(
            "looking for a git repository under {}: {e}",
            start.display()
        )),
    }
}

/// What HEAD points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadInfo {
    /// HEAD is on a local branch.
    Branch(String),
    /// HEAD is detached at a commit.
    Detached(git2::Oid),
    /// HEAD points at a branch that has no commits yet (fresh repository).
    Unborn(String),
}

impl HeadInfo {
    /// Short human-readable form for headers and list rows.
    pub fn describe(&self) -> String {
        match self {
            HeadInfo::Branch(name) => name.clone(),
            HeadInfo::Detached(oid) => format!("{} (detached)", &oid.to_string()[..7]),
            HeadInfo::Unborn(name) => format!("{name} (no commits yet)"),
        }
    }
}

/// Branch name, detached oid, or unborn branch of `repo`'s HEAD.
pub fn head_info(repo: &Repository) -> Result<HeadInfo> {
    match repo.head() {
        Ok(head) => {
            if head.is_branch() {
                Ok(HeadInfo::Branch(
                    head.shorthand().unwrap_or("HEAD").to_string(),
                ))
            } else {
                let oid = head
                    .target()
                    .ok_or_else(|| anyhow!("HEAD is detached but has no target"))?;
                Ok(HeadInfo::Detached(oid))
            }
        }
        Err(e) if e.code() == ErrorCode::UnbornBranch => {
            let name = repo
                .find_reference("HEAD")
                .ok()
                .and_then(|r| r.symbolic_target().map(str::to_string))
                .and_then(|t| t.rsplit('/').next().map(str::to_string))
                .unwrap_or_else(|| "HEAD".to_string());
            Ok(HeadInfo::Unborn(name))
        }
        Err(e) => Err(anyhow!("reading HEAD: {e}")),
    }
}

/// True when the worktree or index differs from HEAD (untracked files count).
pub fn is_dirty(repo: &Repository) -> Result<bool> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts))?;
    Ok(statuses
        .iter()
        .any(|entry| entry.status() != Status::CURRENT))
}

/// One submodule of the superproject, as taid sees it.
#[allow(dead_code)] // every field is consumed by the TUI from step 5
#[derive(Debug, Clone)]
pub struct SubmoduleEntry {
    /// Name from `.gitmodules` (`[submodule "<name>"]`).
    pub name: String,
    /// Path relative to the superproject root.
    pub path: String,
    /// True when a checkout exists on disk and can be opened.
    pub initialized: bool,
    /// Absolute path of the submodule's working directory, when initialized.
    pub workdir: Option<PathBuf>,
    /// Commit the superproject's HEAD tree records for this submodule.
    pub recorded_oid: Option<git2::Oid>,
    /// Commit currently staged in the superproject's index.
    pub index_oid: Option<git2::Oid>,
    /// Commit the submodule's own HEAD is at
    /// (`None` if not initialized or without commits).
    pub workdir_oid: Option<git2::Oid>,
    /// The submodule's HEAD state (`None` if not initialized).
    pub head: Option<HeadInfo>,
    /// Whether the submodule's own working tree is dirty (`None` if not initialized).
    pub dirty: Option<bool>,
}

/// Snapshot of everything the UI needs about the current repository.
#[derive(Debug)]
pub struct RepoOverview {
    /// Absolute path of the repository's working directory.
    pub root: PathBuf,
    pub head: HeadInfo,
    /// Commit HEAD points at (`None` when HEAD is unborn).
    pub head_oid: Option<git2::Oid>,
    pub dirty: bool,
    pub submodules: Vec<SubmoduleEntry>,
}

/// Collect the full read-only snapshot of `repo`.
pub fn overview(repo: &Repository) -> Result<RepoOverview> {
    let root = repo
        .workdir()
        .ok_or_else(|| anyhow!("bare repositories are not supported"))?
        .to_path_buf();
    let head = head_info(repo)?;
    let head_oid = repo.head().ok().and_then(|h| h.target());
    let dirty = is_dirty(repo)?;
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let index = repo.index().ok();

    let mut submodules = Vec::new();
    for sm in repo.submodules().context("listing submodules")? {
        let path = sm
            .path()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        let recorded_oid = head_tree
            .as_ref()
            .and_then(|t| t.get_path(Path::new(&path)).ok())
            .filter(|e| e.filemode() == GITLINK_MODE)
            .map(|e| e.id());
        let index_oid = index
            .as_ref()
            .and_then(|idx| idx.get_path(Path::new(&path), 0).map(|e| e.id));

        // A submodule "exists" for us when its checkout is on disk and opens.
        let workdir_path = root.join(&path);
        let opened = if workdir_path.join(".git").exists() {
            sm.open().ok()
        } else {
            None
        };
        let initialized = opened.is_some();
        let (head, workdir_oid, dirty) = match &opened {
            Some(sub) => (
                head_info(sub).ok(),
                sub.head().ok().and_then(|h| h.target()),
                is_dirty(sub).ok(),
            ),
            None => (None, None, None),
        };

        submodules.push(SubmoduleEntry {
            name: sm
                .name()
                .map(str::to_string)
                .unwrap_or_else(|| path.clone()),
            path,
            initialized,
            workdir: initialized.then_some(workdir_path),
            recorded_oid,
            index_oid,
            workdir_oid,
            head,
            dirty,
        });
    }

    Ok(RepoOverview {
        root,
        head,
        head_oid,
        dirty,
        submodules,
    })
}

/// A single commit, ready for display.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub oid: git2::Oid,
    /// Abbreviated hash, as git would shorten it.
    pub short_sha: String,
    /// First line of the commit message.
    pub title: String,
    pub author_name: String,
    /// Author time: unix seconds plus the original UTC offset in minutes.
    pub time: Time,
}

impl CommitInfo {
    /// `YYYY-MM-DD HH:MM` in the commit's own timezone.
    pub fn time_str(&self) -> String {
        let offset = chrono::FixedOffset::east_opt(self.time.offset_minutes() * 60)
            .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).expect("zero offset is valid"));
        chrono::DateTime::from_timestamp(self.time.seconds(), 0)
            .map(|dt| {
                dt.with_timezone(&offset)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "?".to_string())
    }
}

/// A commit history, newest first, parsed lazily.
///
/// All object ids are collected up front (cheap); `CommitInfo` rows are
/// materialized only as far as the UI has scrolled.
#[derive(Debug)]
pub struct CommitLog {
    oids: Vec<git2::Oid>,
    commits: Vec<CommitInfo>,
}

impl CommitLog {
    /// Load the history reachable from `tip`, newest first. Pass `None` for
    /// a repository without commits (e.g. HEAD is unborn) to get an empty log.
    pub fn load(repo: &Repository, tip: Option<git2::Oid>) -> Result<Self> {
        let Some(tip) = tip else {
            return Ok(Self {
                oids: Vec::new(),
                commits: Vec::new(),
            });
        };
        let mut walk = repo.revwalk()?;
        walk.push(tip)?;
        walk.set_sorting(Sort::TIME)?;
        let mut oids = Vec::new();
        for oid in walk {
            oids.push(oid?);
        }
        Ok(Self {
            oids,
            commits: Vec::new(),
        })
    }

    /// Total number of commits in the loaded history.
    pub fn len(&self) -> usize {
        self.oids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.oids.is_empty()
    }

    /// Materialize commits `[0, up_to)` (clamped to the history length) and
    /// return how many are materialized now.
    pub fn ensure_loaded(&mut self, repo: &Repository, up_to: usize) -> Result<usize> {
        let target = up_to.min(self.oids.len());
        while self.commits.len() < target {
            let oid = self.oids[self.commits.len()];
            let commit = repo.find_commit(oid)?;
            self.commits.push(CommitInfo {
                oid,
                short_sha: short_sha(&commit)?,
                title: commit
                    .summary()
                    .unwrap_or("(no summary)")
                    .trim()
                    .to_string(),
                author_name: commit.author().name().unwrap_or("").to_string(),
                time: commit.time(),
            });
        }
        Ok(self.commits.len())
    }

    pub fn get(&self, index: usize) -> Option<&CommitInfo> {
        self.commits.get(index)
    }
}

fn short_sha(commit: &git2::Commit) -> Result<String> {
    let buf = commit.as_object().short_id()?;
    Ok(buf.as_str().map(str::to_string).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, BASE_TIME, SubSpec, TempDir};
    use std::fs;

    fn sub<'a>(ov: &'a RepoOverview, name: &str) -> &'a SubmoduleEntry {
        ov.submodules
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("submodule {name} not found"))
    }

    #[test]
    fn discovers_repository_from_subdirectory() -> Result<()> {
        let temp = TempDir::new("discover")?;
        let (_repo, _oids) = fixture::make_history(&temp.path().join("repo"), &["one", "two"])?;
        let nested = temp.path().join("repo/deep/inside");
        fs::create_dir_all(&nested)?;

        let found = discover(&nested)?.expect("repo should be discovered from a subdirectory");
        let got = found.workdir().expect("discovered repo has a workdir");
        assert_eq!(
            got.canonicalize()?,
            temp.path().join("repo").canonicalize()?
        );

        let outside = TempDir::new("discover-outside")?;
        assert!(discover(outside.path())?.is_none());
        Ok(())
    }

    #[test]
    fn head_info_reports_branch_detached_and_unborn() -> Result<()> {
        let temp = TempDir::new("head-info")?;
        let (repo, oids) = fixture::make_history(&temp.path().join("repo"), &["one", "two"])?;
        assert_eq!(head_info(&repo)?, HeadInfo::Branch("main".into()));
        repo.set_head_detached(oids[0])?;
        assert_eq!(head_info(&repo)?, HeadInfo::Detached(oids[0]));

        let fresh = fixture::init_repo(&temp.path().join("fresh"), "main")?;
        assert_eq!(head_info(&fresh)?, HeadInfo::Unborn("main".into()));
        Ok(())
    }

    #[test]
    fn dirty_flag_tracks_worktree_changes() -> Result<()> {
        let temp = TempDir::new("dirty")?;
        let root = temp.path().join("repo");
        let (repo, _) = fixture::make_history(&root, &["one", "two"])?;

        assert!(!is_dirty(&repo)?);
        fs::write(root.join("untracked.txt"), "x")?;
        assert!(is_dirty(&repo)?);
        fs::remove_file(root.join("untracked.txt"))?;
        assert!(!is_dirty(&repo)?);
        fs::write(root.join("file0.txt"), "changed")?;
        assert!(is_dirty(&repo)?);
        Ok(())
    }

    #[test]
    fn enumerates_submodules_with_oids_and_state() -> Result<()> {
        let temp = TempDir::new("submodules")?;
        let super_root = temp.path().join("super");
        let sup = fixture::make_super(
            &super_root,
            &[
                SubSpec {
                    name: "subA",
                    msgs: &["a1", "a2", "a3"],
                    recorded: None,
                    drop_workdir: false,
                },
                SubSpec {
                    name: "subB",
                    msgs: &["b1", "b2"],
                    recorded: Some(0),
                    drop_workdir: false,
                },
                SubSpec {
                    name: "subC",
                    msgs: &["c1"],
                    recorded: None,
                    drop_workdir: true,
                },
            ],
        )?;

        let ov = overview(&sup.repo)?;
        assert_eq!(ov.root.canonicalize()?, super_root.canonicalize()?);
        assert_eq!(ov.submodules.len(), 3);

        // subA: initialized, clean, recorded == workdir
        let a = sub(&ov, "subA");
        assert_eq!(a.path, "subA");
        assert!(a.initialized);
        assert_eq!(a.head.as_ref(), Some(&HeadInfo::Branch("main".into())));
        let a_tip = sup.sub_oids["subA"][2];
        assert_eq!(a.recorded_oid, Some(a_tip));
        assert_eq!(a.index_oid, Some(a_tip));
        assert_eq!(a.workdir_oid, Some(a_tip));
        assert_eq!(a.dirty, Some(false));
        assert!(a.workdir.as_deref().is_some());

        // subB: workdir is ahead of the recorded pointer
        let b = sub(&ov, "subB");
        let (b0, b1) = (sup.sub_oids["subB"][0], sup.sub_oids["subB"][1]);
        assert!(b.initialized);
        assert_eq!(b.recorded_oid, Some(b0));
        assert_eq!(b.index_oid, Some(b0));
        assert_eq!(b.workdir_oid, Some(b1));
        assert_eq!(b.dirty, Some(false));

        // subC: recorded but never initialized
        let c = sub(&ov, "subC");
        assert!(!c.initialized);
        assert_eq!(c.recorded_oid, Some(sup.sub_oids["subC"][0]));
        assert_eq!(c.workdir_oid, None);
        assert!(c.head.is_none());
        assert_eq!(c.dirty, None);
        assert!(c.workdir.is_none());

        // subB's drift and subC's missing checkout make the superproject dirty
        assert!(ov.dirty);

        // an untracked file inside subA marks it dirty
        fs::write(super_root.join("subA/untracked.txt"), "x")?;
        let ov2 = overview(&sup.repo)?;
        assert_eq!(sub(&ov2, "subA").dirty, Some(true));
        Ok(())
    }

    #[test]
    fn commit_log_lists_history_newest_first() -> Result<()> {
        let temp = TempDir::new("log")?;
        let (repo, oids) =
            fixture::make_history(&temp.path().join("repo"), &["c1", "c2", "c3", "c4", "c5"])?;
        let tip = repo.head()?.target().expect("head has a target");

        let mut log = CommitLog::load(&repo, Some(tip))?;
        assert_eq!(log.len(), 5);
        assert!(!log.is_empty());

        log.ensure_loaded(&repo, 2)?;
        let first = log.get(0).expect("row 0");
        assert_eq!(first.oid, oids[4]);
        assert_eq!(first.title, "c5");
        assert_eq!(first.author_name, "T. Ester");
        assert_eq!(first.time.seconds(), BASE_TIME + 4 * 3600);
        assert_eq!(first.time.offset_minutes(), 60);
        assert!(oids[4].to_string().starts_with(&first.short_sha));
        assert!(log.get(2).is_none(), "row 2 not materialized yet");

        log.ensure_loaded(&repo, usize::MAX)?;
        assert_eq!(log.get(4).map(|c| c.title.as_str()), Some("c1"));
        assert_eq!(log.get(4).map(|c| c.oid), Some(oids[0]));
        Ok(())
    }

    #[test]
    fn commit_log_is_empty_without_commits() -> Result<()> {
        let temp = TempDir::new("log-empty")?;
        let repo = fixture::init_repo(&temp.path().join("fresh"), "main")?;
        let mut log = CommitLog::load(&repo, None)?;
        assert_eq!(log.len(), 0);
        assert!(log.is_empty());
        assert_eq!(log.ensure_loaded(&repo, 10)?, 0);
        assert!(log.get(0).is_none());
        Ok(())
    }

    #[test]
    fn time_str_uses_commit_timezone() {
        let info = CommitInfo {
            oid: git2::Oid::zero(),
            short_sha: "0000000".to_string(),
            title: "t".to_string(),
            author_name: "a".to_string(),
            time: git2::Time::new(1_700_000_000, 60),
        };
        // 2023-11-14 22:13:20 UTC, shown at +01:00
        assert_eq!(info.time_str(), "2023-11-14 23:13");
    }
}
