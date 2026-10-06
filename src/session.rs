//! Session envelopes and reviewed multi-file restore journals, private to a checkout.
//! Plans are durable before any checkout/YAML write. Recovery only inspects.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::config::Form;
use crate::history::{
    Change, Store, hash, history_dir, private_dir, resolve_config, sync_dir, write_private,
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn identifier() -> Result<String> {
    Ok(format!(
        "{:020}-{:010}-{:010}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(crate) fn valid_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 100 || !id.bytes().all(|c| c.is_ascii_digit() || c == b'-') {
        bail!("invalid private journal identifier");
    }
    Ok(())
}

fn directory(root: &Path, name: &str) -> Result<PathBuf> {
    let dir = history_dir(root)?.parent().unwrap().join(name);
    private_dir(&dir)?;
    Ok(dir)
}

#[derive(Serialize, Deserialize)]
struct Checked<T> {
    payload: T,
    checksum: String,
}

pub(crate) fn write<T: Serialize>(path: &Path, payload: &T) -> Result<()> {
    let checksum = hash(&serde_json::to_string(payload)?)?;
    let data = serde_json::to_vec(&Checked { payload, checksum })?;
    if data.len() > 32 * 1024 * 1024 {
        bail!("private journal exceeds the 32 MiB limit; no write performed");
    }
    write_private(path, &data)?;
    sync_dir(path.parent().unwrap())
}

pub(crate) fn read<T: Serialize + DeserializeOwned>(path: &Path) -> Result<T> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.file_type().is_symlink() || m.len() > 32 * 1024 * 1024 {
        bail!("invalid private journal: {}", path.display());
    }
    let value: Checked<T> = serde_json::from_slice(&fs::read(path)?)?;
    if hash(&serde_json::to_string(&value.payload)?)? != value.checksum {
        bail!("private journal integrity check failed: {}", path.display());
    }
    Ok(value.payload)
}

pub(crate) fn lock(path: &Path, create: bool) -> Result<File> {
    if let Ok(m) = fs::symlink_metadata(path) {
        if !m.is_file() || m.file_type().is_symlink() {
            bail!("journal lock was replaced");
        }
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    file.try_lock_exclusive()
        .context("journal is active in another taid instance")?;
    Ok(file)
}

pub(crate) fn contended(error: &anyhow::Error) -> bool {
    error.chain().any(|error| {
        error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
        })
    })
}

#[derive(Serialize, Deserialize)]
struct Started {
    id: String,
    timestamp: i64,
    pid: u32,
}

pub struct Session {
    pub id: String,
    dir: PathBuf,
    _lock: File,
}

impl Session {
    pub fn begin(root: &Path, started: SystemTime) -> Result<Self> {
        let id = identifier()?;
        let dir = directory(root, "config-sessions")?.join(&id);
        private_dir(&dir)?;
        let active = lock(&dir.join("active.lock"), true)?;
        write(
            &dir.join("start.json"),
            &Started {
                id: id.clone(),
                timestamp: started.duration_since(UNIX_EPOCH)?.as_secs() as i64,
                pid: std::process::id(),
            },
        )?;
        Ok(Self {
            id,
            dir,
            _lock: active,
        })
    }

    // Called only after the TUI exits normally, never from Drop/panic unwinding.
    pub fn finish(&self) -> Result<()> {
        if !self.dir.join("end.json").exists() {
            write(
                &self.dir.join("end.json"),
                &(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64),
            )?;
        }
        Ok(())
    }
}

pub struct Info {
    pub started: i64,
    pub ended: Option<i64>,
    pub description: String,
    pub state: String,
}

pub fn info(root: &Path, id: &str) -> Result<Info> {
    valid_id(id)?;
    let dir = directory(root, "config-sessions")?.join(id);
    let metadata = fs::symlink_metadata(&dir)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("session directory was replaced");
    }
    let start: Started = read(&dir.join("start.json"))?;
    if start.id != id {
        bail!("session identity mismatch");
    }
    let ended = if dir.join("end.json").exists() {
        Some(read::<i64>(&dir.join("end.json"))?)
    } else {
        None
    };
    let state = if ended.is_some() {
        "closed"
    } else {
        match lock(&dir.join("active.lock"), false) {
            Ok(_) => "interrupted",
            Err(e) if contended(&e) => "active",
            Err(e) => return Err(e),
        }
    };
    let mut notes = Vec::new();
    for file in fs::read_dir(&dir)? {
        let file = file?.path();
        if file
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("note-"))
        {
            notes.push(file);
        }
    }
    notes.sort();
    let description = match notes.last() {
        Some(file) => read::<String>(file)?,
        None => String::new(),
    };
    Ok(Info {
        started: start.timestamp,
        ended,
        description,
        state: state.into(),
    })
}

