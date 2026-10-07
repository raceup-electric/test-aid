//! Programmatic git fixtures for unit tests: real repositories built with
//! the git2 API only — no git CLI, no network.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use git2::{IndexEntry, IndexTime, Repository, RepositoryInitOptions, Signature, Time};

/// Deterministic base for fixture commit times (2023-11-14 22:13:20 UTC).
pub const BASE_TIME: i64 = 1_700_000_000;

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Document a test action using the same note input as a user, then press Enter.
#[cfg(test)]
pub(crate) fn confirm_action(app: &mut crate::app::App) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let needs_note = if let Some(review) = &app.batch_review {
        review.outcome.is_none() && review.note.trim().is_empty()
    } else if let Some(form) = &app.form {
        matches!(&form.overlay, Some(crate::config::Overlay::Save { note, .. }) if note.trim().is_empty())
    } else {
        app.action_note.trim().is_empty()
            && (app
                .branch_popup
                .as_ref()
                .is_some_and(|p| !p.branches.is_empty())
                || matches!(&app.save_popup, Some(crate::app::SaveStage::Confirm(changes)) if !changes.is_empty())
                || app.popup == Some(crate::app::Popup::PullConfirm))
    };
    if needs_note {
        let editing = app.action_note_editing
            || app.batch_review.as_ref().is_some_and(|r| r.note_editing)
            || app.form.as_ref().is_some_and(|f| {
                matches!(
                    &f.overlay,
                    Some(crate::config::Overlay::Save {
                        note_editing: true,
                        ..
                    })
                )
            });
        if !editing {
            app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        }
        app.handle_paste("Test change");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
}

#[cfg(test)]
pub(crate) fn confirm_restore(
    review: &mut crate::session::Review,
    root: &Path,
    session: &str,
) -> Result<()> {
    if review.note.is_empty() {
        review.note = "Test restore".into();
    }
    review.confirm(root, session)
}

#[cfg(test)]
pub(crate) fn confirm_form(
    form: &mut crate::config::Form,
    modifiers: crossterm::event::KeyModifiers,
    page: usize,
) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    if matches!(&form.overlay, Some(crate::config::Overlay::Save { note, .. }) if note.trim().is_empty())
    {
        if matches!(
            &form.overlay,
            Some(crate::config::Overlay::Save {
                note_editing: false,
                ..
            })
        ) {
            form.overlay_key(KeyCode::Char('n'), KeyModifiers::NONE, page);
        }
        form.paste("Test config change");
        form.overlay_key(KeyCode::Enter, KeyModifiers::NONE, page);
    }
    form.overlay_key(KeyCode::Enter, modifiers, page)
}

/// A unique temporary directory, removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> Result<Self> {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("taid-test-{tag}-{}-{n}", std::process::id()));
        if path.exists() {
            fs::remove_dir_all(&path).with_context(|| format!("cleaning {}", path.display()))?;
        }
        fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Signature with a fixed identity and explicit time (+01:00).
pub fn sig_at(time: i64) -> Result<Signature<'static>> {
    Signature::new("T. Ester", "tester@example.com", &Time::new(time, 60))
        .context("creating fixture signature")
}

/// Init a non-bare repository at `dir` with the given initial branch.
pub fn init_repo(dir: &Path, branch: &str) -> Result<Repository> {
    let mut opts = RepositoryInitOptions::new();
    opts.bare(false)
        .initial_head(&format!("refs/heads/{branch}"));
    Repository::init_opts(dir, &opts).with_context(|| format!("initializing {}", dir.display()))
}

/// Write `path` in the worktree, stage it and commit it on HEAD.
///
/// Keeps the index in sync with HEAD, so the repo ends up clean.
pub fn commit_file(
    repo: &Repository,
    path: &str,
    content: &str,
    msg: &str,
    time: i64,
) -> Result<git2::Oid> {
    let workdir = repo
        .workdir()
        .context("repository has no workdir")?
        .to_path_buf();
    let file = workdir.join(path);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&file, content)?;

    let mut index = repo.index()?;
    index.add_path(Path::new(path))?;
    index.write()?;
    let tree = repo.find_tree(index.write_tree()?)?;

    let sig = sig_at(time)?;
    let parents: Vec<git2::Commit> = match repo.head() {
        Ok(head) => vec![head.peel_to_commit()?],
        Err(_) => Vec::new(),
    };
    let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
    Ok(repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parent_refs)?)
}

