//! Private, append-only configuration journal. No Git objects or refs are made:
//! records live only below the repository's actual Git administrative directory.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn time_text(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "unknown time".into())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub name: String,
    pub before: String,
    pub after: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    version: u32,
    pub id: String,
    pub timestamp: i64,
    pub path: String,
    pub branch: String,
    pub commit: String,
    pub note: String,
    pub restored_from: Option<String>,
    pub changes: Vec<Change>,
    pub before: String,
    pub after: String,
    before_hash: String,
    after_hash: String,
    checksum: String,
    pub unix_mode: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Summary {
    pub id: String,
    pub timestamp: i64,
    pub branch: String,
    pub commit: String,
    pub note: String,
    pub state: String,
    pub note_group: Option<String>,
    pub changes: usize,
    pub path: String,
    pub module: String,
    pub session: Option<String>,
    file: PathBuf,
}

pub struct Store {
    dir: PathBuf,
    relative: String,
    branch: String,
    commit: String,
    module: Option<String>,
    session: Option<String>,
}

pub(crate) fn hash(text: &str) -> Result<String> {
    // Hash only; never write a blob to Git's object database.
    Ok(git2::Oid::hash_object(git2::ObjectType::Blob, text.as_bytes())?.to_string())
}

pub(crate) fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if !m.is_dir() || m.file_type().is_symlink() => {
            bail!("history directory was replaced: {}", path.display())
        }
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => sync_dir(path.parent().context("history directory has no parent")?)?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return private_dir(path),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path; // File data is synced; directory fsync is not portable on Windows.
    Ok(())
}

impl Store {
    pub(crate) fn with_session(mut self, session: Option<String>) -> Self {
        self.session = session;
        self
    }
    pub(crate) fn refresh_context(&mut self, path: &Path) -> Result<()> {
        let repo = git2::Repository::discover(path.parent().context("config has no parent")?)?;
        self.branch = crate::gitx::head_info(&repo)?.describe();
        self.commit = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .map(|id| id.to_string())
            .unwrap_or_default();
        Ok(())
    }
    pub fn open(path: &Path) -> Result<Self> {
        let repo = git2::Repository::discover(path.parent().context("config has no parent")?)
            .context("configuration history requires a Git working checkout")?;
        Self::open_in(
            path,
            repo.workdir()
                .context("configuration is in a bare repository")?,
        )
    }

    /// Explicit scope is the checkout opened in taid, not the nearest submodule.
    pub fn open_in(path: &Path, scope: &Path) -> Result<Self> {
        let repo = git2::Repository::discover(path.parent().context("config has no parent")?)?;
        let root = scope.canonicalize()?;
        let full = path.canonicalize()?;
        let relative = full
            .strip_prefix(&root)?
            .to_str()
            .context("config path is not UTF-8")?
            .to_owned();
        validate_relative(&relative)?;
        let owner = repo
            .workdir()
            .context("configuration is in a bare repository")?
            .canonicalize()?;
        let module = owner
            .strip_prefix(&root)?
            .to_str()
            .context("module path is not UTF-8")?
            .to_owned();
        let module = if module.is_empty() {
            ".".into()
        } else {
            module
        };
        let histories = history_dir(&root)?;
        let dir = histories.join(hash(&relative)?);
        private_dir(&dir)?;
        let branch = crate::gitx::head_info(&repo)?.describe();
        let commit = repo
            .head()
            .ok()
            .and_then(|h| h.target())
            .map(|oid| oid.to_string())
            .unwrap_or_default();
        let store = Self {
            dir,
            relative,
            branch,
            commit,
            module: Some(module.clone()),
            session: None,
        };
        // Import this file's previous nearest-repository journal without deleting it.
        let old_base = repo.path().canonicalize()?.join("taid/config-history");
        if old_base != histories {
            let local = full
                .strip_prefix(&owner)?
                .to_str()
                .context("config path is not UTF-8")?;
            let source = old_base.join(hash(local)?);
            if source.exists() {
                store.import_bucket(&source, local, &module)?;
            }
        }
        Ok(store)
    }

    fn import_bucket(&self, source: &Path, local: &str, prefix: &str) -> Result<()> {
        if source == self.dir {
            return Ok(());
        }
        reject_symlink_dir(source)?;
        let old = Self {
            dir: source.into(),
            relative: local.into(),
            branch: String::new(),
            commit: String::new(),
            module: None,
            session: None,
        };
        let _old_lock = old.lock()?;
        let _new_lock = self.lock()?;
        for file in fs::read_dir(source)? {
            let file = file?.path();
            let extension = file.extension().and_then(|s| s.to_str()).unwrap_or("");
            if !matches!(extension, "json" | "pending" | "interrupted") {
                continue;
            }
            let mut record = old
                .read(&file)
                .context("legacy history remains untouched; import refused")?;
            record.path = self.relative.clone();
            record.version = 2;
            record.module = Some(match record.module.as_deref() {
                None | Some(".") => prefix.into(),
                Some(inner) => format!("{prefix}/{inner}"),
            });
            record.checksum.clear();
            record.checksum = hash(&serde_json::to_string(&record)?)?;
            // A recovered/imported entry must not be reintroduced as pending on
            // every browse just because the preserved legacy copy is still pending.
            let existing = ["json", "interrupted", "pending"]
                .into_iter()
                .map(|ext| self.dir.join(format!("{}.{}", record.id, ext)))
                .find(|p| p.exists());
            if let Some(existing) = existing {
                let previous = self.read(&existing)?;
                if previous.checksum != record.checksum {
                    bail!("history ID collision; neither record was overwritten");
                }
                continue;
            }
            let temporary = self.dir.join(format!("{}.import", record.id));
            let target = self.dir.join(format!("{}.{}", record.id, extension));
            if temporary.exists() {
                if self.read(&temporary)?.checksum != record.checksum {
                    bail!("interrupted history import conflicts; originals remain untouched");
                }
            } else {
                write_private(&temporary, &serde_json::to_vec(&record)?)?;
            }
            fs::rename(temporary, target)?;
            sync_dir(&self.dir)?;
        }
        Ok(())
    }

    pub fn list_global(scope: &Path, modules: &[PathBuf]) -> Result<(Vec<Summary>, Vec<String>)> {
        let root = scope.canonicalize()?;
        let base = history_dir(&root)?;
        let mut warnings = Vec::new();
        // Bring older submodule-local journals into the opened checkout's scope.
        let mut queue = modules.to_vec();
        queue.push(root.clone());
        let mut visited = std::collections::HashSet::new();
        while let Some(module) = queue.pop() {
            let module = match module.canonicalize() {
                Ok(m) => m,
                Err(e) => {
                    warnings.push(e.to_string());
                    continue;
                }
            };
            if !visited.insert(module.clone()) {
                continue;
            }
            if !module.starts_with(&root) {
                warnings.push("Module outside opened checkout was not imported".into());
                continue;
            }
            if let Err(e) = import_module(&root, &module, &base) {
                warnings.push(format!("{}: {e:#}", module.display()));
            }
            if let Ok(repo) = git2::Repository::open(&module) {
                if let Ok(submodules) = repo.submodules() {
                    for sub in submodules {
                        if let Ok(child) = sub.open() {
                            if let Some(workdir) = child.workdir() {
                                queue.push(workdir.into());
                            }
                        }
                    }
                }
            }
        }
        let mut entries = Vec::new();
        for bucket in fs::read_dir(&base)? {
            let bucket = bucket?.path();
            if let Err(e) = reject_symlink_dir(&bucket) {
                warnings.push(format!("{e:#}"));
                continue;
            }
            let mut relative: Option<String> = None;
            for file in fs::read_dir(&bucket)? {
                let file = file?.path();
                if matches!(
                    file.extension().and_then(|s| s.to_str()),
                    Some("json" | "pending" | "interrupted")
                ) {
                    // Identity can still be displayed for a corrupt record,
                    // but load/restore always requires its verified checksum.
                    if let Ok(Ok(value)) = fs::symlink_metadata(&file)
                        .and_then(|m| {
                            if m.is_file()
                                && !m.file_type().is_symlink()
                                && m.len() <= 32 * 1024 * 1024
                            {
                                fs::read(&file)
                            } else {
                                Err(std::io::Error::other("invalid record"))
                            }
                        })
                        .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes))
                    {
                        if let Some(path) = value["path"].as_str() {
                            if validate_relative(path).is_ok()
                                && hash(path)? == bucket.file_name().unwrap().to_string_lossy()
                            {
                                relative = Some(path.into());
                                break;
                            }
                        }
                    }
                }
            }
            let Some(relative) = relative else {
                warnings.push(format!(
                    "Unreadable history bucket retained: {}",
                    bucket.display()
                ));
                continue;
            };
            let store = Self {
                dir: bucket,
                relative: relative.clone(),
                branch: String::new(),
                commit: String::new(),
                module: None,
                session: None,
            };
            match resolve_config(&root, &relative).and_then(|path| store.recover(&path)) {
                Ok(messages) => {
                    warnings.extend(messages.into_iter().map(|m| format!("{relative}: {m}")))
                }
                Err(e) => warnings.push(format!("{relative}: recovery unavailable: {e:#}")),
            }
            match store.list() {
                Ok(records) => entries.extend(records),
                Err(e) => warnings.push(format!("{relative}: {e:#}")),
            }
        }
        entries.sort_by(|a, b| b.id.cmp(&a.id).then_with(|| a.path.cmp(&b.path)));
        Ok((entries, warnings))
    }

    pub fn load_global(scope: &Path, summary: &Summary) -> Result<Record> {
        validate_relative(&summary.path)?;
        let expected = history_dir(scope)?.join(hash(&summary.path)?);
        reject_symlink_dir(&expected)?;
        if summary.file.parent() != Some(&expected) {
            bail!("history entry is outside the opened checkout's journal");
        }
        let store = Self {
            dir: expected,
            relative: summary.path.clone(),
            branch: String::new(),
            commit: String::new(),
            module: None,
            session: None,
        };
        store.load(summary)
    }
}