pub fn describe(root: &Path, id: &str, description: &str) -> Result<()> {
    info(root, id)?; // Validate the scope and immutable start before appending.
    if description.chars().count() > 500 || description.chars().any(char::is_control) {
        bail!("session description must be at most 500 printable characters");
    }
    let dir = directory(root, "config-sessions")?.join(id);
    write(
        &dir.join(format!("note-{}.json", identifier()?)),
        &description,
    )
}

#[derive(Clone)]
pub struct Target {
    pub path: String,
    pub before: String,
    pub after: String,
    pub source: String,
    pub uncertain: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct FilePlan {
    pub path: String,
    pub before: String,
    pub after: String,
    pub changes: Vec<Change>,
    pub source: String,
}

#[derive(Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub session: String,
    pub timestamp: i64,
    pub label: String,
    pub note: String,
    pub files: Vec<FilePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<crate::repository_history::RepoPlan>,
}

pub struct Review {
    pub label: String,
    pub files: Vec<FilePlan>,
    pub repositories: Vec<crate::repository_history::RepoPlan>,
    unchanged: BTreeMap<String, String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub note: String,
    pub note_editing: bool,
    pub scroll: usize,
    pub outcome: Option<String>,
    pub recovery: Option<String>,
}

impl Review {
    pub fn prepare(
        root: &Path,
        targets: Vec<Target>,
        before: bool,
        label: String,
        recovery: Option<String>,
    ) -> Self {
        let mut review = Self {
            label,
            files: Vec::new(),
            repositories: Vec::new(),
            unchanged: BTreeMap::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            note: String::new(),
            note_editing: false,
            scroll: 0,
            outcome: None,
            recovery,
        };
        let mut seen = std::collections::HashSet::new();
        for target in targets {
            let path = target.path.clone();
            let result = (|| -> Result<Option<FilePlan>> {
                if !seen.insert(path.clone()) {
                    bail!("duplicate file in restore target");
                }
                let file = resolve_config(root, &path)?;
                let mut form = Form::open_in(file, root)?;
                let current = form.baseline().to_owned();
                let snapshot = if before {
                    &target.before
                } else {
                    &target.after
                };
                let expected = if before {
                    &target.after
                } else {
                    &target.before
                };
                form.restore(snapshot, target.source.clone())?;
                let after = form.text()?;
                if after == current {
                    review.unchanged.insert(path.clone(), current);
                    return Ok(None);
                }
                if target.uncertain {
                    review.warnings.push(format!(
                        "{path}: includes interrupted/uncertain saves; verify the target values"
                    ));
                }
                if crate::config::compare(&current, expected)
                    .map(|c| !c.is_empty())
                    .unwrap_or(true)
                {
                    review.warnings.push(format!("{path}: later/external edits differ from the expected endpoint; confirmation explicitly replaces the displayed values"));
                }
                Ok(Some(FilePlan {
                    path: path.clone(),
                    before: current,
                    after,
                    changes: form.changes(),
                    source: format!(
                        "{}:{}",
                        target.source,
                        if before { "before" } else { "after" }
                    ),
                }))
            })();
            match result {
                Ok(Some(file)) => review.files.push(file),
                Ok(None) => {}
                Err(e) => review.errors.push(format!("{path}: {e:#}")),
            }
        }
        review.files.sort_by(|a, b| a.path.cmp(&b.path));
        review
    }

    pub fn prepare_project(
        root: &Path,
        targets: Vec<Target>,
        repository_targets: Vec<crate::repository_history::Target>,
        before: bool,
        label: String,
        recovery: Option<String>,
    ) -> Self {
        let mut review = Self::prepare(root, targets, before, label, recovery);
        for target in repository_targets {
            match crate::repository_history::prepare(root, &target, before) {
                Ok((plan, warnings)) => {
                    review.repositories.push(plan);
                    review.warnings.extend(warnings);
                }
                Err(e) => review
                    .errors
                    .push(format!("{}: {e:#}", target.before.module)),
            }
        }
        review.repositories.sort_by(|a, b| {
            a.before
                .module
                .split('/')
                .count()
                .cmp(&b.before.module.split('/').count())
                .then(a.before.module.cmp(&b.before.module))
        });
        // A checkout can replace tracked YAML. Never silently reuse a preview
        // based on a different revision/schema: restore these rows separately.
        for (path, baseline) in review
            .unchanged
            .iter()
            .chain(review.files.iter().map(|f| (&f.path, &f.before)))
        {
            match crate::repository_history::projected_file(root, path, &review.repositories, baseline) {
                Ok(projected) if projected == *baseline => {},
                Ok(_) => review.errors.push(format!("{path}: repository checkout changes this config revision; restore repository and config rows separately and review the resulting schema")),
                Err(e) => review.errors.push(format!("{path}: {e:#}")),
            }
        }
        review
    }

