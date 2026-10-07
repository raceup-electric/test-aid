//! Private write-ahead audit of repository operations. Never creates Git objects/refs.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use git2::{ObjectType, Oid, Repository};
use serde::{Deserialize, Serialize};

use crate::history::{Change, hash, history_dir, private_dir};
use crate::session::{identifier, read, write};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub branch: Option<String>,
    pub commit: Option<String>,
}
impl Head {
    pub fn describe(&self) -> String {
        format!(
            "{} @ {}",
            self.branch.as_deref().unwrap_or("detached"),
            self.commit.as_deref().unwrap_or("unborn")
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub module: String,
    pub head: Head,
    pub links: BTreeMap<String, String>,
    pub local_refs: BTreeMap<String, String>,
    pub remote_refs: BTreeMap<String, String>,
    pub regular_index: String,
    pub dirty_paths: Vec<String>,
    pub staged_paths: Vec<String>,
    pub git_busy: bool,
}
impl State {
    pub fn checkout_eq(&self, other: &Self) -> bool {
        self.head == other.head && self.links == other.links
    }
}

fn relative(path: &str) -> Result<()> {
    if path.is_empty()
        || !Path::new(path)
            .components()
            .all(|c| matches!(c,Component::Normal(p) if p!=".git"))
    {
        bail!("unsafe repository-relative path");
    }
    Ok(())
}

pub fn open(root: &Path, module: &str) -> Result<Repository> {
    let mut path = root.canonicalize()?;
    if module != "." {
        relative(module)?;
        for part in Path::new(module).components() {
            path.push(part);
            let m = fs::symlink_metadata(&path)?;
            if !m.is_dir() || m.file_type().is_symlink() {
                bail!("repository path was removed/replaced by a symlink");
            }
        }
    }
    let repo = Repository::open(&path)?;
    if repo
        .workdir()
        .map(Path::canonicalize)
        .transpose()?
        .as_deref()
        != Some(path.as_path())
    {
        bail!("not the expected Git working checkout");
    }
    Ok(repo)
}

pub fn capture(root: &Path, module: &str) -> Result<State> {
    let repo = open(root, module)?;
    let head = repo.head().ok();
    let detached = repo.head_detached().unwrap_or(false);
    let branch = if detached {
        None
    } else {
        head.as_ref()
            .and_then(|h| h.shorthand().map(str::to_owned))
            .or_else(|| {
                repo.find_reference("HEAD").ok().and_then(|h| {
                    h.symbolic_target()
                        .and_then(|s| s.strip_prefix("refs/heads/"))
                        .map(str::to_owned)
                })
            })
    };
    let commit = head
        .as_ref()
        .and_then(|h| h.target())
        .map(|id| id.to_string());
    let tree = head.as_ref().and_then(|h| h.peel_to_tree().ok());
    let index = repo.index()?;
    let mut links = BTreeMap::new();
    let mut regular = Vec::new();
    for e in index.iter() {
        let path = std::str::from_utf8(&e.path).context("non-UTF-8 index path")?;
        if e.mode == crate::gitx::GITLINK_MODE as u32 && (e.flags >> 12) & 3 == 0 {
            relative(path)?;
            links.insert(path.to_owned(), e.id.to_string());
        } else {
            regular.push((
                path.to_owned(),
                e.mode,
                e.id.to_string(),
                (e.flags >> 12) & 3,
            ));
        }
    }
    let mut dirty_paths = Vec::new();
    let mut staged_paths = Vec::new();
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(false);
    for s in repo.statuses(Some(&mut opts))?.iter() {
        let path = s.path().context("non-UTF-8 status path")?;
        let indexed = index.get_path(Path::new(path), 0);
        let head_link = tree
            .as_ref()
            .and_then(|t| t.get_path(Path::new(path)).ok())
            .is_some_and(|e| e.filemode() == crate::gitx::GITLINK_MODE);
        let index_link = indexed
            .as_ref()
            .is_some_and(|e| e.mode == crate::gitx::GITLINK_MODE as u32);
        if (head_link && (index_link || indexed.is_none()))
            || (index_link
                && tree
                    .as_ref()
                    .is_none_or(|t| t.get_path(Path::new(path)).is_err()))
        {
            continue;
        }
        if s.status() != git2::Status::CURRENT {
            dirty_paths.push(path.into());
        }
        if s.status().intersects(
            git2::Status::INDEX_NEW
                | git2::Status::INDEX_MODIFIED
                | git2::Status::INDEX_DELETED
                | git2::Status::INDEX_RENAMED
                | git2::Status::INDEX_TYPECHANGE
                | git2::Status::CONFLICTED,
        ) {
            staged_paths.push(path.into());
        }
    }
    let mut local_refs = BTreeMap::new();
    let mut remote_refs = BTreeMap::new();
    for reference in repo.references()? {
        let r = reference?;
        if let (Some(name), Some(id)) = (r.name(), r.target()) {
            if name.starts_with("refs/heads/") {
                local_refs.insert(name.into(), id.to_string());
            }
            if name.starts_with("refs/remotes/") {
                remote_refs.insert(name.into(), id.to_string());
            }
        }
    }
    Ok(State {
        module: module.into(),
        head: Head { branch, commit },
        links,
        local_refs,
        remote_refs,
        regular_index: hash(&serde_json::to_string(&regular)?)?,
        dirty_paths,
        staged_paths,
        git_busy: repo.state() != git2::RepositoryState::Clean || index.has_conflicts(),
    })
}

fn stack(root: &Path, previous: &[State]) -> Result<Vec<State>> {
    let root = root.canonicalize()?;
    let mut modules = BTreeSet::from([".".to_owned()]);
    let mut queue = vec![".".to_owned()];
    while let Some(module) = queue.pop() {
        let repo = open(&root, &module)?;
        for sub in repo.submodules()? {
            if let Ok(child) = sub.open() {
                if let Some(dir) = child.workdir() {
                    let path = dir
                        .canonicalize()?
                        .strip_prefix(&root)?
                        .to_str()
                        .context("non-UTF-8 submodule path")?
                        .to_owned();
                    if modules.insert(path.clone()) {
                        queue.push(path);
                    }
                }
            }
        }
    }
    // Retain initialized modules which a superproject checkout just unregistered.
    modules.extend(previous.iter().map(|s| s.module.clone()));
    modules
        .into_iter()
        .map(|module| capture(&root, &module))
        .collect()
}

pub(crate) fn metadata_lock(dir: &Path) -> Result<File> {
    let path = dir.join("checkout.lock");
    if let Ok(m) = fs::symlink_metadata(&path) {
        if !m.is_file() || m.file_type().is_symlink() {
            bail!("checkout lock was replaced");
        }
    }
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path)?;
    file.try_lock_exclusive()
        .context("another taid operation is using this checkout; retry after it finishes")?;
    Ok(file)
}

pub fn workspace_lock(root: &Path) -> Result<File> {
    metadata_lock(history_dir(root)?.parent().unwrap())
}

fn base(root: &Path) -> Result<PathBuf> {
    let dir = history_dir(root)?
        .parent()
        .unwrap()
        .join("repository-history");
    private_dir(&dir)?;
    Ok(dir)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Intent {
    pub version: u32,
    pub id: String,
    pub session: String,
    pub timestamp: i64,
    pub kind: String,
    pub subject: String,
    pub requested: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// One confirmation may apply to multiple repositories. Never group by text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_group: Option<String>,
    pub before: Vec<State>,
}
pub struct Annotation<'a> {
    pub note: &'a str,
    pub group: Option<&'a str>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub after: Vec<State>,
    pub success: bool,
    pub detail: String,
}
pub struct Event {
    pub intent: Intent,
    pub outcome: Option<Outcome>,
    pub status: String,
    file: PathBuf,
}
pub struct Journal {
    dir: PathBuf,
    pub intent: Intent,
    _active: File,
    _workspace: Option<File>,
}

impl Journal {
    pub fn begin(
        root: &Path,
        session: &str,
        kind: &str,
        subject: &str,
        requested: &str,
    ) -> Result<Self> {
        Self::begin_noted(root, session, kind, subject, requested, "")
    }