/// Global timeline state is independent of any currently open parameter form.
pub struct Browser {
    pub root: PathBuf,
    pub entries: Vec<Summary>,
    pub visible: Vec<usize>,
    pub scroll: crate::app::ScrollState,
    pub record: Option<Box<Record>>,
    pub compare_current: bool,
    pub before: bool,
    pub diff_scroll: usize,
    pub diff_focus: bool,
    pub notes_focus: bool,
    pub notes_scroll: usize,
    pub filter: String,
    pub filtering: bool,
    pub file_filter: Option<String>,
    pub notice: String,
    pub warnings: Vec<String>,
    pub show_warnings: bool,
    pub rows: Vec<HistoryRow>,
    level: Level,
    pub recoveries: Vec<crate::session::Recovery>,
    pub recovery_view: bool,
    pub description_input: Option<String>,
    infos: std::collections::BTreeMap<String, crate::session::Info>,
    pub repository_events: Vec<crate::repository_history::Event>,
}

pub enum HistoryRow {
    Session(String, Vec<usize>),
    File(Vec<usize>),
    Save(usize),
    Recovery(usize),
    Repository(usize),
}

enum Level {
    Sessions,
    Files(String),
    Saves(String, String),
    Legacy,
}

pub enum BrowserAction {
    None,
    Close,
    Restore,
    RestoreBatch,
}

impl Browser {
    pub fn open(root: &Path) -> Result<Self> {
        let (entries, warnings) = Store::list_global(root, &[])?;
        let len = entries.len();
        let grouped = entries.iter().any(|e| e.session.is_some());
        let mut browser = Self {
            root: root.canonicalize()?,
            entries,
            visible: (0..len).collect(),
            scroll: crate::app::ScrollState::new(len),
            record: None,
            compare_current: false,
            before: false,
            diff_scroll: 0,
            diff_focus: false,
            notes_focus: false,
            notes_scroll: 0,
            filter: String::new(),
            filtering: false,
            file_filter: None,
            notice:
                "Repository actions and configuration saves in this opened checkout, including submodules · local only"
                    .into(),
            warnings,
            show_warnings: false,
            rows: Vec::new(),
            level: if grouped {
                Level::Sessions
            } else {
                Level::Legacy
            },
            recoveries: Vec::new(),
            recovery_view: false,
            description_input: None,
            infos: std::collections::BTreeMap::new(),
            repository_events: Vec::new(),
        };
        browser.read_metadata();
        if !browser.repository_events.is_empty() {
            browser.level = Level::Sessions;
        }
        browser.apply_filter();
        Ok(browser)
    }

    fn read_metadata(&mut self) {
        self.infos.clear();
        match crate::repository_history::events(&self.root) {
            Ok((events, warnings)) => {
                self.repository_events = events;
                self.warnings.extend(warnings);
            }
            Err(e) => self.warnings.push(format!("Repository history: {e:#}")),
        }
        for event in &self.repository_events {
            let id = &event.intent.session;
            if self.infos.contains_key(id) {
                continue;
            }
            match crate::session::info(&self.root, id) {
                Ok(info) => {
                    self.infos.insert(id.clone(), info);
                }
                Err(e) => self.warnings.push(format!("Session {id}: {e:#}")),
            }
        }
        for entry in &self.entries {
            if let Some(id) = &entry.session {
                if !self.infos.contains_key(id) {
                    match crate::session::info(&self.root, id) {
                        Ok(info) => {
                            self.infos.insert(id.clone(), info);
                        }
                        Err(e) => self.warnings.push(format!("Session {id}: {e:#}")),
                    }
                }
            }
        }
        match crate::session::recoveries(&self.root) {
            Ok((recoveries, warnings)) => {
                self.recoveries = recoveries;
                self.warnings.extend(warnings);
            }
            Err(e) => self
                .warnings
                .push(format!("Restore recovery catalogue: {e:#}")),
        }
    }

    fn indices(&self, row: &HistoryRow) -> Vec<usize> {
        match row {
            HistoryRow::Session(_, indices) | HistoryRow::File(indices) => indices.clone(),
            HistoryRow::Save(i) => vec![*i],
            HistoryRow::Recovery(_) | HistoryRow::Repository(_) => Vec::new(),
        }
    }

    pub fn selected_row(&self) -> Option<&HistoryRow> {
        self.visible
            .get(self.scroll.selected())
            .and_then(|&i| self.rows.get(i))
    }

    pub fn selected(&self) -> Option<&Summary> {
        self.selected_row()
            .and_then(|row| self.indices(row).first().copied())
            .and_then(|i| self.entries.get(i))
    }

    pub fn location(&self) -> String {
        if self.recovery_view {
            return "Unfinished restores · r resume / b then r roll back".into();
        }
        match &self.level {
            Level::Sessions => {
                "Sessions (Enter expands → repository actions and config files)".into()
            }
            Level::Files(id) => {
                format!(
                    "Session {id} · repository actions / files · Enter individual saves · Backspace up"
                )
            }
            Level::Saves(_, path) => format!("{path} · individual saves · Backspace up"),
            Level::Legacy => {
                "Legacy saves (original session boundaries unknown) · Backspace sessions".into()
            }
        }
    }

