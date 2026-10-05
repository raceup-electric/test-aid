//! Mutating git operations for taid.
//!
//! Every operation here is deliberately conservative:
//! - checkouts are always SAFE (never `--force`): local changes that would
//!   be overwritten cause a clear error instead of data loss;
//! - moving to an older commit only moves HEAD (detached) — branches and
//!   history are never rewritten, so newer commits stay reachable;
//! - setting a submodule pointer checks the commit out inside the submodule
//!   and stages the gitlink in the superproject's index (the equivalent of
//!   `git add <submodule>`); nothing is committed and nothing leaves the
//!   machine.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use git2::build::CheckoutBuilder;
use git2::{BranchType, ErrorCode, ObjectType, Repository};

use crate::gitx::GITLINK_MODE;

/// A local branch, as listed by [`branches`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchEntry {
    pub name: String,
    /// True when HEAD currently points at this branch.
    pub is_head: bool,
}

/// List the repository's local branches in alphabetical order, with the
/// current branch (if any) marked. Remote-tracking branches are excluded.
pub fn branches(repo: &Repository) -> Result<Vec<BranchEntry>> {
    let mut out = Vec::new();
    for entry in repo.branches(Some(BranchType::Local))? {
        let (branch, _) = entry?;
        out.push(BranchEntry {
            name: branch.name()?.unwrap_or("?").to_string(),
            is_head: branch.is_head(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Switch HEAD to the local branch `name` and update the working directory
/// (like `git checkout <branch>`). Refuses to overwrite uncommitted changes.
pub fn switch_branch(repo: &Repository, name: &str) -> Result<()> {
    let what = format!("branch `{name}`");
    let reference = repo
        .find_branch(name, BranchType::Local)
        .with_context(|| format!("finding local {what}"))?
        .into_reference();
    let oid = reference
        .target()
        .ok_or_else(|| anyhow!("{what} has no target"))?;
    checkout_tree_of(repo, oid, &what)?;
    repo.set_head(&format!("refs/heads/{name}"))
        .with_context(|| format!("moving HEAD to {what}"))?;
    Ok(())
}

/// Move HEAD to `oid` as a detached checkout and update the working
/// directory (like `git checkout --detach <sha>`). Branches are untouched,
/// so newer commits stay reachable.
pub fn checkout_oid(repo: &Repository, oid: git2::Oid) -> Result<()> {
    let what = format!("commit {}", short(oid));
    checkout_tree_of(repo, oid, &what)?;
    repo.set_head_detached(oid)
        .with_context(|| format!("moving HEAD to {what}"))?;
    Ok(())
}

/// Set the commit the superproject records for the submodule at `path`:
/// check `oid` out inside the submodule (detached HEAD, like
/// `git submodule update`) and stage the new gitlink in the superproject's
/// index (like `git add <submodule>`). No commit is created.
pub fn set_submodule_pointer(super_repo: &Repository, path: &str, oid: git2::Oid) -> Result<()> {
    let sm = super_repo
        .find_submodule(path)
        .with_context(|| format!("finding submodule `{path}`"))?;
    let sub = match sm.open() {
        Ok(sub) => sub,
        Err(_) => {
            return Err(anyhow!(
                "submodule `{path}` is not initialized — \
                 run `git submodule update --init {path}` first"
            ));
        }
    };
    checkout_oid(&sub, oid).map_err(|e| e.context(format!("inside submodule `{path}`")))?;

    // Stage the new gitlink (equivalent of `git add <submodule>`).
    let mut index = super_repo.index()?;
    let mut entry = index
        .get_path(Path::new(path), 0)
        .unwrap_or_else(|| git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: GITLINK_MODE as u32,
            uid: 0,
            gid: 0,
            file_size: 0,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
            id: oid,
        });
    entry.mode = GITLINK_MODE as u32;
    entry.id = oid;
    index
        .add(&entry)
        .with_context(|| format!("staging submodule `{path}`"))?;
    index
        .write()
        .with_context(|| format!("staging submodule `{path}`"))?;
    Ok(())
}

/// Shared SAFE checkout used by [`checkout_oid`] and [`switch_branch`]:
/// update workdir and index to `oid`'s tree, never overwriting uncommitted
/// changes. HEAD is left for the caller to move.
fn checkout_tree_of(repo: &Repository, oid: git2::Oid, what: &str) -> Result<()> {
    let obj = repo
        .find_object(oid, Some(ObjectType::Commit))
        .with_context(|| format!("finding {what}"))?;
    let mut opts = CheckoutBuilder::new();
    opts.safe();
    repo.checkout_tree(&obj, Some(&mut opts))
        .map_err(|e| explain_checkout(e, what))?;
    Ok(())
}

/// Turn libgit2's terse checkout errors into actionable messages.
fn explain_checkout(err: git2::Error, what: &str) -> anyhow::Error {
    match err.code() {
        ErrorCode::Conflict => anyhow!(
            "cannot update to {what}: the working tree has local changes \
             that would be overwritten — commit or stash them first"
        ),
        ErrorCode::Exists => anyhow!(
            "cannot update to {what}: an untracked file is in the way — \
             move or delete it first"
        ),
        _ => anyhow!("cannot update to {what}: {err}"),
    }
}

fn short(oid: git2::Oid) -> String {
    oid.to_string()[..7].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, BASE_TIME, SubSpec, TempDir};
    use crate::gitx::{CommitLog, HeadInfo, head_info, is_dirty, overview};
    use std::fs;

    /// A repo whose tracked file changes between commits, so checkouts
    /// actually conflict with local modifications.
    fn varying_history(dir: &std::path::Path) -> Result<(Repository, Vec<git2::Oid>)> {
        let repo = fixture::init_repo(dir, "main")?;
        let oid1 = fixture::commit_file(&repo, "f.txt", "one\n", "c1", BASE_TIME)?;
        let oid2 = fixture::commit_file(&repo, "f.txt", "two\n", "c2", BASE_TIME + 60)?;
        Ok((repo, vec![oid1, oid2]))
    }

    #[test]
    fn checkout_moves_head_without_losing_future_commits() -> Result<()> {
        let temp = TempDir::new("checkout")?;
        let root = temp.path().join("repo");
        let (repo, oids) = fixture::make_history(&root, &["c1", "c2", "c3"])?;

        checkout_oid(&repo, oids[0])?;

        // HEAD detached at the old commit, workdir matches it.
        assert_eq!(head_info(&repo)?, HeadInfo::Detached(oids[0]));
        assert!(root.join("file0.txt").exists());
        assert!(!root.join("file2.txt").exists());
        assert!(!is_dirty(&repo)?);

        // Newer commits are untouched and still reachable via the branch.
        let main_tip = repo
            .find_branch("main", BranchType::Local)?
            .get()
            .target()
            .expect("main has a target");
        assert_eq!(main_tip, oids[2]);
        let mut log = CommitLog::load(&repo, Some(main_tip))?;
        log.ensure_loaded(&repo, usize::MAX)?;
        assert_eq!(log.len(), 3);

        // And we can move forward again.
        checkout_oid(&repo, oids[2])?;
        assert!(root.join("file2.txt").exists());
        Ok(())
    }

    #[test]
    fn checkout_refuses_to_overwrite_local_changes() -> Result<()> {
        let temp = TempDir::new("checkout-conflict")?;
        let root = temp.path().join("repo");
        let (repo, oids) = varying_history(&root)?;

        fs::write(root.join("f.txt"), "local\n")?;
        let err = checkout_oid(&repo, oids[0]).expect_err("must refuse to overwrite");
        let msg = format!("{err:#}");
        assert!(msg.contains("would be overwritten"), "got: {msg}");

        // Nothing happened: still on main at the tip, local change intact.
        assert_eq!(head_info(&repo)?, HeadInfo::Branch("main".into()));
        assert_eq!(repo.head()?.target(), Some(oids[1]));
        assert_eq!(fs::read_to_string(root.join("f.txt"))?, "local\n");

        // Untracked files do not block a checkout.
        fs::write(root.join("f.txt"), "two\n")?;
        fs::write(root.join("untracked.txt"), "x")?;
        checkout_oid(&repo, oids[0])?;
        assert_eq!(fs::read_to_string(root.join("f.txt"))?, "one\n");
        assert!(root.join("untracked.txt").exists());
        Ok(())
    }

    #[test]
    fn checkout_refuses_to_overwrite_staged_changes() -> Result<()> {
        let temp = TempDir::new("checkout-staged-conflict")?;
        let root = temp.path().join("repo");
        let (repo, oids) = varying_history(&root)?;

        fs::write(root.join("f.txt"), "staged\n")?;
        let mut index = repo.index()?;
        index.add_path(Path::new("f.txt"))?;
        index.write()?;
        let staged_oid = index.get_path(Path::new("f.txt"), 0).unwrap().id;

        let err = checkout_oid(&repo, oids[0]).expect_err("must preserve staged changes");
        assert!(format!("{err:#}").contains("would be overwritten"));
        assert_eq!(head_info(&repo)?, HeadInfo::Branch("main".into()));
        assert_eq!(repo.head()?.target(), Some(oids[1]));
        assert_eq!(fs::read_to_string(root.join("f.txt"))?, "staged\n");
        assert_eq!(
            repo.index()?.get_path(Path::new("f.txt"), 0).unwrap().id,
            staged_oid
        );
        Ok(())
    }

    #[test]
    fn set_submodule_pointer_to_older_commit() -> Result<()> {
        let temp = TempDir::new("pointer-old")?;
        let super_root = temp.path().join("super");
        let sup = fixture::make_super(
            &super_root,
            &[SubSpec {
                name: "subA",
                msgs: &["a1", "a2", "a3"],
                recorded: None,
                drop_workdir: false,
            }],
        )?;
        let oids = &sup.sub_oids["subA"];
        let target = oids[0]; // oldest

        set_submodule_pointer(&sup.repo, "subA", target)?;

        // Submodule workdir moved to the old commit, detached and clean.
        let sub = sup.repo.find_submodule("subA")?.open()?;
        assert_eq!(head_info(&sub)?, HeadInfo::Detached(target));
        assert!(super_root.join("subA/file0.txt").exists());
        assert!(!super_root.join("subA/file2.txt").exists());
        assert!(!is_dirty(&sub)?);

        // Superproject staged exactly the old commit...
        let ov = overview(&sup.repo)?;
        let entry = ov.submodules.iter().find(|s| s.name == "subA").unwrap();
        assert_eq!(entry.index_oid, Some(target));
        // ...while HEAD still records the tip (nothing was committed).
        assert_eq!(entry.recorded_oid, Some(oids[2]));
        assert!(
            ov.dirty,
            "staged pointer change should mark the superproject dirty"
        );

        // The submodule's branch and newer commits are untouched.
        let sub_tip = sub
            .find_branch("main", BranchType::Local)?
            .get()
            .target()
            .expect("sub main has a target");
        assert_eq!(sub_tip, oids[2]);
        let mut log = CommitLog::load(&sub, Some(sub_tip))?;
        log.ensure_loaded(&sub, usize::MAX)?;
        assert_eq!(log.len(), 3);
        Ok(())
    }

    #[test]
    fn set_submodule_pointer_refuses_conflicting_dirty_submodule() -> Result<()> {
        let temp = TempDir::new("pointer-conflict")?;
        let super_root = temp.path().join("super");
        let sup = fixture::make_super(
            &super_root,
            &[SubSpec {
                name: "subA",
                msgs: &["a1", "a2", "a3"],
                recorded: None,
                drop_workdir: false,
            }],
        )?;
        let oids = &sup.sub_oids["subA"];

        // Move the submodule one commit past what the superproject records,
        // with a file whose content differs across commits.
        let sub = sup.repo.find_submodule("subA")?.open()?;
        let a4 = fixture::commit_file(&sub, "file0.txt", "changed\n", "a4", BASE_TIME + 500)?;

        // Uncommitted local change that would be overwritten.
        fs::write(super_root.join("subA/file0.txt"), "dirty\n")?;

        let err = set_submodule_pointer(&sup.repo, "subA", oids[0])
            .expect_err("must refuse to overwrite");
        let msg = format!("{err:#}");
        assert!(msg.contains("subA"), "got: {msg}");
        assert!(msg.contains("would be overwritten"), "got: {msg}");

        // Nothing happened anywhere.
        assert_eq!(head_info(&sub)?, HeadInfo::Branch("main".into()));
        assert_eq!(sub.head()?.target(), Some(a4));
        let ov = overview(&sup.repo)?;
        let entry = ov.submodules.iter().find(|s| s.name == "subA").unwrap();
        assert_eq!(entry.index_oid, Some(oids[2]), "index must be untouched");
        assert_eq!(
            fs::read_to_string(super_root.join("subA/file0.txt"))?,
            "dirty\n"
        );
        Ok(())
    }

    #[test]
    fn set_submodule_pointer_uninitialized_is_a_clear_error() -> Result<()> {
        let temp = TempDir::new("pointer-uninit")?;
        let sup = fixture::make_super(
            &temp.path().join("super"),
            &[SubSpec {
                name: "subC",
                msgs: &["c1"],
                recorded: None,
                drop_workdir: true,
            }],
        )?;

        let err = set_submodule_pointer(&sup.repo, "subC", sup.sub_oids["subC"][0])
            .expect_err("uninitialized submodule must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("not initialized"), "got: {msg}");
        assert!(msg.contains("submodule update --init"), "got: {msg}");
        Ok(())
    }

    #[test]
    fn lists_local_branches_with_current_marked() -> Result<()> {
        let temp = TempDir::new("branches")?;
        let root = temp.path().join("repo");
        let (repo, oids) = fixture::make_history(&root, &["c1", "c2"])?;
        let commit0 = repo.find_commit(oids[0])?;
        repo.branch("feature", &commit0, false)?;
        repo.branch("zeta", &commit0, false)?;

        assert_eq!(
            branches(&repo)?,
            vec![
                BranchEntry {
                    name: "feature".into(),
                    is_head: false
                },
                BranchEntry {
                    name: "main".into(),
                    is_head: true
                },
                BranchEntry {
                    name: "zeta".into(),
                    is_head: false
                },
            ]
        );

        // Unborn HEAD: no branches exist yet.
        let fresh = fixture::init_repo(&temp.path().join("fresh"), "main")?;
        assert!(branches(&fresh)?.is_empty());
        Ok(())
    }

    #[test]
    fn switch_branch_moves_head_and_worktree() -> Result<()> {
        let temp = TempDir::new("switch")?;
        let root = temp.path().join("repo");
        let (repo, oids) = fixture::make_history(&root, &["c1", "c2"])?;
        let commit0 = repo.find_commit(oids[0])?;
        repo.branch("feature", &commit0, false)?;

        // Switching to the current branch is a clean no-op.
        switch_branch(&repo, "main")?;
        assert_eq!(head_info(&repo)?, HeadInfo::Branch("main".into()));

        switch_branch(&repo, "feature")?;
        assert_eq!(head_info(&repo)?, HeadInfo::Branch("feature".into()));
        assert!(!root.join("file1.txt").exists());
        assert!(!is_dirty(&repo)?);

        // From a detached HEAD back onto a branch.
        checkout_oid(&repo, oids[0])?;
        assert_eq!(head_info(&repo)?, HeadInfo::Detached(oids[0]));
        switch_branch(&repo, "main")?;
        assert_eq!(head_info(&repo)?, HeadInfo::Branch("main".into()));
        assert!(root.join("file1.txt").exists());
        Ok(())
    }

    #[test]
    fn switch_branch_refuses_to_overwrite_local_changes() -> Result<()> {
        let temp = TempDir::new("switch-conflict")?;
        let root = temp.path().join("repo");
        let repo = fixture::init_repo(&root, "main")?;
        let oid1 = fixture::commit_file(&repo, "f.txt", "one\n", "c1", BASE_TIME)?;
        let commit1 = repo.find_commit(oid1)?;
        repo.branch("feature", &commit1, false)?;
        let _oid2 = fixture::commit_file(&repo, "f.txt", "two\n", "c2", BASE_TIME + 60)?;

        fs::write(root.join("f.txt"), "local\n")?;
        let err = switch_branch(&repo, "feature").expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("would be overwritten"), "got: {msg}");

        assert_eq!(head_info(&repo)?, HeadInfo::Branch("main".into()));
        assert_eq!(fs::read_to_string(root.join("f.txt"))?, "local\n");
        Ok(())
    }
}