    pub fn begin_noted(
        root: &Path,
        session: &str,
        kind: &str,
        subject: &str,
        requested: &str,
        note: &str,
    ) -> Result<Self> {
        Self::begin_grouped(
            root,
            session,
            kind,
            subject,
            requested,
            &Annotation { note, group: None },
        )
    }

    pub fn begin_grouped(
        root: &Path,
        session: &str,
        kind: &str,
        subject: &str,
        requested: &str,
        annotation: &Annotation<'_>,
    ) -> Result<Self> {
        let workspace = workspace_lock(root)?;
        let mut journal =
            Self::begin_grouped_locked(root, session, kind, subject, requested, annotation)?;
        journal._workspace = Some(workspace);
        Ok(journal)
    }

    fn begin_grouped_locked(
        root: &Path,
        session: &str,
        kind: &str,
        subject: &str,
        requested: &str,
        annotation: &Annotation<'_>,
    ) -> Result<Self> {
        let note = annotation.note;
        crate::session::validate_note(note)?;
        if let Some(group) = annotation.group {
            crate::session::valid_id(group)?;
        }
        let id = identifier()?;
        let before = stack(root, &[])
            .context("cannot capture the repository baseline; no Git action started")?;
        let intent = Intent {
            version: 1,
            id: id.clone(),
            session: session.into(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64,
            kind: kind.into(),
            subject: subject.into(),
            requested: requested.into(),
            note: note.into(),
            note_group: annotation.group.map(str::to_owned),
            before,
        };
        let dir = base(root)?.join(&id);
        private_dir(&dir)?;
        let active = crate::session::lock(&dir.join("active.lock"), true)?;
        write(&dir.join("intent.json"), &intent)
            .context("repository journal unavailable; no Git action started")?;
        Ok(Self {
            dir,
            intent,
            _active: active,
            _workspace: None,
        })
    }
    pub fn finish(&self, root: &Path, result: &Result<String>) -> Result<()> {
        let after=stack(root,&self.intent.before).context("Git action may have completed but final state could not be captured; intent retained, inspect checkout before retrying")?;
        let detail = match result {
            Ok(msg) => msg.clone(),
            Err(e) => format!("{e:#}"),
        };
        let outcome = Outcome {
            after,
            success: result.is_ok(),
            detail: crate::network::safe_diagnostic(&detail)
                .chars()
                .take(4000)
                .collect(),
        };
        write(&self.dir.join("outcome.json"),&outcome).context("Git action may have completed but final history could not be synced; intent retained, inspect history/status before retrying")
    }
}

pub fn run(
    root: &Path,
    session: &str,
    kind: &str,
    subject: &str,
    requested: &str,
    action: impl FnOnce() -> Result<String>,
) -> Result<String> {
    run_noted(root, session, kind, subject, requested, "", action)
}

pub fn run_noted(
    root: &Path,
    session: &str,
    kind: &str,
    subject: &str,
    requested: &str,
    note: &str,
    action: impl FnOnce() -> Result<String>,
) -> Result<String> {
    run_grouped(
        root,
        session,
        kind,
        subject,
        requested,
        &Annotation { note, group: None },
        action,
    )
}

pub fn run_grouped(
    root: &Path,
    session: &str,
    kind: &str,
    subject: &str,
    requested: &str,
    annotation: &Annotation<'_>,
    action: impl FnOnce() -> Result<String>,
) -> Result<String> {
    let journal = Journal::begin_grouped(root, session, kind, subject, requested, annotation)?;
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)).unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "repository operation failed unexpectedly; inspect current state"
            ))
        });
    journal.finish(root, &result)?;
    result
}

fn read_event(dir: &Path) -> Result<Event> {
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("repository journal directory was replaced");
    }
    let intent: Intent = read(&dir.join("intent.json"))?;
    if intent.version != 1 || Some(intent.id.as_str()) != dir.file_name().and_then(|s| s.to_str()) {
        bail!("repository journal identity/version mismatch");
    }
    crate::session::valid_id(&intent.id)?;
    crate::session::valid_id(&intent.session)?;
    if let Some(group) = &intent.note_group {
        crate::session::valid_id(group)?;
    }
    let (outcome, status) = if dir.join("outcome.json").exists() {
        let value: Outcome = read(&dir.join("outcome.json"))?;
        let status = if value.success {
            "completed"
        } else {
            "failed / possibly partial"
        }
        .into();
        (Some(value), status)
    } else {
        let status = match crate::session::lock(&dir.join("active.lock"), false) {
            Ok(_) => "interrupted / outcome unknown",
            Err(e) if crate::session::contended(&e) => "active",
            Err(e) => return Err(e),
        };
        (None, status.into())
    };
    for state in intent
        .before
        .iter()
        .chain(outcome.iter().flat_map(|o| o.after.iter()))
    {
        validate(state)?;
    }
    Ok(Event {
        intent,
        outcome,
        status,
        file: dir.join("intent.json"),
    })
}