    pub fn row_text(&self, index: usize) -> String {
        match &self.rows[index] {
            HistoryRow::Session(id, indices) => {
                let files: std::collections::HashSet<_> =
                    indices.iter().map(|&i| &self.entries[i].path).collect();
                let mut modules: std::collections::HashSet<_> = indices
                    .iter()
                    .map(|&i| self.entries[i].module.as_str())
                    .collect();
                for event in self
                    .repository_events
                    .iter()
                    .filter(|e| e.intent.session == *id)
                {
                    modules.insert(event.intent.subject.as_str());
                    for before in &event.intent.before {
                        if event
                            .outcome
                            .as_ref()
                            .and_then(|o| o.after.iter().find(|s| s.module == before.module))
                            .is_some_and(|a| !a.checkout_eq(before))
                        {
                            modules.insert(before.module.as_str());
                        }
                    }
                }
                let (time, state, description) = self
                    .infos
                    .get(id)
                    .map(|info| (info.started, info.state.as_str(), info.description.as_str()))
                    .unwrap_or((
                        indices
                            .first()
                            .map(|&i| self.entries[i].timestamp)
                            .or_else(|| {
                                self.repository_events
                                    .iter()
                                    .find(|e| e.intent.session == *id)
                                    .map(|e| e.intent.timestamp)
                            })
                            .unwrap_or_default(),
                        if id == "legacy" {
                            "legacy"
                        } else {
                            "metadata unavailable"
                        },
                        "",
                    ));
                let end = self
                    .infos
                    .get(id)
                    .and_then(|info| info.ended)
                    .map(|t| {
                        format!(
                            " → {}",
                            chrono::DateTime::from_timestamp(t, 0)
                                .map(|t| t.format("%H:%M:%S UTC").to_string())
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                let actions = self
                    .repository_events
                    .iter()
                    .filter(|e| e.intent.session == *id)
                    .count();
                format!(
                    "Session · {}{}{}{} · {} modules / {} files / {} saves · {actions} repository actions · {}{end}",
                    if description.is_empty() {
                        String::new()
                    } else {
                        format!("{description} · ")
                    },
                    state,
                    if indices.iter().any(|&i| self.entries[i].state != "saved") {
                        " / uncertain saves"
                    } else {
                        ""
                    },
                    if self
                        .repository_events
                        .iter()
                        .any(|e| e.intent.session == *id
                            && e.outcome.as_ref().is_none_or(|o| !o.success))
                    {
                        " / uncertain actions"
                    } else {
                        ""
                    },
                    modules.len(),
                    files.len(),
                    indices.len(),
                    time_text(time)
                )
            }
            HistoryRow::File(indices) => format!(
                "{} · {} saves · {}",
                self.entries[indices[0]].path,
                indices.len(),
                self.entries[indices[0]].module
            ),
            HistoryRow::Save(i) => {
                let e = &self.entries[*i];
                format!(
                    "{} · {} · {} fields · {} · {}",
                    time_text(e.timestamp),
                    e.path,
                    e.changes,
                    e.state,
                    e.note
                )
            }
            HistoryRow::Recovery(i) => {
                let r = &self.recoveries[*i];
                format!(
                    "{} · {} · {} · {}",
                    time_text(r.timestamp),
                    r.label,
                    if r.active {
                        "active, restore blocked"
                    } else {
                        "interrupted"
                    },
                    r.state
                )
            }
            HistoryRow::Repository(i) => {
                let event = &self.repository_events[*i];
                format!(
                    "{} · {} · {} · {} → {} · {}",
                    time_text(event.intent.timestamp),
                    event.intent.kind,
                    event.intent.subject,
                    event.intent.requested,
                    event
                        .outcome
                        .as_ref()
                        .map(|o| o.detail.as_str())
                        .unwrap_or("outcome unknown"),
                    event.status
                )
            }
        }
    }

    pub fn selected_label(&self) -> String {
        self.visible
            .get(self.scroll.selected())
            .map(|&i| self.row_text(i))
            .unwrap_or_default()
    }

    pub fn recovery_id(&self) -> Option<String> {
        if let Some(HistoryRow::Recovery(i)) = self.selected_row() {
            Some(self.recoveries[*i].id.clone())
        } else {
            None
        }
    }

    pub fn reload(&mut self) -> Result<()> {
        let selected = self.selected_row().map(|row| self.row_key(row));
        let (entries, warnings) = Store::list_global(&self.root, &[])?;
        self.entries = entries;
        self.warnings = warnings;
        self.read_metadata();
        if matches!(self.level, Level::Legacy)
            && self.entries.is_empty()
            && !self.repository_events.is_empty()
        {
            self.level = Level::Sessions;
        }
        self.apply_filter();
        if let Some(key) = selected {
            if let Some(index) = self
                .visible
                .iter()
                .position(|&i| self.row_key(&self.rows[i]) == key)
            {
                self.select(index);
            }
        }
        Ok(())
    }

    fn row_key(&self, row: &HistoryRow) -> String {
        match row {
            HistoryRow::Session(id, _) => format!("session:{id}"),
            HistoryRow::File(indices) => {
                let e = &self.entries[indices[0]];
                format!(
                    "file:{}:{}",
                    e.session.as_deref().unwrap_or("legacy"),
                    e.path
                )
            }
            HistoryRow::Save(i) => {
                format!("save:{}:{}", self.entries[*i].path, self.entries[*i].id)
            }
            HistoryRow::Recovery(i) => format!("recovery:{}", self.recoveries[*i].id),
            HistoryRow::Repository(i) => {
                format!("repository:{}", self.repository_events[*i].intent.id)
            }
        }
    }

    fn event_indices(&self) -> Vec<usize> {
        match self.selected_row() {
            Some(HistoryRow::Session(id, _)) => self
                .repository_events
                .iter()
                .enumerate()
                .filter(|(_, e)| e.intent.session == *id)
                .map(|(i, _)| i)
                .collect(),
            Some(HistoryRow::Repository(i)) => vec![*i],
            _ => Vec::new(),
        }
    }

    fn selected_session(&self) -> Option<String> {
        match self.selected_row()? {
            HistoryRow::Session(id, _) if id != "legacy" => Some(id.clone()),
            HistoryRow::Repository(i) => Some(self.repository_events[*i].intent.session.clone()),
            _ => self.selected().and_then(|e| e.session.clone()),
        }
    }

    /// Notes for the entire selected session, even while inspecting one action/file.
    /// Save/action notes remain attached to their immutable journal records.
    pub fn notes(&self) -> Vec<(String, String)> {
        let session = self.selected_session();
        let mut changes = Vec::new();
        let mut grouped = std::collections::BTreeMap::new();
        if let Some(info) = session.as_ref().and_then(|id| self.infos.get(id)) {
            for note in info.notes.iter().filter(|n| !n.text.is_empty()) {
                let timestamp = note
                    .id
                    .split('-')
                    .next()
                    .and_then(|s| s.parse::<u128>().ok())
                    .and_then(|n| i64::try_from(n / 1_000_000_000).ok())
                    .unwrap_or(info.started);
                changes.push((
                    timestamp,
                    note.id.clone(),
                    format!("Session note · {}", time_text(timestamp)),
                    note.text.clone(),
                ));
            }
        }
        for entry in &self.entries {
            let belongs = if let Some(id) = &session {
                entry.session.as_ref() == Some(id)
            } else {
                self.selected_row().is_some_and(|row| {
                    self.indices(row).iter().any(|&i| {
                        self.entries[i].id == entry.id && self.entries[i].path == entry.path
                    })
                })
            };
            if belongs && !entry.note.is_empty() {
                if let Some(group) = &entry.note_group {
                    let (timestamp, subjects) = grouped
                        .entry((
                            group.clone(),
                            "Session restore".to_owned(),
                            entry.note.clone(),
                        ))
                        .or_insert_with(|| (entry.timestamp, Vec::new()));
                    *timestamp = (*timestamp).max(entry.timestamp);
                    subjects.push(format!("{} ({})", entry.path, entry.state));
                    continue;
                }
                changes.push((
                    entry.timestamp,
                    entry.id.clone(),
                    format!("Config · {} · {}", entry.path, time_text(entry.timestamp)),
                    entry.note.clone(),
                ));
            }
        }
        for event in &self.repository_events {
            if session.as_deref() == Some(&event.intent.session) && !event.intent.note.is_empty() {
                if let Some(group) = &event.intent.note_group {
                    let key = (
                        group.clone(),
                        if event.intent.kind == "restore repository" {
                            "Session restore".into()
                        } else {
                            event.intent.kind.clone()
                        },
                        event.intent.note.clone(),
                    );
                    let (timestamp, subjects) = grouped
                        .entry(key)
                        .or_insert_with(|| (event.intent.timestamp, Vec::new()));
                    *timestamp = (*timestamp).max(event.intent.timestamp);
                    subjects.push(format!("{} ({})", event.intent.subject, event.status));
                    continue;
                }
                changes.push((
                    event.intent.timestamp,
                    event.intent.id.clone(),
                    format!(
                        "{} · {} · {} · {}",
                        event.intent.kind,
                        event.intent.subject,
                        time_text(event.intent.timestamp),
                        event.status
                    ),
                    event.intent.note.clone(),
                ));
            }
        }
        for ((group, kind, note), (timestamp, subjects)) in grouped {
            changes.push((
                timestamp,
                group,
                format!(
                    "{kind} · {} · {}",
                    subjects.join(", "),
                    time_text(timestamp)
                ),
                note,
            ));
        }
        changes.sort_by(|a, b| (&b.0, &b.1).cmp(&(&a.0, &a.1)));
        changes
            .into_iter()
            .map(|(_, _, title, note)| (title, note))
            .collect()
    }

    pub fn repository_targets(&self) -> Result<Vec<crate::repository_history::Target>> {
        if let Some(HistoryRow::Recovery(i)) = self.selected_row() {
            let id = &self.recoveries[*i].id;
            return Ok(crate::session::recoveries(&self.root)?
                .0
                .into_iter()
                .find(|r| r.id == *id && !r.active)
                .context("recovery cannot be verified")?
                .repository_targets);
        }
        let mut indices = self.event_indices();
        indices.sort_by(|&a, &b| {
            self.repository_events[a]
                .intent
                .id
                .cmp(&self.repository_events[b].intent.id)
        });
        let mut targets =
            std::collections::BTreeMap::<String, crate::repository_history::Target>::new();
        for i in indices {
            let event = crate::repository_history::verify(&self.root, &self.repository_events[i])?;
            if event.status == "active" {
                bail!("repository action is still active; wait and reload history");
            }
            for before in &event.intent.before {
                let after = event
                    .outcome
                    .as_ref()
                    .and_then(|o| o.after.iter().find(|s| s.module == before.module))
                    .cloned();
                // Keep unchanged endpoints too: a subsequent action may change this module.
                let target = targets.entry(before.module.clone()).or_insert_with(|| {
                    crate::repository_history::Target {
                        before: before.clone(),
                        after: after.clone(),
                        source: format!("repository:{}", event.intent.id),
                        uncertain: false,
                    }
                });
                target.after = after;
                target.uncertain |= event.outcome.as_ref().is_none_or(|o| !o.success);
            }
        }
        Ok(targets.into_values().collect())
    }

    pub fn targets(&self) -> Result<Vec<crate::session::Target>> {
        let row = self.selected_row().context("no history entry selected")?;
        if let HistoryRow::Recovery(i) = row {
            let r = &self.recoveries[*i];
            if r.active {
                bail!("restore journal is active in another taid instance");
            }
            // Re-read and integrity-check the plan at action time, not just display time.
            let (records, _) = crate::session::recoveries(&self.root)?;
            return Ok(records
                .into_iter()
                .find(|entry| entry.id == r.id && !entry.active)
                .context("restore journal changed or cannot be verified")?
                .targets);
        }
        let mut indices = self.indices(row);
        indices.sort_by(|&a, &b| self.entries[a].id.cmp(&self.entries[b].id));
        let mut targets = std::collections::BTreeMap::<String, crate::session::Target>::new();
        for index in indices {
            let entry = &self.entries[index];
            let record = Store::load_global(&self.root, entry)?;
            let target =
                targets
                    .entry(record.path.clone())
                    .or_insert_with(|| crate::session::Target {
                        path: record.path.clone(),
                        before: record.before.clone(),
                        after: record.after.clone(),
                        source: format!(
                            "session:{}:{}",
                            record.session.as_deref().unwrap_or("legacy"),
                            record.path
                        ),
                        uncertain: false,
                    });
            target.after = record.after;
            target.uncertain |= entry.state != "saved";
        }
        Ok(targets.into_values().collect())
    }

    pub fn select(&mut self, idx: usize) {
        self.scroll.select(idx);
        self.record = None;
        self.diff_scroll = 0;
        self.notes_scroll = 0;
        self.notice = if matches!(self.selected_row(), Some(HistoryRow::Recovery(_))) {
            "r reviews resume (after) · b selects rollback (before) · w shows each file's observed outcome".into()
        } else {
            "Enter expands sessions/files · b before/after · c current diff · r reviews the whole selected row; no automatic restore".into()
        };
        if let Some(entry) = self.selected() {
            match Store::load_global(&self.root, entry) {
                Ok(record) => {
                    self.record = Some(Box::new(record));
                    self.notice = "Enter expands · Backspace up · c current comparison · b before/after · r restores the WHOLE selected row for review".into();
                }
                Err(e) => self.notice = format!("Entry cannot be restored: {e:#}"),
            }
        }
    }

    fn apply_filter(&mut self) {
        self.rows.clear();
        if self.recovery_view {
            self.rows
                .extend((0..self.recoveries.len()).map(HistoryRow::Recovery));
        } else {
            let mut groups = std::collections::BTreeMap::<String, Vec<usize>>::new();
            for (i, e) in self.entries.iter().enumerate() {
                groups
                    .entry(e.session.clone().unwrap_or_else(|| "legacy".into()))
                    .or_default()
                    .push(i);
            }
            for event in &self.repository_events {
                groups.entry(event.intent.session.clone()).or_default();
            }
            match &self.level {
                Level::Sessions => {
                    let legacy = groups.remove("legacy");
                    for (id, indices) in groups.into_iter().rev() {
                        self.rows.push(HistoryRow::Session(id, indices));
                    }
                    if let Some(indices) = legacy {
                        self.rows
                            .push(HistoryRow::Session("legacy".into(), indices));
                    }
                }
                Level::Files(id) => {
                    self.rows.extend(
                        self.repository_events
                            .iter()
                            .enumerate()
                            .filter(|(_, e)| e.intent.session == *id)
                            .map(|(i, _)| HistoryRow::Repository(i)),
                    );
                    let mut files = std::collections::BTreeMap::<String, Vec<usize>>::new();
                    for &i in groups.get(id).into_iter().flatten() {
                        files
                            .entry(self.entries[i].path.clone())
                            .or_default()
                            .push(i);
                    }
                    self.rows.extend(files.into_values().map(HistoryRow::File));
                }
                Level::Saves(id, path) => {
                    self.rows.extend(
                        groups
                            .get(id)
                            .into_iter()
                            .flatten()
                            .filter(|&&i| self.entries[i].path == *path)
                            .map(|&i| HistoryRow::Save(i)),
                    );
                }
                Level::Legacy => {
                    self.rows.extend(
                        self.entries
                            .iter()
                            .enumerate()
                            .filter(|(_, e)| e.session.is_none())
                            .map(|(i, _)| HistoryRow::Save(i)),
                    );
                }
            }
        }
        let needle = self.filter.to_lowercase();
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, row)| {
                let matches = self.indices(row).into_iter().any(|index| {
                    let e = &self.entries[index];
                    self.file_filter.as_ref().is_none_or(|path| path == &e.path)
                        && format!(
                            "{} {} {} {} {}",
                            e.module, e.path, e.note, e.branch, e.commit
                        )
                        .to_lowercase()
                        .contains(&needle)
                });
                let repository_matches = self.repository_events.iter().any(|e| {
                    let belongs = match row {
                        HistoryRow::Session(id, _) => e.intent.session == *id,
                        HistoryRow::Repository(index) => {
                            e.intent.id == self.repository_events[*index].intent.id
                        }
                        _ => false,
                    };
                    belongs
                        && serde_json::to_string(&e.intent)
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&needle)
                });
                let label_matches = self.file_filter.is_none()
                    && (self.row_text(i).to_lowercase().contains(&needle)
                        || repository_matches
                        || match row {
                            HistoryRow::Session(id, _) => self.infos.get(id).is_some_and(|info| {
                                info.notes
                                    .iter()
                                    .any(|note| note.text.to_lowercase().contains(&needle))
                            }),
                            _ => false,
                        });
                let recovery_matches = match row {
                    HistoryRow::Recovery(index) => self.recoveries[*index]
                        .targets
                        .iter()
                        .any(|target| target.path.to_lowercase().contains(&needle)),
                    _ => false,
                };
                (matches || label_matches || recovery_matches).then_some(i)
            })
            .collect();
        self.scroll = crate::app::ScrollState::new(self.visible.len());
        self.select(0);
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(input) = &mut self.description_input {
            input.extend(
                text.chars()
                    .filter(|c| !c.is_control())
                    .take(500usize.saturating_sub(input.chars().count())),
            );
            return;
        }
        if self.filtering {
            self.filter.extend(
                text.chars()
                    .filter(|c| !c.is_control())
                    .take(300usize.saturating_sub(self.filter.chars().count())),
            );
            self.apply_filter();
        }
    }