    pub fn confirm(&mut self, root: &Path, session: &str) -> Result<()> {
        if self.outcome.is_some() {
            bail!(
                "restore already attempted; reopen history to retry after reviewing current disk values"
            );
        }
        if !self.errors.is_empty() {
            bail!(
                "restore blocked: resolve every incompatible/missing file before retrying; nothing was written"
            );
        }
        let _workspace = crate::repository_history::workspace_lock(root)?;
        for plan in &self.repositories {
            crate::repository_history::preflight(root, plan)?;
        }
        // Take every per-file lock in deterministic order, then revalidate ALL
        // schemas/disk baselines before creating the batch or touching any YAML.
        if let Some(id) = &self.recovery {
            let (records, _) = recoveries(root)?;
            if !records
                .iter()
                .any(|record| record.id == *id && !record.active)
            {
                bail!(
                    "recovery journal is active, already resolved or cannot be verified; nothing was written"
                );
            }
        }
        let mut baselines = self.unchanged.clone();
        for file in &self.files {
            baselines.insert(file.path.clone(), file.before.clone());
        }
        let mut stores = BTreeMap::new();
        let mut locks = Vec::new();
        for (relative, before) in &baselines {
            let path = resolve_config(root, relative)?;
            let form = Form::open_in(path.clone(), root)?;
            if form.baseline() != before {
                bail!(
                    "{} changed after review; reopen the restore preview. Nothing was written",
                    relative
                );
            }
            let store = Store::open_in(&path, root)?.with_session(Some(session.into()));
            locks.push(store.lock()?);
            stores.insert(relative.clone(), (path, store));
        }
        for (relative, before) in &baselines {
            if fs::read_to_string(&stores[relative].0)? != *before {
                bail!("{} changed after review; nothing was written", relative);
            }
        }
        let id = identifier()?;
        let dir = directory(root, "config-restores")?.join(&id);
        private_dir(&dir)?;
        let _active = lock(&dir.join("active.lock"), true)?;
        let plan = Plan {
            id: id.clone(),
            session: session.into(),
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
            label: self.label.clone(),
            note: self.note.clone(),
            files: self.files.clone(),
            repositories: self.repositories.clone(),
        };
        // Even configs not yet reached by the write loop have recovery snapshots.
        write(&dir.join("plan.json"), &plan)?;
        let mut applied = 0;
        let mut repositories_applied = 0;
        let result = (|| -> Result<()> {
            for (index, repository) in self.repositories.iter().enumerate() {
                if !repository.before.checkout_eq(&repository.after) {
                    crate::repository_history::apply(root, session, repository, &self.note)?;
                    repositories_applied += 1;
                }
                if !crate::repository_history::capture(root, &repository.before.module)?
                    .checkout_eq(&repository.after)
                {
                    bail!(
                        "{} no longer matches the reviewed checkout/index; stopped without forcing",
                        repository.before.module
                    );
                }
                write(
                    &dir.join(format!("repo-{index:05}.done")),
                    &repository.after,
                )?;
            }
            for (index, file) in self.files.iter().enumerate() {
                let (path, store) = stores.get_mut(&file.path).unwrap();
                store.refresh_context(path)?;
                #[cfg(test)]
                batch_failpoint(index)?;
                let notice = store.save_locked(
                    path,
                    &file.before,
                    &file.after,
                    file.changes.clone(),
                    &self.note,
                    Some(format!("batch:{id}:{}", file.source)),
                )?;
                applied += 1;
                write(&dir.join(format!("{index:05}.done")), &hash(&file.after)?)?;
                if notice.contains("needs recovery") {
                    bail!("{}: {notice}", file.path);
                }
            }
            write(&dir.join("complete.json"), &id)?;
            if let Some(previous) = &self.recovery {
                resolve_recovery(root, previous, &id)?;
            }
            Ok(())
        })();
        self.outcome = Some(match result {
            Ok(()) => format!(
                "Restored {applied} file(s), {repositories_applied} repository state(s) · saved in the current session · recovery plan {id}"
            ),
            Err(e) => format!(
                "Restore stopped: {e:#} · {applied}/{} writes completed, {repositories_applied} repository states restored (inspect disk for uncertain outcomes). Plan {id} retained; h → u reviews resume/rollback; no automatic rollback",
                self.files.len()
            ),
        });
        drop(locks);
        Ok(())
    }
}

pub struct Recovery {
    pub id: String,
    pub timestamp: i64,
    pub label: String,
    pub state: String,
    pub details: Vec<String>,
    pub active: bool,
    pub targets: Vec<Target>,
    pub repository_targets: Vec<crate::repository_history::Target>,
}