pub fn events(root: &Path) -> Result<(Vec<Event>, Vec<String>)> {
    let mut records = Vec::new();
    let mut warnings = Vec::new();
    for dir in fs::read_dir(base(root)?)? {
        let dir = dir?.path();
        let result = read_event(&dir);
        match result {
            Ok(event) => {
                if event.outcome.is_none() {
                    warnings.push(format!(
                        "Repository action {}: {} — no automatic rollback",
                        event.intent.id, event.status
                    ));
                }
                records.push(event);
            }
            Err(e) => warnings.push(format!(
                "Repository journal retained, restore blocked: {}: {e:#}",
                dir.display()
            )),
        }
    }
    records.sort_by(|a, b| b.intent.id.cmp(&a.intent.id));
    Ok((records, warnings))
}
fn validate(state: &State) -> Result<()> {
    if state.module != "." {
        relative(&state.module)?;
    }
    if let Some(commit) = &state.head.commit {
        let _: Oid = commit.parse()?;
    }
    if let Some(branch) = &state.head.branch {
        if !git2::Reference::is_valid_name(&format!("refs/heads/{branch}")) {
            bail!("invalid historical branch name");
        }
    }
    for (path, id) in &state.links {
        relative(path)?;
        let _: Oid = id.parse()?;
    }
    Ok(())
}

pub fn verify(root: &Path, event: &Event) -> Result<Event> {
    crate::session::valid_id(&event.intent.id)?;
    if event.file.parent() != Some(base(root)?.join(&event.intent.id).as_path()) {
        bail!("event outside checkout journal");
    }
    read_event(event.file.parent().context("event has no directory")?)
}

pub fn changes(before: &State, after: &State, refs: bool) -> Vec<Change> {
    let mut result = Vec::new();
    let mut add = |name: String, b: String, a: String| {
        if b != a {
            result.push(Change {
                name: format!("{} · {name}", before.module),
                before: b,
                after: a,
            });
        }
    };
    add(
        "branch / checkout commit".into(),
        before.head.describe(),
        after.head.describe(),
    );
    for path in before
        .links
        .keys()
        .chain(after.links.keys())
        .collect::<BTreeSet<_>>()
    {
        add(
            format!("staged gitlink {path}"),
            before
                .links
                .get(path)
                .cloned()
                .unwrap_or_else(|| "absent".into()),
            after
                .links
                .get(path)
                .cloned()
                .unwrap_or_else(|| "absent".into()),
        );
    }
    if refs {
        for (name, b, a) in [
            (
                "local branch tip (context; never rewound)",
                &before.local_refs,
                &after.local_refs,
            ),
            (
                "fetched ref (context; never rewound)",
                &before.remote_refs,
                &after.remote_refs,
            ),
        ] {
            for path in b.keys().chain(a.keys()).collect::<BTreeSet<_>>() {
                add(
                    format!("{name} {path}"),
                    b.get(path).cloned().unwrap_or_else(|| "absent".into()),
                    a.get(path).cloned().unwrap_or_else(|| "absent".into()),
                );
            }
        }
    }
    result
}

/// Presentation only: journal snapshots and restore targets keep full object IDs.
pub fn display_changes(root: &Path, before: &State, after: &State, refs: bool) -> Vec<Change> {
    let mut result = changes(before, after, refs);
    for change in &mut result {
        let name = change
            .name
            .strip_prefix(&format!("{} · ", before.module))
            .unwrap_or("");
        if name == "branch / checkout commit" {
            change.before = display_head(root, &before.module, &before.head);
            change.after = display_head(root, &after.module, &after.head);
        } else {
            // A gitlink's object belongs to the submodule, not its parent repository.
            let module = if let Some(path) = name.strip_prefix("staged gitlink ") {
                if before.module == "." {
                    path.to_owned()
                } else {
                    format!("{}/{path}", before.module)
                }
            } else {
                before.module.clone()
            };
            change.before = display_commit_text(root, &module, &change.before);
            change.after = display_commit_text(root, &module, &change.after);
        }
    }
    result
}

pub fn display_head(root: &Path, module: &str, head: &Head) -> String {
    format!(
        "{} @ {}",
        head.branch.as_deref().unwrap_or("detached"),
        display_commit_text(root, module, head.commit.as_deref().unwrap_or("unborn"))
    )
}

/// Replace full commit IDs in action requests/results with a short ID and title.
/// Missing/pruned objects still show their short ID without breaking history.
pub fn display_commit_text(root: &Path, module: &str, text: &str) -> String {
    let repo = open(root, module).ok();
    let mut chars = text.chars().peekable();
    let mut output = String::new();
    while let Some(c) = chars.next() {
        if !c.is_ascii_hexdigit() {
            output.push(c);
            continue;
        }
        let mut token = String::from(c);
        while chars.peek().is_some_and(char::is_ascii_hexdigit) {
            token.push(chars.next().unwrap());
        }
        if token.len() == 40 {
            let title = token
                .parse::<Oid>()
                .ok()
                .and_then(|oid| repo.as_ref()?.find_commit(oid).ok())
                .map(|commit| {
                    commit
                        .summary()
                        .filter(|title| !title.is_empty())
                        .unwrap_or("(no title)")
                        .to_owned()
                })
                .unwrap_or_else(|| "(title unavailable)".into());
            output.push_str(&format!("{} · {title}", &token[..7]));
        } else {
            output.push_str(&token);
        }
    }
    output
}