    pub fn comparison(&self) -> Result<Vec<Change>> {
        self.comparison_impl(false)
    }

    /// Human-readable commits in the changes pane; audit/restore IDs stay intact.
    pub fn display_comparison(&self) -> Result<Vec<Change>> {
        self.comparison_impl(true)
    }

    fn comparison_impl(&self, display: bool) -> Result<Vec<Change>> {
        if !matches!(self.selected_row(), Some(HistoryRow::Save(_))) {
            let targets = self.targets()?;
            let mut changes = Vec::new();
            for target in self.repository_targets()? {
                let (from, to) = if self.compare_current {
                    (
                        crate::repository_history::capture(&self.root, &target.before.module)?,
                        if self.before {
                            target.before
                        } else {
                            target.after.context(
                                "repository outcome unknown; select before (b) to compare",
                            )?
                        },
                    )
                } else {
                    let Some(after) = target.after else {
                        changes.push(Change {
                            name: format!("{} · branch / checkout commit", target.before.module),
                            before: if display {
                                crate::repository_history::display_head(
                                    &self.root,
                                    &target.before.module,
                                    &target.before.head,
                                )
                            } else {
                                target.before.head.describe()
                            },
                            after:
                                "outcome unknown; select before (b) and r for a reviewed restore"
                                    .into(),
                        });
                        continue;
                    };
                    (target.before, after)
                };
                changes.extend(if display {
                    crate::repository_history::display_changes(&self.root, &from, &to, true)
                } else {
                    crate::repository_history::changes(&from, &to, true)
                });
            }
            for i in self.event_indices() {
                let event = &self.repository_events[i];
                let present = |text: &str| {
                    if display && event.intent.kind == "commit pick" {
                        crate::repository_history::display_commit_text(
                            &self.root,
                            &event.intent.subject,
                            text,
                        )
                    } else {
                        text.to_owned()
                    }
                };
                changes.push(Change {
                    name: format!(
                        "{} · {} · {}",
                        event.intent.subject, event.intent.kind, event.intent.id
                    ),
                    before: format!("Requested: {}", present(&event.intent.requested)),
                    after: format!(
                        "{}: {}",
                        event.status,
                        present(
                            event
                                .outcome
                                .as_ref()
                                .map(|o| o.detail.as_str())
                                .unwrap_or("outcome unknown; no automatic rollback")
                        )
                    ),
                });
            }
            for target in targets {
                let (from, to) = if self.compare_current {
                    (
                        fs::read_to_string(resolve_config(&self.root, &target.path)?)?,
                        if self.before {
                            target.before
                        } else {
                            target.after
                        },
                    )
                } else {
                    (target.before, target.after)
                };
                for mut change in crate::config::compare(&from, &to)? {
                    change.name = format!("{} · {}", target.path, change.name);
                    changes.push(change);
                }
            }
            return Ok(changes);
        }
        let record = self
            .record
            .as_ref()
            .context("no verified record selected")?;
        if !self.compare_current {
            return Ok(record.changes.clone());
        }
        let file = resolve_config(&self.root, &record.path)?;
        let current = fs::read_to_string(file)?;
        if current.len() > 2 * 1024 * 1024 {
            bail!("current config exceeds 2 MiB");
        }
        crate::config::compare(
            &current,
            if self.before {
                &record.before
            } else {
                &record.after
            },
        )
    }