pub fn recoveries(root: &Path) -> Result<(Vec<Recovery>, Vec<String>)> {
    let base = directory(root, "config-restores")?;
    let mut records = Vec::new();
    let mut warnings = Vec::new();
    for dir in fs::read_dir(base)? {
        let dir = dir?.path();
        let result = (|| -> Result<Option<Recovery>> {
            let id = dir
                .file_name()
                .and_then(|s| s.to_str())
                .context("non-UTF-8 restore directory")?;
            valid_id(id)?;
            let meta = fs::symlink_metadata(&dir)?;
            if !meta.is_dir() || meta.file_type().is_symlink() {
                bail!("restore directory was replaced");
            }
            if dir.join("resolved.json").exists() {
                let _: String = read(&dir.join("resolved.json"))?;
                return Ok(None);
            }
            let plan: Plan = read(&dir.join("plan.json"))?;
            if plan.id != id {
                bail!("restore identity mismatch");
            }
            valid_id(&plan.session)?;
            if dir.join("complete.json").exists() {
                if read::<String>(&dir.join("complete.json"))? != id {
                    bail!("restore completion identity mismatch");
                }
                return Ok(None);
            }
            let active = match lock(&dir.join("active.lock"), false) {
                Ok(_) => false,
                Err(e) if contended(&e) => true,
                Err(e) => return Err(e),
            };
            let mut details = Vec::new();
            let mut targets = Vec::new();
            let mut counts = BTreeMap::<&str, usize>::new();
            let mut repository_targets = Vec::new();
            for (index, repository) in plan.repositories.into_iter().enumerate() {
                let state =
                    match crate::repository_history::capture(root, &repository.before.module) {
                        Ok(current) if current.checkout_eq(&repository.after) => {
                            "target checkout/index"
                        }
                        Ok(current) if current.checkout_eq(&repository.before) => {
                            "not applied / rolled back"
                        }
                        Ok(_) => "externally changed",
                        Err(_) => "missing / unsafe repository",
                    };
                *counts.entry(state).or_default() += 1;
                let progress = dir.join(format!("repo-{index:05}.done"));
                if progress.exists() {
                    let recorded: crate::repository_history::State = read(&progress)?;
                    if recorded != repository.after {
                        bail!("repository progress integrity mismatch");
                    }
                }
                details.push(format!(
                    "{}: {state} · {}",
                    repository.before.module,
                    if progress.exists() {
                        "write recorded"
                    } else {
                        "no completion marker"
                    }
                ));
                repository_targets.push(crate::repository_history::Target {
                    before: repository.before,
                    after: Some(repository.after),
                    source: format!("recovery:{id}"),
                    uncertain: false,
                });
            }
            for (index, file) in plan.files.into_iter().enumerate() {
                if file.before.len() > 2 * 1024 * 1024 || file.after.len() > 2 * 1024 * 1024 {
                    bail!("restore snapshots exceed the file limit");
                }
                let state = match resolve_config(root, &file.path)
                    .and_then(|p| Ok(fs::read_to_string(p)?))
                {
                    Ok(current) if current == file.after => "target on disk",
                    Ok(current) if current == file.before => "not applied / rolled back",
                    Ok(_) => "externally changed",
                    Err(_) => "missing / unsafe path",
                };
                *counts.entry(state).or_default() += 1;
                let progress = if dir.join(format!("{index:05}.done")).exists() {
                    if read::<String>(&dir.join(format!("{index:05}.done")))? != hash(&file.after)?
                    {
                        bail!("restore progress integrity mismatch");
                    }
                    "write recorded"
                } else {
                    "no completion marker"
                };
                details.push(format!("{}: {state} · {progress}", file.path));
                targets.push(Target {
                    path: file.path,
                    before: file.before,
                    after: file.after,
                    source: format!("recovery:{id}"),
                    uncertain: false,
                });
            }
            let state = counts
                .iter()
                .map(|(s, n)| format!("{n} {s}"))
                .collect::<Vec<_>>()
                .join("; ");
            Ok(Some(Recovery {
                id: id.into(),
                timestamp: plan.timestamp,
                label: plan.label,
                state,
                details,
                active,
                targets,
                repository_targets,
            }))
        })();
        match result {
            Ok(Some(record)) => {
                warnings.push(format!("Unfinished restore {}: {} (u opens recovery; nothing is changed automatically)", record.id, record.state));
                records.push(record);
            }
            Ok(None) => {}
            Err(e) => warnings.push(format!(
                "Restore journal retained, recovery blocked: {}: {e:#}",
                dir.display()
            )),
        }
    }
    records.sort_by(|a, b| b.id.cmp(&a.id));
    Ok((records, warnings))
}