#[derive(Clone)]
pub struct Target {
    pub before: State,
    pub after: Option<State>,
    pub source: String,
    pub uncertain: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct RepoPlan {
    pub before: State,
    pub after: State,
    pub source: String,
}

pub fn prepare(root: &Path, target: &Target, before: bool) -> Result<(RepoPlan, Vec<String>)> {
    let desired = if before {
        &target.before
    } else {
        target.after.as_ref().context(
            "final repository state was not recorded; only the before state can be reviewed",
        )?
    };
    validate(desired)?;
    let current = capture(root, &desired.module)?;
    let repo = open(root, &desired.module)?;
    let mut after = desired.clone();
    let mut warnings = Vec::new();
    if target.uncertain {
        warnings.push(format!(
            "{}: failed/interrupted action; inspect the recorded and current states",
            desired.module
        ));
    }
    let expected = if before {
        target.after.as_ref()
    } else {
        Some(&target.before)
    };
    if expected.is_none_or(|state| !current.checkout_eq(state)) && !current.checkout_eq(desired) {
        warnings.push(format!("{}: later/external checkout or staged-pointer changes will be replaced only on confirmation",desired.module));
    }
    if let Some(branch) = &after.head.branch {
        if repo
            .find_branch(branch, git2::BranchType::Local)
            .ok()
            .and_then(|b| b.get().target())
            .map(|id| id.to_string())
            != after.head.commit
        {
            warnings.push(format!("{}: `{branch}` moved or was deleted; restore will detach at {} and leave all branch tips untouched",desired.module,after.head.commit.as_deref().unwrap_or("unborn")));
            after.head.branch = None;
        }
    }
    let plan = RepoPlan {
        before: current,
        after,
        source: target.source.clone(),
    };
    preflight(root, &plan)?;
    Ok((plan, warnings))
}

pub fn preflight(root: &Path, plan: &RepoPlan) -> Result<()> {
    let current = capture(root, &plan.before.module)?;
    if current != plan.before {
        bail!(
            "{} changed after review; reopen preview before restoring",
            current.module
        );
    }
    if current.git_busy {
        bail!(
            "{} has conflicts or an active Git operation; resolve it outside taid first",
            current.module
        );
    }
    if current.checkout_eq(&plan.after) {
        return Ok(());
    }
    let repo = open(root, &current.module)?;
    if current.head != plan.after.head {
        if current.head.commit != plan.after.head.commit
            && (!current.dirty_paths.is_empty() || !current.staged_paths.is_empty())
        {
            bail!(
                "{} has dirty/staged tracked files ({}). Restore config files separately first, or commit/stash outside taid; no work is discarded",
                current.module,
                current.dirty_paths.join(", ")
            );
        }
        let id: Oid = plan
            .after
            .head
            .commit
            .as_ref()
            .context("cannot restore an unborn checkout without rewriting references")?
            .parse()?;
        let object = repo
            .find_object(id, Some(ObjectType::Commit))
            .context("historical commit is no longer available locally; nothing restored")?;
        check_untracked(&repo, &object.peel_to_tree()?)?;
        let tree = object.peel_to_tree()?;
        for path in plan.after.links.keys() {
            if tree
                .get_path(Path::new(path))
                .ok()
                .is_some_and(|e| e.filemode() != crate::gitx::GITLINK_MODE)
            {
                bail!(
                    "gitlink restoration would replace a regular entry in the target revision at {path}"
                );
            }
        }
        if let Some(branch) = &plan.after.head.branch {
            if repo
                .find_branch(branch, git2::BranchType::Local)?
                .get()
                .target()
                != Some(id)
            {
                bail!("target branch moved after preview; retry to review a detached checkout");
            }
        }
        let mut opts = git2::build::CheckoutBuilder::new();
        opts.safe().dry_run();
        if let Err(e) = repo.checkout_tree(&object, Some(&mut opts)) {
            // An explicitly reviewed staged gitlink can be normalized before
            // a checkout. Never excuse regular-file or untracked collisions.
            if e.code() != git2::ErrorCode::Conflict
                || current.links == tree_links(&repo, current.head.commit.as_deref())?
            {
                return Err(e.into());
            }
            check_untracked(&repo, &object.peel_to_tree()?)?;
        }
    }
    for path in plan.after.links.keys() {
        relative(path)?;
        if repo
            .index()?
            .get_path(Path::new(path), 0)
            .is_some_and(|e| e.mode != crate::gitx::GITLINK_MODE as u32)
            && current.head == plan.after.head
        {
            bail!("gitlink restoration would replace a non-submodule index entry at {path}");
        }
    }
    Ok(())
}

fn check_untracked(repo: &Repository, tree: &git2::Tree<'_>) -> Result<()> {
    fn walk(
        repo: &Repository,
        tree: &git2::Tree<'_>,
        prefix: &Path,
        index: &git2::Index,
    ) -> Result<()> {
        for entry in tree.iter() {
            let path = prefix.join(entry.name().context("non-UTF-8 target tree path")?);
            if entry.kind() == Some(ObjectType::Tree) {
                walk(repo, &repo.find_tree(entry.id())?, &path, index)?;
            } else if entry.filemode() != crate::gitx::GITLINK_MODE
                && index.get_path(&path, 0).is_none()
                && fs::symlink_metadata(repo.workdir().unwrap().join(&path)).is_ok()
            {
                bail!("untracked path would be overwritten: {}", path.display());
            }
        }
        Ok(())
    }
    walk(repo, tree, Path::new(""), &repo.index()?)
}

fn tree_links(repo: &Repository, commit: Option<&str>) -> Result<BTreeMap<String, String>> {
    let mut links = BTreeMap::new();
    if let Some(commit) = commit {
        let tree = repo.find_commit(commit.parse()?)?.tree()?;
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.filemode() == crate::gitx::GITLINK_MODE {
                if let Some(name) = entry.name() {
                    links.insert(format!("{root}{name}"), entry.id().to_string());
                }
            }
            git2::TreeWalkResult::Ok
        })?;
    }
    Ok(links)
}
fn stage_links(repo: &Repository, links: &BTreeMap<String, String>) -> Result<()> {
    let mut index = repo.index()?;
    let old: Vec<_> = index
        .iter()
        .filter(|e| e.mode == crate::gitx::GITLINK_MODE as u32)
        .map(|e| e.path.clone())
        .collect();
    for path in old {
        let path = Path::new(std::str::from_utf8(&path)?);
        if !links.contains_key(path.to_str().unwrap()) {
            index.remove_path(path)?;
        }
    }
    for (path, id) in links {
        let mut e = index
            .get_path(Path::new(path), 0)
            .unwrap_or_else(|| git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: crate::gitx::GITLINK_MODE as u32,
                uid: 0,
                gid: 0,
                file_size: 0,
                id: Oid::zero(),
                flags: 0,
                flags_extended: 0,
                path: path.as_bytes().to_vec(),
            });
        if e.mode != crate::gitx::GITLINK_MODE as u32 {
            bail!("refusing to replace non-gitlink index entry at {path}");
        }
        e.id = id.parse()?;
        index.add(&e)?;
    }
    index.write()?;
    Ok(())
}