    pub fn key(
        &mut self,
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
        page: usize,
    ) -> BrowserAction {
        use crossterm::event::{KeyCode, KeyModifiers};
        if self.description_input.is_some() {
            match code {
                KeyCode::Esc => self.description_input = None,
                KeyCode::Enter => {
                    if let Some(id) = self.selected_session() {
                        let note = self.description_input.take().unwrap();
                        match crate::session::describe(&self.root, &id, &note) {
                            Ok(()) => {
                                if let Err(e) = self.reload() {
                                    self.notice = format!("History reload failed: {e:#}");
                                }
                            }
                            Err(e) => self.notice = format!("Description not saved: {e:#}"),
                        }
                    }
                }
                KeyCode::Backspace => {
                    self.description_input.as_mut().unwrap().pop();
                }
                KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.description_input.as_mut().unwrap().clear()
                }
                KeyCode::Char(c)
                    if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !c.is_control() =>
                {
                    self.paste(&c.to_string())
                }
                _ => {}
            }
            return BrowserAction::None;
        }
        if self.filtering {
            match code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.apply_filter();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.apply_filter();
                }
                KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.clear();
                    self.apply_filter();
                }
                KeyCode::Char(c)
                    if !c.is_control()
                        && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && self.filter.chars().count() < 300 =>
                {
                    self.filter.push(c);
                    self.apply_filter();
                }
                _ => {}
            }
            return BrowserAction::None;
        }
        let previous = self.scroll.selected();
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') => return BrowserAction::Close,
            KeyCode::Char('r') => {
                return if matches!(self.selected_row(), Some(HistoryRow::Save(_))) {
                    BrowserAction::Restore
                } else {
                    BrowserAction::RestoreBatch
                };
            }
            KeyCode::Enter => {
                match self.selected_row() {
                    Some(HistoryRow::Session(id, _)) if id == "legacy" => {
                        self.level = Level::Legacy
                    }
                    Some(HistoryRow::Session(id, _)) => self.level = Level::Files(id.clone()),
                    Some(HistoryRow::File(indices)) => {
                        let e = &self.entries[indices[0]];
                        self.level = Level::Saves(
                            e.session.clone().unwrap_or_else(|| "legacy".into()),
                            e.path.clone(),
                        );
                    }
                    _ => return BrowserAction::None,
                }
                self.apply_filter();
            }
            KeyCode::Backspace => {
                self.level = match &self.level {
                    Level::Saves(id, _) => Level::Files(id.clone()),
                    _ => Level::Sessions,
                };
                self.recovery_view = false;
                self.apply_filter();
            }
            KeyCode::Char('u') => {
                self.recovery_view = !self.recovery_view;
                self.before = false;
                self.filter.clear();
                self.file_filter = None;
                if let Err(e) = self.reload() {
                    self.notice = format!("Recovery reload failed: {e:#}");
                }
            }
            KeyCode::Char('n') => {
                if self.selected_session().is_some() {
                    self.description_input = Some(String::new());
                    self.notes_scroll = 0;
                } else {
                    self.notice =
                        "Select a session to append a note; legacy save notes are retained".into();
                }
            }
            KeyCode::Char('c') => {
                self.compare_current = !self.compare_current;
                self.diff_scroll = 0;
            }
            KeyCode::Char('b') => {
                self.before = !self.before;
                self.diff_scroll = 0;
            }
            KeyCode::Char('w') => {
                self.show_warnings = !self.show_warnings;
                self.diff_scroll = 0;
            }
            KeyCode::Tab => {
                (self.diff_focus, self.notes_focus) = match (self.diff_focus, self.notes_focus) {
                    (false, false) => (true, false),
                    (true, false) => (false, true),
                    _ => (false, false),
                };
            }
            KeyCode::BackTab => {
                (self.diff_focus, self.notes_focus) = match (self.diff_focus, self.notes_focus) {
                    (false, false) => (false, true),
                    (false, true) => (true, false),
                    _ => (false, false),
                };
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('f') => {
                self.file_filter = if self.file_filter.is_some() {
                    None
                } else {
                    self.selected().map(|e| e.path.clone())
                };
                self.apply_filter();
            }
            KeyCode::Char('R') => {
                if let Err(e) = self.reload() {
                    self.notice = format!("Reload failed: {e:#}");
                }
            }
            KeyCode::Down | KeyCode::Char('j') if self.notes_focus => {
                self.notes_scroll = self.notes_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') if self.notes_focus => {
                self.notes_scroll = self.notes_scroll.saturating_sub(1)
            }
            KeyCode::PageDown if self.notes_focus => {
                self.notes_scroll = self.notes_scroll.saturating_add(page.max(1))
            }
            KeyCode::PageUp if self.notes_focus => {
                self.notes_scroll = self.notes_scroll.saturating_sub(page.max(1))
            }
            KeyCode::Home if self.notes_focus => self.notes_scroll = 0,
            KeyCode::End if self.notes_focus => self.notes_scroll = usize::MAX,
            KeyCode::Down | KeyCode::Char('j') if self.diff_focus => {
                self.diff_scroll = self.diff_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') if self.diff_focus => {
                self.diff_scroll = self.diff_scroll.saturating_sub(1)
            }
            KeyCode::PageDown if self.diff_focus => {
                self.diff_scroll = self.diff_scroll.saturating_add(page.max(1))
            }
            KeyCode::PageUp if self.diff_focus => {
                self.diff_scroll = self.diff_scroll.saturating_sub(page.max(1))
            }
            KeyCode::Home if self.diff_focus => self.diff_scroll = 0,
            KeyCode::End if self.diff_focus => self.diff_scroll = usize::MAX,
            KeyCode::Down | KeyCode::Char('j') => self.scroll.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll.move_by(-1),
            KeyCode::PageDown => self.scroll.page_by(page, 1),
            KeyCode::PageUp => self.scroll.page_by(page, -1),
            KeyCode::Home => self.scroll.first(),
            KeyCode::End => self.scroll.last(),
            _ => {}
        }
        if self.scroll.selected() != previous {
            self.select(self.scroll.selected());
        }
        BrowserAction::None
    }
}

pub(crate) fn history_dir(scope: &Path) -> Result<PathBuf> {
    let repo = git2::Repository::open(scope)?;
    let root = scope.canonicalize()?;
    if repo
        .workdir()
        .map(Path::canonicalize)
        .transpose()?
        .as_deref()
        != Some(&root)
    {
        bail!("history scope must be the opened Git checkout's root");
    }
    let admin = repo.path().canonicalize()?;
    if admin.starts_with(&root) && !admin.starts_with(root.join(".git")) {
        bail!(
            "Git metadata is inside a trackable worktree directory; move it outside the worktree before enabling private history"
        );
    }
    let private = admin.join("taid");
    private_dir(&private)?;
    let histories = private.join("config-history");
    private_dir(&histories)?;
    Ok(histories)
}

fn reject_symlink_dir(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.file_type().is_symlink() {
        bail!("unsafe history directory: {}", path.display());
    }
    Ok(())
}

fn validate_relative(path: &str) -> Result<()> {
    if path.is_empty()
        || !Path::new(path)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(p) if p != ".git"))
    {
        bail!("invalid relative configuration path");
    }
    Ok(())
}