fn resolve_recovery(root: &Path, previous: &str, replacement: &str) -> Result<()> {
    valid_id(previous)?;
    let dir = directory(root, "config-restores")?.join(previous);
    let metadata = fs::symlink_metadata(&dir)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("restore directory was replaced");
    }
    let plan: Plan = read(&dir.join("plan.json"))?;
    if plan.id != previous {
        bail!("restore identity mismatch");
    }
    let _lock = lock(&dir.join("active.lock"), false)?;
    if !dir.join("resolved.json").exists() {
        write(&dir.join("resolved.json"), &replacement)?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! { static BATCH_FAIL: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
fn batch_failpoint(index: usize) -> Result<()> {
    if BATCH_FAIL.with(|p| {
        if p.get() == Some(index) {
            p.set(None);
            true
        } else {
            false
        }
    }) {
        bail!("simulated interruption between files");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fixture::{self, SubSpec, TempDir},
        history::{Browser, HistoryRow},
    };
    use crossterm::event::{KeyCode, KeyModifiers};

    fn fixture(tag: &str) -> Result<(TempDir, PathBuf, Vec<PathBuf>)> {
        let temp = TempDir::new(tag)?;
        let stack = fixture::make_super(
            temp.path(),
            &[SubSpec {
                name: "subA",
                msgs: &["initial"],
                recorded: None,
                drop_workdir: false,
            }],
        )?;
        let root = stack.repo.workdir().unwrap().canonicalize()?;
        let paths = vec![root.join("config/a.yaml"), root.join("subA/config/b.yaml")];
        for path in &paths {
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, "# keep\ngain: 1\n")?;
        }
        Ok((temp, root, paths))
    }

    fn save(root: &Path, path: &Path, session: &Session, value: &str) -> Result<()> {
        let mut form = Form::open_in(path.to_path_buf(), root)?;
        form.set_session(root, &session.id);
        form.activate();
        form.paste(value);
        form.save()
    }

    fn populated(root: &Path, paths: &[PathBuf]) -> Result<Session> {
        let session = Session::begin(root, SystemTime::now())?;
        save(root, &paths[0], &session, "2")?;
        save(root, &paths[0], &session, "3")?;
        save(root, &paths[1], &session, "4")?;
        Ok(session)
    }

    #[test]
    fn same_commit_branch_restore_and_tracked_config_restore_preserve_changes_until_confirmation()
    -> Result<()> {
        let (_temp, root, paths) = fixture("session-tracked-branch")?;
        let repo = git2::Repository::open(&root)?;
        let id = fixture::commit_file(
            &repo,
            "config/a.yaml",
            "gain: 1\n",
            "config",
            fixture::BASE_TIME + 200,
        )?;
        repo.branch("experiment", &repo.find_commit(id)?, false)?;
        let session = Session::begin(&root, SystemTime::now())?;
        crate::repository_history::run(
            &root,
            &session.id,
            "branch switch",
            ".",
            "experiment",
            || {
                crate::gitops::switch_branch(&repo, "experiment")?;
                Ok("switched".into())
            },
        )?;
        save(&root, &paths[0], &session, "2")?;
        let browser = Browser::open(&root)?;
        let mut review = Review::prepare_project(
            &root,
            browser.targets()?,
            browser.repository_targets()?,
            true,
            "mixed".into(),
            None,
        );
        assert!(review.errors.is_empty(), "{:?}", review.errors);
        assert_eq!(fs::read_to_string(&paths[0])?, "gain: 2\n");
        review.confirm(&root, &session.id)?;
        assert!(
            review
                .outcome
                .as_ref()
                .unwrap()
                .starts_with("Restored 1 file(s), 1 repository state(s)"),
            "{:?}",
            review.outcome
        );
        assert_eq!(fs::read_to_string(&paths[0])?, "gain: 1\n");
        assert_eq!(repo.head()?.shorthand(), Some("main"));
        assert_eq!(Store::open_in(&paths[0], &root)?.list()?[0].branch, "main");
        Ok(())
    }

    #[test]
    fn mixed_session_restore_and_partial_recovery_include_repository_endpoints() -> Result<()> {
        for rollback in [false, true] {
            let (_temp, root, paths) = fixture("session-repository-recovery")?;
            let repo = git2::Repository::open(&root)?;
            repo.branch("experiment", &repo.head()?.peel_to_commit()?, false)?;
            let session = Session::begin(&root, SystemTime::now())?;
            crate::repository_history::run(
                &root,
                &session.id,
                "branch switch",
                ".",
                "experiment",
                || {
                    crate::gitops::switch_branch(&repo, "experiment")?;
                    Ok("switched".into())
                },
            )?;
            save(&root, &paths[0], &session, "2")?;
            save(&root, &paths[1], &session, "3")?;
            let browser = Browser::open(&root)?;
            assert_eq!(browser.visible.len(), 1);
            let changes = browser.comparison()?;
            assert_eq!(
                changes.iter().filter(|c| c.name.ends_with("gain")).count(),
                2
            );
            assert!(
                changes
                    .iter()
                    .any(|c| c.name.contains("branch / checkout commit"))
            );
            let mut review = Review::prepare_project(
                &root,
                browser.targets()?,
                browser.repository_targets()?,
                true,
                "mixed restore".into(),
                None,
            );
            assert!(review.errors.is_empty(), "{:?}", review.errors);
            BATCH_FAIL.with(|p| p.set(Some(0)));
            review.confirm(&root, &session.id)?;
            assert!(review.outcome.as_ref().unwrap().contains("Restore stopped"));
            assert_eq!(repo.head()?.shorthand(), Some("main"));
            assert_eq!(fs::read_to_string(&paths[0])?, "# keep\ngain: 2\n");
            let mut browser = Browser::open(&root)?;
            browser.key(KeyCode::Char('u'), KeyModifiers::NONE, 10);
            assert_eq!(browser.visible.len(), 1);
            assert_eq!(browser.repository_targets()?.len(), 2);
            assert!(
                browser.recoveries[0]
                    .details
                    .iter()
                    .any(|s| s.contains("target checkout/index"))
            );
            let mut recovery = Review::prepare_project(
                &root,
                browser.targets()?,
                browser.repository_targets()?,
                rollback,
                "recovery".into(),
                browser.recovery_id(),
            );
            assert!(recovery.errors.is_empty(), "{:?}", recovery.errors);
            recovery.confirm(&root, &session.id)?;
            assert!(recoveries(&root)?.0.is_empty());
            assert_eq!(
                repo.head()?.shorthand(),
                Some(if rollback { "experiment" } else { "main" })
            );
            assert_eq!(
                fs::read_to_string(&paths[0])?,
                if rollback {
                    "# keep\ngain: 2\n"
                } else {
                    "# keep\ngain: 1\n"
                }
            );
        }
        Ok(())
    }

    #[test]
    fn mixed_restore_blocks_a_checkout_that_would_replace_the_config_preview_revision() -> Result<()>
    {
        let (_temp, root, paths) = fixture("session-repository-schema")?;
        let repo = git2::Repository::open(&root)?;
        let old = crate::fixture::commit_file(
            &repo,
            "config/a.yaml",
            "gain: 1\n",
            "old",
            crate::fixture::BASE_TIME + 200,
        )?;
        crate::fixture::commit_file(
            &repo,
            "config/a.yaml",
            "gain: 2\n",
            "new",
            crate::fixture::BASE_TIME + 300,
        )?;
        let session = Session::begin(&root, SystemTime::now())?;
        crate::repository_history::run(
            &root,
            &session.id,
            "commit pick",
            ".",
            &old.to_string(),
            || {
                crate::gitops::checkout_oid(&repo, old)?;
                Ok("picked".into())
            },
        )?;
        save(&root, &paths[0], &session, "3")?;
        // Clean the tracked config without changing its recorded endpoints.
        fs::write(&paths[0], "gain: 1\n")?;
        let browser = Browser::open(&root)?;
        let mut review = Review::prepare_project(
            &root,
            browser.targets()?,
            browser.repository_targets()?,
            true,
            "mixed".into(),
            None,
        );
        assert!(
            review
                .errors
                .iter()
                .any(|s| s.contains("restore repository and config rows separately")),
            "{:?}",
            review.errors
        );
        assert!(review.confirm(&root, &session.id).is_err());
        assert_eq!(repo.head()?.target(), Some(old));
        assert_eq!(fs::read_to_string(&paths[0])?, "gain: 1\n");
        Ok(())
    }

    #[test]
    fn session_encloses_modules_and_saves_with_net_diff_and_drill_down() -> Result<()> {
        let (_temp, root, paths) = fixture("session-group")?;
        let session = populated(&root, &paths)?;
        describe(&root, &session.id, "Wet track tuning λ")?;
        assert_eq!(info(&root, &session.id)?.state, "active");
        let mut browser = Browser::open(&root)?;
        assert_eq!(browser.visible.len(), 1);
        assert!(
            matches!(browser.selected_row(),Some(HistoryRow::Session(_,indices)) if indices.len()==3)
        );
        assert!(
            browser
                .selected_label()
                .contains("2 modules / 2 files / 3 saves")
        );
        assert!(browser.selected_label().contains("Wet track tuning λ"));
        let targets = browser.targets()?;
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].before, "# keep\ngain: 1\n");
        assert_eq!(targets[0].after, "# keep\ngain: 3\n");
        assert_eq!(browser.comparison()?.len(), 2);
        browser.key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert_eq!(browser.visible.len(), 2);
        assert!(matches!(browser.selected_row(), Some(HistoryRow::File(_))));
        browser.key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert_eq!(browser.visible.len(), 2);
        assert!(matches!(browser.selected_row(), Some(HistoryRow::Save(_))));
        assert_eq!(browser.comparison()?[0].before, "2");
        assert_eq!(browser.comparison()?[0].after, "3");
        session.finish()?;
        assert_eq!(info(&root, &session.id)?.state, "closed");
        drop(session);
        assert_eq!(Browser::open(&root)?.targets()?.len(), 2);
        Ok(())
    }

    #[test]
    fn abandoned_sessions_are_interrupted_and_do_not_merge_across_instances() -> Result<()> {
        let (_temp, root, paths) = fixture("session-crash")?;
        let first = populated(&root, &paths)?;
        let id = first.id.clone();
        drop(first); // No explicit normal-close marker: crash-equivalent lock release.
        assert_eq!(info(&root, &id)?.state, "interrupted");
        let second = Session::begin(&root, SystemTime::now())?;
        save(&root, &paths[0], &second, "5")?;
        let browser = Browser::open(&root)?;
        assert_eq!(browser.visible.len(), 2);
        assert_eq!(browser.targets()?[0].before, "# keep\ngain: 3\n");
        assert_eq!(browser.targets()?[0].after, "# keep\ngain: 5\n");
        Ok(())
    }

    #[test]
    fn legacy_entries_remain_individual_and_search_never_trims_session_restore_scope() -> Result<()>
    {
        let (_temp, root, paths) = fixture("session-legacy-search")?;
        let mut old = Form::open_in(paths[0].clone(), &root)?;
        old.activate();
        old.paste("7");
        old.save()?;
        let session = populated(&root, &paths)?;
        let mut browser = Browser::open(&root)?;
        assert_eq!(browser.visible.len(), 2);
        assert!(
            matches!(browser.selected_row(),Some(HistoryRow::Session(id,_)) if id == &session.id),
            "new sessions should precede the legacy folder"
        );
        browser.key(KeyCode::Char('/'), KeyModifiers::NONE, 10);
        browser.paste("subA");
        browser.key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert_eq!(browser.visible.len(), 1);
        assert_eq!(
            browser.targets()?.len(),
            2,
            "search hides rows; it must not silently narrow an entire-session restore"
        );
        browser.key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert_eq!(
            browser.targets()?.len(),
            1,
            "a file row deliberately narrows the scope"
        );
        assert_eq!(browser.targets()?[0].path, "subA/config/b.yaml");
        session.finish()?;
        Ok(())
    }

    #[test]
    fn batch_restore_reviews_net_values_preserves_format_and_adds_new_session_saves() -> Result<()>
    {
        let (_temp, root, paths) = fixture("session-restore")?;
        let original = populated(&root, &paths)?;
        original.finish()?;
        drop(original);
        let targets = Browser::open(&root)?.targets()?;
        let mut review = Review::prepare(&root, targets, true, "undo tuning".into(), None);
        assert!(review.errors.is_empty());
        assert!(review.warnings.is_empty());
        assert_eq!(review.files.len(), 2);
        assert_eq!(fs::read_to_string(&paths[0])?, "# keep\ngain: 3\n");
        let restore = Session::begin(&root, SystemTime::now())?;
        review.confirm(&root, &restore.id)?;
        assert!(review.outcome.as_ref().unwrap().contains("Restored 2"));
        assert_eq!(fs::read_to_string(&paths[0])?, "# keep\ngain: 1\n");
        assert_eq!(fs::read_to_string(&paths[1])?, "# keep\ngain: 1\n");
        assert_eq!(Store::list_global(&root, &[])?.0.len(), 5);
        assert_eq!(Browser::open(&root)?.visible.len(), 2);
        assert!(recoveries(&root)?.0.is_empty());
        Ok(())
    }

    #[test]
    fn batch_preflight_blocks_schema_missing_file_disk_races_and_locks_before_any_write()
    -> Result<()> {
        for mode in ["schema", "missing", "race", "lock", "noop-race"] {
            let (_temp, root, paths) = fixture(&format!("session-preflight-{mode}"))?;
            let original = populated(&root, &paths)?;
            let targets = Browser::open(&root)?.targets()?;
            if mode == "schema" {
                fs::write(&paths[1], "gain: 'text'\n")?;
            }
            if mode == "missing" {
                fs::remove_file(&paths[1])?;
            }
            if mode == "noop-race" {
                fs::write(&paths[1], "# keep\ngain: 1\n")?;
            }
            let mut review = Review::prepare(&root, targets, true, "blocked".into(), None);
            if mode == "race" || mode == "noop-race" {
                fs::write(&paths[1], "gain: 9\n")?;
            }
            let store = Store::open_in(&paths[0], &root)?;
            let _lock = if mode == "lock" {
                Some(store.lock()?)
            } else {
                None
            };
            assert!(review.confirm(&root, &original.id).is_err(), "{mode}");
            assert_eq!(
                fs::read_to_string(&paths[0])?,
                "# keep\ngain: 3\n",
                "no partial writes on preflight failure: {mode}"
            );
            assert!(
                recoveries(&root)?.0.is_empty(),
                "no batch starts on preflight failure"
            );
        }
        Ok(())
    }

    #[test]
    fn later_edits_require_visible_warning_and_preserve_current_comments() -> Result<()> {
        let (_temp, root, paths) = fixture("session-later-edit")?;
        let session = populated(&root, &paths)?;
        fs::write(&paths[0], "# external comments preserved\ngain: 9\n")?;
        let mut review = Review::prepare(
            &root,
            Browser::open(&root)?.targets()?,
            true,
            "restore".into(),
            None,
        );
        assert!(review.warnings.iter().any(|w| w.contains("later/external")));
        assert_eq!(review.files[0].changes[0].before, "9");
        assert_eq!(
            fs::read_to_string(&paths[0])?,
            "# external comments preserved\ngain: 9\n"
        );
        review.confirm(&root, &session.id)?;
        assert_eq!(
            fs::read_to_string(&paths[0])?,
            "# external comments preserved\ngain: 1\n"
        );
        Ok(())
    }

    #[test]
    fn interrupted_batch_retains_unwritten_snapshots_and_supports_reviewed_resume_or_rollback()
    -> Result<()> {
        for rollback in [false, true] {
            let (_temp, root, paths) = fixture(&format!("session-batch-recovery-{rollback}"))?;
            let original = populated(&root, &paths)?;
            let mut review = Review::prepare(
                &root,
                Browser::open(&root)?.targets()?,
                true,
                "restore".into(),
                None,
            );
            BATCH_FAIL.with(|p| p.set(Some(1)));
            review.confirm(&root, &original.id)?;
            assert!(
                review
                    .outcome
                    .as_ref()
                    .unwrap()
                    .contains("1/2 writes completed")
            );
            assert_eq!(fs::read_to_string(&paths[0])?, "# keep\ngain: 1\n");
            assert_eq!(fs::read_to_string(&paths[1])?, "# keep\ngain: 4\n");
            let (mut records, warnings) = recoveries(&root)?;
            assert!(!warnings.is_empty());
            let recovery = records.remove(0);
            assert_eq!(
                recovery.targets.len(),
                2,
                "snapshots include the never-written second file"
            );
            assert!(recovery.details.iter().any(|s| s.contains("not applied")));
            // Reopening is observational; it never blindly resumes or rolls back.
            Browser::open(&root)?;
            assert_eq!(fs::read_to_string(&paths[1])?, "# keep\ngain: 4\n");
            let mut retry = Review::prepare(
                &root,
                recovery.targets,
                rollback,
                "recovery".into(),
                Some(recovery.id),
            );
            assert!(retry.errors.is_empty());
            retry.confirm(&root, &original.id)?;
            assert!(recoveries(&root)?.0.is_empty());
            assert_eq!(
                fs::read_to_string(&paths[0])?,
                if rollback {
                    "# keep\ngain: 3\n"
                } else {
                    "# keep\ngain: 1\n"
                }
            );
            assert_eq!(
                fs::read_to_string(&paths[1])?,
                if rollback {
                    "# keep\ngain: 4\n"
                } else {
                    "# keep\ngain: 1\n"
                }
            );
        }
        Ok(())
    }

    #[test]
    fn corrupt_or_active_batch_journals_never_offer_an_unverified_restore() -> Result<()> {
        let (_temp, root, paths) = fixture("session-corrupt-batch")?;
        let session = populated(&root, &paths)?;
        let mut review = Review::prepare(
            &root,
            Browser::open(&root)?.targets()?,
            true,
            "restore".into(),
            None,
        );
        BATCH_FAIL.with(|p| p.set(Some(0)));
        review.confirm(&root, &session.id)?;
        let recovery = recoveries(&root)?.0.remove(0);
        let dir = directory(&root, "config-restores")?.join(&recovery.id);
        let live = lock(&dir.join("active.lock"), false)?;
        let mut browser = Browser::open(&root)?;
        browser.file_filter = Some("config/a.yaml".into());
        browser.filter = "unrelated search".into();
        browser.key(KeyCode::Char('u'), KeyModifiers::NONE, 10);
        assert_eq!(
            browser.visible.len(),
            1,
            "recovery mode must clear stale file/search filters"
        );
        assert!(browser.targets().is_err());
        drop(live);
        fs::write(dir.join("plan.json"), "{}")?;
        let (records, warnings) = recoveries(&root)?;
        assert!(records.is_empty());
        assert!(warnings.iter().any(|s| s.contains("recovery blocked")));
        assert_eq!(fs::read_to_string(&paths[0])?, "# keep\ngain: 3\n");
        assert_eq!(fs::read_to_string(&paths[1])?, "# keep\ngain: 4\n");
        Ok(())
    }
}