pub(crate) fn apply(
    root: &Path,
    session: &str,
    plan: &RepoPlan,
    annotation: &Annotation<'_>,
) -> Result<()> {
    let current = capture(root, &plan.before.module)?;
    // Parent checkout may have changed gitlinks, but never this module's HEAD.
    if current.head != plan.before.head
        || current.regular_index != plan.before.regular_index
        || current.dirty_paths != plan.before.dirty_paths
        || current.git_busy
        || current.local_refs != plan.before.local_refs
        || current.remote_refs != plan.before.remote_refs
    {
        bail!(
            "{} drifted during restore; stopped without forcing checkout",
            current.module
        );
    }
    let repo = open(root, &current.module)?;
    let journal = Journal::begin_grouped_locked(
        root,
        session,
        "restore repository",
        &current.module,
        &plan.source,
        annotation,
    )?;
    let result = (|| -> Result<String> {
        if current.head != plan.after.head {
            stage_links(&repo, &tree_links(&repo, current.head.commit.as_deref())?)?;
            if let Some(branch) = &plan.after.head.branch {
                let id = plan
                    .after
                    .head
                    .commit
                    .as_ref()
                    .context("unborn restore unsupported")?
                    .parse()?;
                if repo
                    .find_branch(branch, git2::BranchType::Local)?
                    .get()
                    .target()
                    != Some(id)
                {
                    bail!("branch moved during restore; no reference was rewound");
                }
                crate::gitops::switch_branch(&repo, branch)?;
            } else {
                crate::gitops::checkout_oid(
                    &repo,
                    plan.after
                        .head
                        .commit
                        .as_ref()
                        .context("unborn restore unsupported")?
                        .parse()?,
                )?;
            }
        }
        stage_links(&repo, &plan.after.links)?;
        if !capture(root, &current.module)?.checkout_eq(&plan.after) {
            bail!(
                "restored checkout/index does not match the reviewed target; inspect current state before retrying"
            );
        }
        Ok(format!(
            "restored checkout/index for {} without rewriting refs",
            current.module
        ))
    })();
    journal.finish(root, &result)?;
    result.map(|_| ())
}