pub fn resolve_config(scope: &Path, relative: &str) -> Result<PathBuf> {
    validate_relative(relative)?;
    let mut path = scope.canonicalize()?;
    for part in Path::new(relative).components() {
        path.push(part);
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            bail!("historical config path was replaced by a symlink; no restore allowed");
        }
    }
    let m = fs::symlink_metadata(&path)?;
    if !m.is_file() || m.len() > 2 * 1024 * 1024 {
        bail!("historical config is not a regular file up to 2 MiB");
    }
    Ok(path)
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > 32 * 1024 * 1024 {
        bail!("history record exceeds 32 MiB");
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn import_module(root: &Path, module: &Path, central: &Path) -> Result<()> {
    let module = module.canonicalize()?;
    let prefix = module
        .strip_prefix(root)?
        .to_str()
        .context("module path is not UTF-8")?;
    if prefix.is_empty() {
        return Ok(());
    }
    let repo = git2::Repository::open(&module)?;
    if repo
        .workdir()
        .map(Path::canonicalize)
        .transpose()?
        .as_deref()
        != Some(&module)
    {
        bail!("module is not its expected checkout");
    }
    let old_base = repo.path().canonicalize()?.join("taid/config-history");
    if !old_base.exists() || old_base == central {
        return Ok(());
    }
    reject_symlink_dir(&old_base)?;
    for bucket in fs::read_dir(old_base)? {
        let bucket = bucket?.path();
        reject_symlink_dir(&bucket)?;
        for file in fs::read_dir(&bucket)? {
            let file = file?.path();
            if !matches!(
                file.extension().and_then(|s| s.to_str()),
                Some("json" | "pending" | "interrupted")
            ) {
                continue;
            }
            let r = read_record(&file)?;
            if hash(&r.path)? != bucket.file_name().unwrap().to_string_lossy() {
                bail!("legacy history bucket identity mismatch");
            }
            let relative = format!("{prefix}/{}", r.path);
            validate_relative(&relative)?;
            let dir = central.join(hash(&relative)?);
            private_dir(&dir)?;
            let store = Store {
                dir,
                relative,
                branch: String::new(),
                commit: String::new(),
                module: Some(prefix.into()),
                session: None,
            };
            store.import_bucket(&bucket, &r.path, prefix)?;
            break;
        }
    }
    Ok(())
}

fn read_record(path: &Path) -> Result<Record> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.file_type().is_symlink() || m.len() > 32 * 1024 * 1024 {
        bail!("invalid history record: {}", path.display());
    }
    let record: Record = serde_json::from_slice(&fs::read(path)?)?;
    let mut checked = record.clone();
    checked.checksum.clear();
    validate_relative(&record.path)?;
    if record.checksum != hash(&serde_json::to_string(&checked)?)?
        || !matches!(record.version, 1..=3)
        || record.before.len() > 2 * 1024 * 1024
        || record.after.len() > 2 * 1024 * 1024
        || record.before_hash != hash(&record.before)?
        || record.after_hash != hash(&record.after)?
        || record.id != path.file_stem().and_then(|s| s.to_str()).unwrap_or("")
    {
        bail!("history integrity check failed: {}", path.display());
    }
    Ok(record)
}

impl Store {
    pub(crate) fn lock(&self) -> Result<File> {
        let path = self.dir.join("journal.lock");
        if let Ok(m) = fs::symlink_metadata(&path) {
            if !m.is_file() || m.file_type().is_symlink() {
                bail!("history lock was replaced");
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        file.try_lock_exclusive()
            .context("another taid instance is using this config's history; retry shortly")?;
        Ok(file) // OS releases the lock on normal exit or a crash.
    }

    fn read(&self, path: &Path) -> Result<Record> {
        let record = read_record(path)?;
        if record.path != self.relative {
            bail!("history integrity check failed: {}", path.display());
        }
        Ok(record)
    }

    fn recover_locked(&self, path: &Path) -> Result<Vec<String>> {
        let current = fs::read_to_string(path)?;
        let mut notices = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) == Some("tmp") {
                fs::rename(&path, path.with_extension("interrupted"))?;
                sync_dir(&self.dir)?;
                notices.push("Incomplete journal write detected; that transaction never started replacing the YAML. Inspect History; nothing was restored automatically".into());
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("pending") {
                continue;
            }
            let record = self.read(&path)?;
            let (extension, message) = if current == record.after {
                (
                    "json",
                    "Recovered an interrupted save: its values are on disk and its history is retained",
                )
            } else if current == record.before {
                (
                    "interrupted",
                    "Interrupted save detected: disk matches its before snapshot; nothing was restored automatically",
                )
            } else {
                (
                    "interrupted",
                    "Interrupted save detected: disk differs from both snapshots; inspect History to recover, no file was overwritten",
                )
            };
            fs::rename(&path, path.with_extension(extension))?;
            sync_dir(&self.dir)?;
            notices.push(message.into());
        }
        Ok(notices)
    }

    pub fn recover(&self, path: &Path) -> Result<Vec<String>> {
        let _lock = self.lock()?;
        self.recover_locked(path)
    }

    pub fn list(&self) -> Result<Vec<Summary>> {
        let _lock = self.lock()?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            let state = match path.extension().and_then(|s| s.to_str()) {
                Some("json") => "saved",
                Some("interrupted") => "interrupted / outcome uncertain",
                Some("pending") => "pending recovery",
                _ => continue,
            };
            let r = match self.read(&path) {
                Ok(r) => r,
                Err(e) => {
                    entries.push(Summary {
                        id: path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .into(),
                        timestamp: 0,
                        branch: "unknown".into(),
                        commit: String::new(),
                        note: format!("{e:#}"),
                        state: "corrupt — restore blocked".into(),
                        note_group: None,
                        changes: 0,
                        path: self.relative.clone(),
                        module: self.module.clone().unwrap_or_else(|| ".".into()),
                        session: None,
                        file: path,
                    });
                    continue;
                }
            };
            entries.push(Summary {
                id: r.id,
                timestamp: r.timestamp,
                branch: r.branch,
                commit: r.commit,
                note: r.note,
                note_group: r
                    .restored_from
                    .as_deref()
                    .and_then(|source| source.strip_prefix("batch:"))
                    .and_then(|source| source.split(':').next())
                    .filter(|id| crate::session::valid_id(id).is_ok())
                    .map(str::to_owned),
                changes: r.changes.len(),
                path: r.path,
                module: r
                    .module
                    .unwrap_or_else(|| self.module.clone().unwrap_or_else(|| ".".into())),
                session: r.session,
                state: state.into(),
                file: path,
            });
        }
        entries.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(entries)
    }

    pub fn load(&self, summary: &Summary) -> Result<Record> {
        if summary.file.parent() != Some(&self.dir) {
            bail!("history belongs to a different configuration");
        }
        self.read(&summary.file)
    }

    pub fn save(
        &self,
        path: &Path,
        before: &str,
        after: &str,
        changes: Vec<Change>,
        note: &str,
        restored_from: Option<String>,
    ) -> Result<String> {
        if before.len() > 2 * 1024 * 1024 || after.len() > 2 * 1024 * 1024 {
            bail!("configuration snapshots exceed the 2 MiB limit; nothing was saved");
        }
        let _workspace =
            crate::repository_history::metadata_lock(self.dir.parent().unwrap().parent().unwrap())?;
        let _lock = self.lock()?;
        self.save_locked(path, before, after, changes, note, restored_from)
    }

    /// Caller holds the per-file lock for the entire reviewed batch.
    pub(crate) fn save_locked(
        &self,
        path: &Path,
        before: &str,
        after: &str,
        changes: Vec<Change>,
        note: &str,
        restored_from: Option<String>,
    ) -> Result<String> {
        if before.len() > 2 * 1024 * 1024 || after.len() > 2 * 1024 * 1024 {
            bail!("configuration snapshots exceed the 2 MiB limit; nothing was saved");
        }
        self.recover_locked(path)?;
        if fs::read_to_string(path)? != before {
            bail!("config file changed on disk; reopen it before saving");
        }
        if before == after {
            return Ok("No changes to save".into());
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let id = format!(
            "{:020}-{:010}-{:010}",
            now.as_nanos(),
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        #[cfg(unix)]
        let unix_mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(fs::metadata(path)?.permissions().mode())
        };
        #[cfg(not(unix))]
        let unix_mode = None;
        let mut record = Record {
            version: 3,
            id: id.clone(),
            timestamp: now.as_secs() as i64,
            path: self.relative.clone(),
            branch: self.branch.clone(),
            commit: self.commit.clone(),
            note: note.into(),
            restored_from,
            changes,
            before: before.into(),
            after: after.into(),
            before_hash: hash(before)?,
            after_hash: hash(after)?,
            checksum: String::new(),
            unix_mode,
            module: self.module.clone(),
            session: self.session.clone(),
        };
        record.checksum = hash(&serde_json::to_string(&record)?)?;
        let bytes = serde_json::to_vec(&record)?;
        if bytes.len() > 32 * 1024 * 1024 {
            bail!("history record exceeds the integrity reader's 32 MiB limit; nothing was saved");
        }
        let temporary = self.dir.join(format!("{id}.tmp"));
        let pending = self.dir.join(format!("{id}.pending"));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .context("creating recovery record; YAML has not changed")?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &pending)?;
        sync_dir(&self.dir).context("syncing recovery record; YAML has not changed")?;
        #[cfg(test)]
        failpoint(1)?;
        // The complete before/after record is durable before replacing the YAML.
        if let Err(error) = crate::config::atomic_save(path, before, after) {
            if fs::read_to_string(path).ok().as_deref() != Some(after) {
                return Err(error.context("save did not complete; recovery snapshots are retained"));
            }
            return Ok(format!(
                "Values are on disk; save durability needs recovery on reopen: {error:#}"
            ));
        }
        #[cfg(test)]
        failpoint(2)?;
        if let Err(error) = fs::rename(&pending, pending.with_extension("json"))
            .and_then(|_| sync_dir(&self.dir).map_err(std::io::Error::other))
        {
            return Ok(format!(
                "Saved; history finalization needs recovery on reopen: {error}"
            ));
        }
        Ok("Saved parameter values · local recovery history recorded".into())
    }
}