/// Create a repository at `dir` with one commit per message, each adding
/// `file{i}.txt`. Returns the repo and its commit oids, oldest first.
pub fn make_history(dir: &Path, msgs: &[&str]) -> Result<(Repository, Vec<git2::Oid>)> {
    let repo = init_repo(dir, "main")?;
    let mut oids = Vec::new();
    for (i, msg) in msgs.iter().enumerate() {
        let path = format!("file{i}.txt");
        let content = format!("content of {path}\n");
        oids.push(commit_file(
            &repo,
            &path,
            &content,
            msg,
            BASE_TIME + (i as i64) * 3600,
        )?);
    }
    Ok((repo, oids))
}

/// Spec for one submodule inside a [`make_super`] fixture.
pub struct SubSpec<'a> {
    pub name: &'a str,
    /// History of the submodule, oldest first (must be non-empty).
    pub msgs: &'a [&'a str],
    /// Index into `msgs` of the commit the superproject records
    /// (`None` = the newest).
    pub recorded: Option<usize>,
    /// Remove the submodule's working directory after recording it,
    /// simulating a submodule that was never initialized.
    pub drop_workdir: bool,
}

/// A built superproject fixture.
pub struct SuperFixture {
    pub repo: Repository,
    /// Commit oids of each submodule's history, oldest first.
    pub sub_oids: BTreeMap<String, Vec<git2::Oid>>,
}

/// Build a superproject at `dir`: a root commit, then a commit adding
/// `.gitmodules` and one gitlink per submodule, each pointing at the
/// commit selected by `spec.recorded`.
pub fn make_super(dir: &Path, subs: &[SubSpec]) -> Result<SuperFixture> {
    let repo = init_repo(dir, "main")?;
    commit_file(&repo, "README.md", "# super\n", "root", BASE_TIME)?;

    let mut gitmodules = String::new();
    let mut sub_oids: BTreeMap<String, Vec<git2::Oid>> = BTreeMap::new();

    let mut index = repo.index()?;
    for spec in subs {
        // Build the submodule's own history inside the superproject workdir.
        let (_sub_repo, oids) = make_history(&dir.join(spec.name), spec.msgs)?;
        sub_oids.insert(spec.name.to_string(), oids.clone());
        let recorded = match spec.recorded {
            Some(i) => oids[i],
            None => *oids.last().expect("at least one commit"),
        };

        let n = spec.name;
        gitmodules.push_str(&format!(
            "[submodule \"{n}\"]\n\tpath = {n}\n\turl = https://example.invalid/{n}.git\n\n"
        ));
        index.add(&IndexEntry {
            ctime: IndexTime::new(0, 0),
            mtime: IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160000,
            uid: 0,
            gid: 0,
            file_size: 0,
            flags: 0,
            flags_extended: 0,
            path: spec.name.as_bytes().to_vec(),
            id: recorded,
        })?;
    }
    if !gitmodules.is_empty() {
        fs::write(dir.join(".gitmodules"), &gitmodules)?;
        index.add_path(Path::new(".gitmodules"))?;
    }
    index.write()?;
    {
        let tree = repo.find_tree(index.write_tree()?)?;
        let root = repo.head()?.peel_to_commit()?;
        let sig = sig_at(BASE_TIME + 100)?;
        repo.commit(Some("HEAD"), &sig, &sig, "Add submodules", &tree, &[&root])?;
    }

    // Mark the submodules as initialized in .git/config (`git submodule init`).
    for spec in subs {
        repo.find_submodule(spec.name)
            .with_context(|| format!("finding submodule {}", spec.name))?
            .init(true)?;
    }
    // Optionally drop the checkouts to simulate uninitialized submodules.
    for spec in subs {
        if spec.drop_workdir {
            fs::remove_dir_all(dir.join(spec.name))?;
        }
    }
    Ok(SuperFixture { repo, sub_oids })
}