/// Predict the tracked YAML after a planned checkout; untracked configs persist.
pub fn projected_file(
    root: &Path,
    relative_file: &str,
    plans: &[RepoPlan],
    current: &str,
) -> Result<String> {
    let owner = plans
        .iter()
        .filter(|p| {
            p.before.module == "." || relative_file.starts_with(&format!("{}/", p.before.module))
        })
        .max_by_key(|p| p.before.module.len());
    let Some(plan) = owner else {
        return Ok(current.into());
    };
    if plan.before.head.commit == plan.after.head.commit {
        return Ok(current.into());
    }
    let repo = open(root, &plan.before.module)?;
    let local = if plan.before.module == "." {
        relative_file
    } else {
        relative_file
            .strip_prefix(&format!("{}/", plan.before.module))
            .unwrap()
    };
    match committed_file(&repo, plan.after.head.commit.as_deref(), local)? {
        Some(text) => Ok(text),
        None if repo.index()?.get_path(Path::new(local), 0).is_none() => Ok(current.into()),
        None => bail!(
            "{relative_file} is absent at the target commit; restore configs/repositories separately"
        ),
    }
}
pub fn committed_file(
    repo: &Repository,
    commit: Option<&str>,
    path: &str,
) -> Result<Option<String>> {
    let Some(commit) = commit else {
        return Ok(None);
    };
    let tree = repo.find_commit(commit.parse()?)?.tree()?;
    let entry = match tree.get_path(Path::new(path)) {
        Ok(entry) => entry,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !matches!(entry.filemode(), 0o100644 | 0o100755) {
        bail!("historical config is not a regular file");
    }
    let blob = repo.find_blob(entry.id())?;
    if blob.size() > 2 * 1024 * 1024 {
        bail!("historical config exceeds 2 MiB");
    }
    Ok(Some(std::str::from_utf8(blob.content())?.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fixture::{self, SubSpec, TempDir},
        history::{Browser, HistoryRow},
        session::{Review, Session},
    };
    use crossterm::event::{KeyCode, KeyModifiers};
    use std::time::SystemTime;

    fn checkout(root: &Path, session: &Session, repo: &Repository, id: Oid) -> Result<()> {
        run(
            root,
            &session.id,
            "commit pick",
            ".",
            &id.to_string(),
            || {
                crate::gitops::checkout_oid(repo, id)?;
                Ok("checkout complete".into())
            },
        )?;
        Ok(())
    }

    fn review(root: &Path, browser: &Browser, before: bool) -> Result<Review> {
        Ok(Review::prepare_project(
            root,
            browser.targets()?,
            browser.repository_targets()?,
            before,
            "restore".into(),
            browser.recovery_id(),
        ))
    }

    #[test]
    fn reload_of_empty_history_discovers_the_first_repository_only_session() -> Result<()> {
        let temp = TempDir::new("repository-first-reload")?;
        let (repo, ids) = fixture::make_history(temp.path(), &["one", "two"])?;
        let mut browser = Browser::open(temp.path())?;
        assert!(browser.visible.is_empty());
        let session = Session::begin(temp.path(), SystemTime::now())?;
        checkout(temp.path(), &session, &repo, ids[0])?;
        browser.reload()?;
        assert_eq!(browser.visible.len(), 1);
        assert!(matches!(
            browser.selected_row(),
            Some(HistoryRow::Session(_, _))
        ));
        Ok(())
    }

    #[test]
    fn branch_only_session_is_visible_searchable_describable_and_restorable() -> Result<()> {
        let temp = TempDir::new("repository-branch")?;
        let (repo, ids) = fixture::make_history(temp.path(), &["one", "two"])?;
        repo.branch("feature", &repo.find_commit(ids[0])?, false)?;
        let session = Session::begin(temp.path(), SystemTime::now())?;
        run(
            temp.path(),
            &session.id,
            "branch switch",
            ".",
            "feature",
            || {
                crate::gitops::switch_branch(&repo, "feature")?;
                Ok("branch selected".into())
            },
        )?;
        let mut browser = Browser::open(temp.path())?;
        assert_eq!(browser.visible.len(), 1);
        assert!(browser.entries.is_empty());
        assert!(browser.selected_label().contains("1 repository actions"));
        assert!(
            browser
                .comparison()?
                .iter()
                .any(|c| c.before.contains("main") && c.after.contains("feature"))
        );
        browser.key(KeyCode::Char('n'), KeyModifiers::NONE, 10);
        browser.paste("branch experiment");
        browser.key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert!(browser.selected_label().contains("branch experiment"));
        browser.key(KeyCode::Char('/'), KeyModifiers::NONE, 10);
        browser.paste(&ids[1].to_string());
        assert_eq!(browser.visible.len(), 1, "search includes before SHA");
        browser.key(KeyCode::Esc, KeyModifiers::NONE, 10);
        browser.key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert!(matches!(
            browser.selected_row(),
            Some(HistoryRow::Repository(_))
        ));
        let mut restore = review(temp.path(), &browser, true)?;
        assert!(restore.errors.is_empty(), "{:?}", restore.errors);
        assert_eq!(
            repo.head()?.shorthand(),
            Some("feature"),
            "preview writes nothing"
        );
        let newer = Session::begin(temp.path(), SystemTime::now())?;
        crate::fixture::confirm_restore(&mut restore, temp.path(), &newer.id)?;
        assert_eq!(repo.head()?.shorthand(), Some("main"));
        assert_eq!(repo.head()?.target(), Some(ids[1]));
        assert_eq!(
            events(temp.path())?.0.len(),
            2,
            "restoration is itself audited"
        );
        Ok(())
    }

    #[test]
    fn advanced_or_deleted_branch_restores_detached_without_rewinding_any_refs() -> Result<()> {
        for delete in [false, true] {
            let temp = TempDir::new("repository-moved")?;
            let (repo, ids) = fixture::make_history(temp.path(), &["one", "two"])?;
            let session = Session::begin(temp.path(), SystemTime::now())?;
            checkout(temp.path(), &session, &repo, ids[0])?;
            if delete {
                repo.find_branch("main", git2::BranchType::Local)?
                    .delete()?;
            } else {
                repo.find_reference("refs/heads/main")?
                    .set_target(ids[0], "external move")?;
            }
            let refs = capture(temp.path(), ".")?.local_refs;
            let browser = Browser::open(temp.path())?;
            let mut restore = review(temp.path(), &browser, true)?;
            assert!(restore.errors.is_empty(), "{:?}", restore.errors);
            assert!(restore.warnings.iter().any(|w| w.contains("will detach")));
            crate::fixture::confirm_restore(&mut restore, temp.path(), &session.id)?;
            assert!(repo.head_detached()?);
            assert_eq!(repo.head()?.target(), Some(ids[1]));
            assert_eq!(capture(temp.path(), ".")?.local_refs, refs);
        }
        Ok(())
    }

    #[test]
    fn submodule_pick_diff_distinguishes_checkout_and_staged_parent_pointer() -> Result<()> {
        let temp = TempDir::new("repository-gitlink")?;
        let stack = fixture::make_super(
            temp.path(),
            &[SubSpec {
                name: "sub",
                msgs: &["one", "two"],
                recorded: None,
                drop_workdir: false,
            }],
        )?;
        let session = Session::begin(temp.path(), SystemTime::now())?;
        let ids = &stack.sub_oids["sub"];
        run(
            temp.path(),
            &session.id,
            "commit pick",
            "sub",
            &ids[0].to_string(),
            || {
                crate::gitops::set_submodule_pointer(&stack.repo, "sub", ids[0])?;
                Ok("pointer staged".into())
            },
        )?;
        let browser = Browser::open(temp.path())?;
        let changes = browser.comparison()?;
        assert!(
            changes
                .iter()
                .any(|c| c.name.contains("staged gitlink sub"))
        );
        assert!(
            changes
                .iter()
                .any(|c| c.name.contains("sub · branch / checkout commit"))
        );
        let shown = browser.display_comparison()?;
        let pointer = shown
            .iter()
            .find(|c| c.name.contains("staged gitlink sub"))
            .unwrap();
        assert_eq!(
            pointer.before,
            format!("{} · two", &ids[1].to_string()[..7])
        );
        assert_eq!(pointer.after, format!("{} · one", &ids[0].to_string()[..7]));
        let checkout = shown
            .iter()
            .find(|c| c.name.contains("sub · branch / checkout commit"))
            .unwrap();
        assert_eq!(
            checkout.after,
            format!("detached @ {} · one", &ids[0].to_string()[..7])
        );
        let action = shown
            .iter()
            .find(|c| c.before.starts_with("Requested:"))
            .unwrap();
        assert_eq!(
            action.before,
            format!("Requested: {} · one", &ids[0].to_string()[..7])
        );
        assert!(
            changes.iter().any(|c| c.after == ids[0].to_string()),
            "audit comparisons keep full IDs"
        );
        let mut restore = review(temp.path(), &browser, true)?;
        assert!(restore.errors.is_empty(), "{:?}", restore.errors);
        crate::fixture::confirm_restore(&mut restore, temp.path(), &session.id)?;
        assert!(
            restore
                .outcome
                .as_ref()
                .unwrap()
                .contains("2 repository state(s)"),
            "{:?}",
            restore.outcome
        );
        assert_eq!(
            open(temp.path(), ".")?
                .index()?
                .get_path(Path::new("sub"), 0)
                .unwrap()
                .id,
            ids[1]
        );
        assert_eq!(open(temp.path(), "sub")?.head()?.target(), Some(ids[1]));
        assert!(!open(temp.path(), "sub")?.head_detached()?);
        Ok(())
    }

    #[test]
    fn commit_display_preserves_surrounding_text_and_handles_unavailable_objects() -> Result<()> {
        let temp = TempDir::new("repository-display")?;
        let (_, ids) = fixture::make_history(temp.path(), &["Trial title λ"])?;
        let full = ids[0].to_string();
        assert_eq!(
            display_commit_text(temp.path(), ".", &format!("Applied commit {full}!")),
            format!("Applied commit {} · Trial title λ!", &full[..7])
        );
        assert_eq!(
            display_commit_text(temp.path(), "missing", &full),
            format!("{} · (title unavailable)", &full[..7])
        );
        assert_eq!(
            display_commit_text(temp.path(), ".", &"1".repeat(40)),
            "1111111 · (title unavailable)"
        );
        for text in ["absent", "unborn", "normal text", "λ", "abc123", ""] {
            assert_eq!(display_commit_text(temp.path(), ".", text), text);
        }
        assert_eq!(
            display_commit_text(temp.path(), ".", &format!("{full}a")),
            format!("{full}a"),
            "longer hex tokens are not commit IDs"
        );
        Ok(())
    }

    #[test]
    fn failures_partial_outcomes_and_crashed_intents_are_retained() -> Result<()> {
        let temp = TempDir::new("repository-partial")?;
        let (repo, ids) = fixture::make_history(temp.path(), &["one", "two"])?;
        let session = Session::begin(temp.path(), SystemTime::now())?;
        assert!(
            run(temp.path(), &session.id, "pull", ".", "all", || {
                crate::gitops::checkout_oid(&repo, ids[0])?;
                bail!("failure after checkout https://user:secret@example.org/repo\u{1b}[31m")
            })
            .is_err()
        );
        let (records, _) = events(temp.path())?;
        assert!(!records[0].outcome.as_ref().unwrap().success);
        assert_eq!(
            records[0].outcome.as_ref().unwrap().after[0].head.commit,
            Some(ids[0].to_string())
        );
        assert!(
            !records[0]
                .outcome
                .as_ref()
                .unwrap()
                .detail
                .contains("secret")
        );
        let intent = Journal::begin(temp.path(), &session.id, "branch switch", ".", "main")?;
        assert!(events(temp.path())?.0.iter().any(|e| e.status == "active"));
        drop(intent);
        let browser = Browser::open(temp.path())?;
        assert!(
            browser
                .warnings
                .iter()
                .any(|w| w.contains("outcome unknown"))
        );
        assert!(
            browser
                .comparison()?
                .iter()
                .any(|c| c.after.contains("outcome unknown"))
        );
        let blocked = review(temp.path(), &browser, false)?;
        assert!(!blocked.errors.is_empty());
        let before = review(temp.path(), &browser, true)?;
        assert!(before.errors.is_empty(), "{:?}", before.errors);
        Ok(())
    }

    #[test]
    fn checkout_restore_blocks_dirty_staged_untracked_busy_missing_commits_and_drift() -> Result<()>
    {
        let temp = TempDir::new("repository-safe")?;
        let (repo, ids) = fixture::make_history(temp.path(), &["one", "two"])?;
        let session = Session::begin(temp.path(), SystemTime::now())?;
        checkout(temp.path(), &session, &repo, ids[0])?;
        let browser = Browser::open(temp.path())?;
        fs::write(temp.path().join("file0.txt"), "local edits")?;
        assert!(
            review(temp.path(), &browser, true)?
                .errors
                .iter()
                .any(|e| e.contains("dirty/staged"))
        );
        let mut index = repo.index()?;
        index.add_path(Path::new("file0.txt"))?;
        index.write()?;
        assert!(!review(temp.path(), &browser, true)?.errors.is_empty());
        index.read_tree(&repo.find_commit(ids[0])?.tree()?)?;
        index.write()?;
        fs::write(temp.path().join("file0.txt"), "content of file0.txt\n")?;
        fs::write(temp.path().join("file1.txt"), "untracked collision")?;
        assert!(!review(temp.path(), &browser, true)?.errors.is_empty());
        assert_eq!(
            fs::read_to_string(temp.path().join("file1.txt"))?,
            "untracked collision"
        );
        fs::remove_file(temp.path().join("file1.txt"))?;
        fs::write(repo.path().join("MERGE_HEAD"), ids[1].to_string())?;
        assert!(!review(temp.path(), &browser, true)?.errors.is_empty());
        fs::remove_file(repo.path().join("MERGE_HEAD"))?;
        let mut restore = review(temp.path(), &browser, true)?;
        assert!(restore.errors.is_empty(), "{:?}", restore.errors);
        repo.reference("refs/remotes/example/main", ids[1], true, "external update")?;
        assert!(crate::fixture::confirm_restore(&mut restore, temp.path(), &session.id).is_err());
        assert_eq!(repo.head()?.target(), Some(ids[0]));
        let mut target = browser.repository_targets()?.remove(0);
        target.before.head.commit = Some("1111111111111111111111111111111111111111".into());
        target.before.head.branch = None;
        assert!(prepare(temp.path(), &target, true).is_err());
        Ok(())
    }

    #[test]
    fn journal_and_workspace_lock_failure_prevent_actions_and_config_writes() -> Result<()> {
        let temp = TempDir::new("repository-lock")?;
        let (repo, _) = fixture::make_history(temp.path(), &["one"])?;
        let session = Session::begin(temp.path(), SystemTime::now())?;
        fs::create_dir(temp.path().join("config"))?;
        let yaml = temp.path().join("config/a.yaml");
        fs::write(&yaml, "gain: 1\n")?;
        let mut form = crate::config::Form::open_in(yaml.clone(), temp.path())?;
        form.set_session(temp.path(), &session.id);
        form.activate();
        form.paste("2");
        let locked = workspace_lock(temp.path())?;
        assert!(form.save().is_err());
        assert_eq!(fs::read_to_string(&yaml)?, "gain: 1\n");
        let mut called = false;
        assert!(
            run(
                temp.path(),
                &session.id,
                "branch switch",
                ".",
                "main",
                || {
                    called = true;
                    Ok("test".into())
                }
            )
            .is_err()
        );
        assert!(!called);
        drop(locked);
        let dir = base(temp.path())?;
        fs::remove_dir(&dir)?;
        fs::write(&dir, "not a directory")?;
        assert!(
            run(
                temp.path(),
                &session.id,
                "branch switch",
                ".",
                "main",
                || {
                    called = true;
                    crate::gitops::switch_branch(&repo, "main")?;
                    Ok("test".into())
                }
            )
            .is_err()
        );
        assert!(!called);
        Ok(())
    }

    #[test]
    fn corrupt_journal_is_retained_and_never_restorable() -> Result<()> {
        let temp = TempDir::new("repository-corrupt")?;
        let (repo, ids) = fixture::make_history(temp.path(), &["one", "two"])?;
        let session = Session::begin(temp.path(), SystemTime::now())?;
        checkout(temp.path(), &session, &repo, ids[0])?;
        let mut records = events(temp.path())?.0;
        let event = records.remove(0);
        fs::write(event.file.parent().unwrap().join("outcome.json"), "{}")?;
        assert!(verify(temp.path(), &event).is_err());
        assert!(events(temp.path())?.0.is_empty());
        assert!(!events(temp.path())?.1.is_empty());
        assert!(event.file.exists());
        Ok(())
    }

    #[test]
    fn pull_only_sessions_record_noop_and_failed_targets_without_contacting_remotes() -> Result<()>
    {
        let temp = TempDir::new("repository-network")?;
        fixture::make_history(temp.path(), &["one"])?;
        let session = Session::begin(temp.path(), SystemTime::now())?;
        let (receiver, _) = crate::network::start_recorded(
            vec![
                crate::network::Target {
                    name: "main".into(),
                    path: Some(temp.path().into()),
                },
                crate::network::Target {
                    name: "uninitialized".into(),
                    path: None,
                },
            ],
            crate::network::Operation::Pull,
            Some(crate::network::Recording {
                root: temp.path().into(),
                session: session.id.clone(),
                note: "Update modules for testing".into(),
                note_group: crate::session::identifier()?,
            }),
        );
        loop {
            if matches!(
                receiver.recv_timeout(std::time::Duration::from_secs(5))?,
                crate::network::Event::Done(_)
            ) {
                break;
            }
        }
        let browser = Browser::open(temp.path())?;
        assert_eq!(browser.repository_events.len(), 2);
        assert!(
            browser
                .repository_events
                .iter()
                .all(|event| event.intent.note == "Update modules for testing")
        );
        assert_eq!(browser.notes().len(), 1);
        assert!(
            browser.notes()[0].0.contains(". (completed)")
                && browser.notes()[0].0.contains("uninitialized")
        );
        assert_eq!(browser.visible.len(), 1);
        assert!(
            browser
                .comparison()?
                .iter()
                .any(|c| c.after.contains("no remotes"))
        );
        assert!(
            browser
                .comparison()?
                .iter()
                .any(|c| c.after.contains("not initialized"))
        );
        Ok(())
    }

    #[test]
    fn ungrouped_legacy_actions_with_equal_notes_remain_separate() -> Result<()> {
        let temp = TempDir::new("repository-legacy-notes")?;
        fixture::make_history(temp.path(), &["one"])?;
        let session = Session::begin(temp.path(), SystemTime::now())?;
        for _ in 0..2 {
            run_noted(
                temp.path(),
                &session.id,
                "branch switch",
                ".",
                "main",
                "Same note",
                || Ok("Unchanged".into()),
            )?;
        }
        session.finish()?;
        let browser = Browser::open(temp.path())?;
        assert_eq!(browser.notes().len(), 2);
        for event in browser.repository_events {
            assert!(event.intent.note_group.is_none());
            let json = serde_json::to_value(&event.intent)?;
            assert!(
                json.get("note_group").is_none(),
                "old journal serialization/checksums remain compatible"
            );
        }
        Ok(())
    }

    #[test]
    fn recorded_fast_forward_keeps_newer_branch_and_remote_refs_when_restoring_old_checkout()
    -> Result<()> {
        let temp = TempDir::new("repository-recorded-ff")?;
        let (remote, _) = fixture::make_history(&temp.path().join("remote"), &["first"])?;
        let root = temp.path().join("local");
        let local = Repository::clone(remote.workdir().unwrap().to_str().unwrap(), &root)?;
        let old = local.head()?.target().unwrap();
        let latest = fixture::commit_file(
            &remote,
            "new.txt",
            "new\n",
            "latest",
            fixture::BASE_TIME + 60,
        )?;
        let session = Session::begin(&root, SystemTime::now())?;
        run(&root, &session.id, "pull", ".", "confirmed pull", || {
            crate::network::update(&root, crate::network::Operation::Pull)
        })?;
        let browser = Browser::open(&root)?;
        let diff = browser.comparison()?;
        assert!(
            diff.iter()
                .any(|c| c.name.contains("fetched ref") && c.after == latest.to_string())
        );
        assert!(
            diff.iter()
                .any(|c| c.name.contains("local branch tip") && c.after == latest.to_string())
        );
        let mut restore = review(&root, &browser, true)?;
        assert!(restore.errors.is_empty(), "{:?}", restore.errors);
        crate::fixture::confirm_restore(&mut restore, &root, &session.id)?;
        let state = capture(&root, ".")?;
        assert_eq!(state.head.commit, Some(old.to_string()));
        assert!(state.head.branch.is_none());
        assert_eq!(state.local_refs["refs/heads/main"], latest.to_string());
        assert_eq!(
            state.remote_refs["refs/remotes/origin/main"],
            latest.to_string()
        );
        Ok(())
    }

    #[test]
    fn repository_metadata_is_not_in_git_status_or_objects_or_refs() -> Result<()> {
        let temp = TempDir::new("repository-private")?;
        let (repo, ids) = fixture::make_history(temp.path(), &["one", "two"])?;
        let session = Session::begin(temp.path(), SystemTime::now())?;
        let refs = capture(temp.path(), ".")?.local_refs;
        let count = || -> Result<usize> {
            let mut n = 0;
            repo.odb()?.foreach(|_| {
                n += 1;
                true
            })?;
            Ok(n)
        };
        let objects = count()?;
        checkout(temp.path(), &session, &repo, ids[0])?;
        assert_eq!(count()?, objects);
        assert_eq!(capture(temp.path(), ".")?.local_refs, refs);
        assert!(repo.statuses(None)?.is_empty());
        assert!(base(temp.path())?.starts_with(repo.path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let record = events(temp.path())?.0.remove(0);
            assert_eq!(
                fs::metadata(&record.file)?.permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(record.file.parent().unwrap())?
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        Ok(())
    }
}