#[cfg(test)]
thread_local! { static FAIL_POINT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
fn failpoint(point: u8) -> Result<()> {
    if FAIL_POINT.with(|p| {
        if p.get() == point {
            p.set(0);
            true
        } else {
            false
        }
    }) {
        bail!("simulated process interruption");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Form,
        fixture::{self, BASE_TIME, TempDir},
    };

    fn fixture(tag: &str) -> Result<(TempDir, git2::Repository, PathBuf)> {
        let temp = TempDir::new(tag)?;
        let repo = fixture::init_repo(temp.path(), "main")?;
        fixture::commit_file(
            &repo,
            "config/params.yaml",
            "# units\ngain: 1\n",
            "initial",
            BASE_TIME,
        )?;
        let path = temp.path().join("config/params.yaml");
        Ok((temp, repo, path))
    }

    #[test]
    fn snapshots_are_private_not_git_objects_or_cloneable_and_noop_is_not_recorded() -> Result<()> {
        let (temp, repo, path) = fixture("private-history")?;
        let head = repo.head()?.target();
        let indexed = repo
            .index()?
            .get_path(Path::new("config/params.yaml"), 0)
            .unwrap()
            .id;
        let store = Store::open(&path)?;
        store.save(
            &path,
            "# units\ngain: 1\n",
            "# units\ngain: 2\n",
            vec![Change {
                name: "gain".into(),
                before: "1".into(),
                after: "2".into(),
            }],
            "tuning λ",
            None,
        )?;
        let entries = store.list()?;
        assert_eq!(entries.len(), 1);
        let record = store.load(&entries[0])?;
        assert_eq!(record.branch, "main");
        assert_eq!(record.note, "tuning λ");
        assert_eq!(record.before, "# units\ngain: 1\n");
        assert_eq!(record.after, "# units\ngain: 2\n");
        assert!(store.dir.starts_with(repo.path()));
        assert_eq!(repo.head()?.target(), head);
        assert_eq!(
            repo.index()?
                .get_path(Path::new("config/params.yaml"), 0)
                .unwrap()
                .id,
            indexed
        );
        assert!(!repo.odb()?.exists(record.after_hash.parse()?));
        assert!(
            repo.statuses(None)?
                .iter()
                .all(|s| !s.path().unwrap_or("").contains("taid"))
        );
        store.save(
            &path,
            &record.after,
            &record.after,
            Vec::new(),
            "noop",
            None,
        )?;
        assert_eq!(store.list()?.len(), 1);
        let copy = TempDir::new("history-clone")?;
        let cloned =
            git2::Repository::clone(temp.path().to_str().unwrap(), copy.path().join("clone"))?;
        assert!(!cloned.path().join("taid").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&entries[0].file)?.permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&store.dir)?.permissions().mode() & 0o777,
                0o700
            );
        }
        Ok(())
    }

    #[test]
    fn interruptions_before_and_after_replacement_recover_without_overwriting_yaml() -> Result<()> {
        for point in [1, 2] {
            let (_temp, _repo, path) = fixture(&format!("journal-fault-{point}"))?;
            let mut form = Form::open(path.clone())?;
            form.activate();
            form.paste("2");
            form.commit_input()?;
            FAIL_POINT.with(|p| p.set(point));
            assert!(form.save_with_note("Test config change").is_err());
            let disk = fs::read_to_string(&path)?;
            assert_eq!(
                disk,
                if point == 1 {
                    "# units\ngain: 1\n"
                } else {
                    "# units\ngain: 2\n"
                }
            );
            let store = Store::open(&path)?;
            assert_eq!(store.list()?[0].state, "pending recovery");
            drop(form);
            let reopened = Form::open(path.clone())?;
            assert!(!reopened.dirty());
            assert_eq!(fs::read_to_string(&path)?, disk);
            assert!(reopened.notice.contains(if point == 1 {
                "Interrupted"
            } else {
                "Recovered"
            }));
            let summary = &store.list()?[0];
            assert_eq!(
                summary.state,
                if point == 1 {
                    "interrupted / outcome uncertain"
                } else {
                    "saved"
                }
            );
            let record = store.load(summary)?;
            assert_eq!(record.before, "# units\ngain: 1\n");
            assert_eq!(record.after, "# units\ngain: 2\n");
        }
        Ok(())
    }

    #[test]
    fn ambiguous_interrupted_save_preserves_external_edits_and_snapshots() -> Result<()> {
        let (_temp, _repo, path) = fixture("journal-external")?;
        let store = Store::open(&path)?;
        FAIL_POINT.with(|p| p.set(1));
        assert!(
            store
                .save(
                    &path,
                    "# units\ngain: 1\n",
                    "# units\ngain: 2\n",
                    Vec::new(),
                    "",
                    None
                )
                .is_err()
        );
        fs::write(&path, "# external\ngain: 9\n")?;
        let f = Form::open(path.clone())?;
        assert!(f.notice.contains("differs from both"));
        assert_eq!(fs::read_to_string(&path)?, "# external\ngain: 9\n");
        assert!(store.list()?[0].state.contains("interrupted"));
        Ok(())
    }

    #[test]
    fn storage_failure_concurrent_lock_and_external_edits_refuse_saving() -> Result<()> {
        let (_temp, repo, path) = fixture("journal-refusals")?;
        fs::write(repo.path().join("taid"), "not a directory")?;
        let mut f = Form::open(path.clone())?;
        f.activate();
        f.paste("2");
        f.commit_input()?;
        assert!(f.save_with_note("Test config change").is_err());
        assert_eq!(fs::read_to_string(&path)?, "# units\ngain: 1\n");
        fs::remove_file(repo.path().join("taid"))?;
        let store = Store::open(&path)?;
        let lock = store.lock()?;
        assert!(f.save_with_note("Test config change").is_err());
        drop(lock);
        fs::write(&path, "gain: 8\n")?;
        assert!(f.save_with_note("Test config change").is_err());
        assert_eq!(fs::read_to_string(&path)?, "gain: 8\n");
        assert!(store.list()?.is_empty());
        Ok(())
    }

    #[test]
    fn corrupted_snapshot_or_metadata_is_never_restored() -> Result<()> {
        let (_temp, _repo, path) = fixture("journal-corrupt")?;
        let store = Store::open(&path)?;
        store.save(
            &path,
            "# units\ngain: 1\n",
            "gain: 2\n",
            Vec::new(),
            "",
            None,
        )?;
        let entry = store.list()?.remove(0);
        let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&entry.file)?)?;
        value["note"] = serde_json::Value::String("corrupted metadata".into());
        fs::write(&entry.file, serde_json::to_vec(&value)?)?;
        assert!(store.load(&entry).is_err());
        assert!(store.list()?[0].state.contains("corrupt"));
        assert_eq!(fs::read_to_string(&path)?, "gain: 2\n");
        Ok(())
    }

    #[test]
    fn gitfile_checkout_uses_actual_admin_directory() -> Result<()> {
        let (temp, repo, path) = fixture("journal-gitfile")?;
        let admin_holder = TempDir::new("journal-admin")?;
        let actual = admin_holder.path().join("metadata");
        repo.config()?
            .set_str("core.worktree", temp.path().to_str().unwrap())?;
        drop(repo);
        fs::rename(temp.path().join(".git"), &actual)?;
        fs::write(
            temp.path().join(".git"),
            format!("gitdir: {}\n", actual.display()),
        )?;
        let store = Store::open(&path)?;
        assert!(store.dir.starts_with(actual.canonicalize()?));
        store.save(
            &path,
            "# units\ngain: 1\n",
            "gain: 2\n",
            Vec::new(),
            "",
            None,
        )?;
        assert_eq!(store.list()?.len(), 1);
        assert!(fs::metadata(temp.path().join(".git"))?.is_file());
        Ok(())
    }

    #[test]
    fn incomplete_or_symlinked_journals_never_overwrite_configuration() -> Result<()> {
        let (_temp, repo, path) = fixture("journal-incomplete")?;
        let store = Store::open(&path)?;
        fs::write(store.dir.join("incomplete.tmp"), b"{ partial JSON")?;
        let f = Form::open(path.clone())?;
        assert!(f.notice.contains("Incomplete"));
        assert_eq!(store.list()?[0].state, "corrupt — restore blocked");
        assert_eq!(fs::read_to_string(&path)?, "# units\ngain: 1\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let other = crate::fixture::TempDir::new("journal-symlink-target")?;
            // Replace only this temporary fixture's private directory, never a real checkout.
            fs::rename(repo.path().join("taid"), repo.path().join("old-taid"))?;
            symlink(other.path(), repo.path().join("taid"))?;
            let mut f = Form::open(path.clone())?;
            f.activate();
            f.paste("2");
            f.commit_input()?;
            assert!(f.save_with_note("Test config change").is_err());
            assert_eq!(fs::read_to_string(&path)?, "# units\ngain: 1\n");
            assert!(fs::read_dir(other.path())?.next().is_none());
        }
        Ok(())
    }

    fn stack(tag: &str) -> Result<(TempDir, crate::fixture::SuperFixture, Vec<PathBuf>)> {
        let temp = TempDir::new(tag)?;
        let fixture = fixture::make_super(
            &temp.path().join("stack"),
            &[
                crate::fixture::SubSpec {
                    name: "subA",
                    msgs: &["a"],
                    recorded: None,
                    drop_workdir: false,
                },
                crate::fixture::SubSpec {
                    name: "subB",
                    msgs: &["b"],
                    recorded: None,
                    drop_workdir: false,
                },
            ],
        )?;
        let root = fixture.repo.workdir().unwrap();
        let paths: Vec<_> = [
            "config/main.yaml",
            "subA/config/gains.yaml",
            "subB/config/gains.yaml",
        ]
        .into_iter()
        .map(|p| root.join(p))
        .collect();
        for path in &paths {
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, "gain: 1\n")?;
        }
        Ok((temp, fixture, paths))
    }

    #[test]
    fn global_scope_registers_main_and_submodules_only_in_opened_checkout_metadata() -> Result<()> {
        let (_temp, fixture, paths) = stack("global-registration")?;
        let root = fixture.repo.workdir().unwrap();
        let original_head = fixture.repo.head()?.target();
        for (i, path) in paths.iter().enumerate() {
            let mut form = Form::open_in(path.clone(), root)?;
            form.activate();
            form.paste(&(i + 2).to_string());
            form.save_with_note("Test config change")?;
        }
        let (entries, warnings) = Store::list_global(root, &[])?;
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(entries.len(), 3);
        assert!(
            entries
                .iter()
                .any(|e| e.path == "config/main.yaml" && e.module == ".")
        );
        assert!(
            entries
                .iter()
                .any(|e| e.path == "subA/config/gains.yaml" && e.module == "subA")
        );
        assert!(
            entries
                .iter()
                .any(|e| e.path == "subB/config/gains.yaml" && e.module == "subB")
        );
        for e in &entries {
            assert!(e.file.starts_with(fixture.repo.path()));
            Store::load_global(root, e)?;
        }
        for module in ["subA", "subB"] {
            let repo = git2::Repository::open(root.join(module))?;
            assert!(
                !repo.path().join("taid").exists(),
                "new saves must not create module-local journals"
            );
        }
        assert_eq!(fixture.repo.head()?.target(), original_head);
        assert!(
            fixture
                .repo
                .statuses(None)?
                .iter()
                .all(|s| !s.path().unwrap_or("").contains("taid"))
        );
        Ok(())
    }

    #[test]
    fn legacy_version_one_pending_history_is_imported_without_removal_or_duplicates() -> Result<()>
    {
        let (_temp, fixture, paths) = stack("global-migrate")?;
        let root = fixture.repo.workdir().unwrap();
        let path = &paths[1];
        let mut old_form = Form::open(path.clone())?;
        old_form.activate();
        old_form.paste("2");
        old_form.save_with_note("Test config change")?;
        let old_store = Store::open(path)?;
        let entry = old_store.list()?.remove(0);
        let mut record = old_store.load(&entry)?;
        record.version = 1;
        record.module = None;
        record.checksum.clear();
        record.checksum = hash(&serde_json::to_string(&record)?)?;
        let bytes = serde_json::to_vec(&record)?;
        fs::write(&entry.file, &bytes)?;
        let old_pending = entry.file.with_extension("pending");
        fs::rename(&entry.file, &old_pending)?;
        let (all, warnings) = Store::list_global(root, &[])?;
        assert_eq!(all.len(), 1);
        assert!(warnings.iter().any(|w| w.contains("Recovered")));
        assert_eq!(all[0].path, "subA/config/gains.yaml");
        assert_eq!(all[0].module, "subA");
        assert_eq!(all[0].state, "saved");
        assert_eq!(
            fs::read(&old_pending)?,
            bytes,
            "original legacy copy must remain untouched"
        );
        assert_eq!(fs::read_to_string(path)?, "gain: 2\n");
        let migrated = Store::load_global(root, &all[0])?;
        assert_eq!(migrated.version, 2);
        assert_eq!(migrated.id, record.id);
        assert_eq!(migrated.before, record.before);
        assert_eq!(migrated.after, record.after);
        assert_eq!(
            Store::list_global(root, &[])?.0.len(),
            1,
            "reopening must not reintroduce old pending entries"
        );
        Ok(())
    }

    #[test]
    fn global_browser_filters_modules_notes_and_file_versions() -> Result<()> {
        let (_temp, fixture, paths) = stack("global-filter")?;
        let root = fixture.repo.workdir().unwrap();
        for (i, path) in paths.iter().enumerate() {
            let store = Store::open_in(path, root)?;
            store.save(
                path,
                "gain: 1\n",
                "gain: 2\n",
                vec![Change {
                    name: "gain".into(),
                    before: "1".into(),
                    after: "2".into(),
                }],
                &format!("tuning-{i}"),
                None,
            )?;
        }
        let mut b = Browser::open(root)?;
        assert_eq!(b.entries.len(), 3);
        b.key(
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyModifiers::NONE,
            10,
        );
        b.paste("subA");
        assert_eq!(b.visible.len(), 1);
        assert_eq!(b.selected().unwrap().path, "subA/config/gains.yaml");
        b.key(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
            10,
        );
        b.key(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::NONE,
            10,
        );
        assert_eq!(b.file_filter.as_deref(), Some("subA/config/gains.yaml"));
        b.key(
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyModifiers::NONE,
            10,
        );
        b.key(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
            10,
        );
        assert_eq!(b.visible.len(), 1);
        b.key(
            crossterm::event::KeyCode::Char('f'),
            crossterm::event::KeyModifiers::NONE,
            10,
        );
        assert_eq!(b.visible.len(), 3);
        b.key(
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyModifiers::NONE,
            10,
        );
        b.paste("tuning-0");
        assert_eq!(b.selected().unwrap().module, ".");
        b.key(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
            10,
        );
        b.key(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::NONE,
            10,
        );
        b.key(
            crossterm::event::KeyCode::Char('b'),
            crossterm::event::KeyModifiers::NONE,
            10,
        );
        assert!(b.comparison()?[0].before.starts_with('2'));
        assert!(b.comparison()?[0].after.starts_with('1'));
        Ok(())
    }

    #[test]
    fn global_recovery_and_missing_files_preserve_snapshots_without_blind_restores() -> Result<()> {
        for point in [1, 2] {
            let (_temp, fixture, paths) = stack(&format!("global-recovery-{point}"))?;
            let root = fixture.repo.workdir().unwrap();
            let path = &paths[1];
            let mut form = Form::open_in(path.clone(), root)?;
            form.activate();
            form.paste("2");
            form.commit_input()?;
            FAIL_POINT.with(|p| p.set(point));
            assert!(form.save_with_note("Test config change").is_err());
            let disk = fs::read_to_string(path)?;
            let (all, warnings) = Store::list_global(root, &[])?;
            assert_eq!(all.len(), 1);
            assert!(!warnings.is_empty());
            assert_eq!(fs::read_to_string(path)?, disk);
            let record = Store::load_global(root, &all[0])?;
            assert_eq!(record.path, "subA/config/gains.yaml");
            assert_eq!(record.before, "gain: 1\n");
            assert_eq!(record.after, "gain: 2\n");
            fs::remove_file(path)?; // Only this temporary fixture config.
            let mut browser = Browser::open(root)?;
            assert_eq!(browser.entries.len(), 1);
            assert!(
                browser.record.is_some(),
                "saved snapshots remain viewable after removal"
            );
            browser.compare_current = true;
            assert!(browser.comparison().is_err());
            assert!(resolve_config(root, "../outside.yaml").is_err());
            assert!(resolve_config(root, ".git/config.yaml").is_err());
        }
        Ok(())
    }
}
