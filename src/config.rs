//! YAML parameter forms. Only existing scalar values can change; source ranges
//! preserve keys, nesting, array lengths, comments and all untouched formatting.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyModifiers};
use yaml_rust2::Yaml;
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};

use crate::app::ScrollState;

static SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn discover(root: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, in_config: bool, files: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let name = entry.file_name();
            if kind.is_dir() {
                if matches!(
                    name.to_str(),
                    Some(".git" | "build" | "install" | "log" | "target" | "thirdparty" | "vendor")
                ) || entry.path().join(".git").exists()
                {
                    continue;
                }
                walk(&entry.path(), in_config || name == "config", files)?;
            } else if kind.is_file()
                && in_config
                && matches!(
                    entry.path().extension().and_then(|s| s.to_str()),
                    Some("yaml" | "yml")
                )
            {
                files.push(entry.path());
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, false, &mut files)?;
    files.sort();
    Ok(files)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Boolean,
    Integer,
    Number,
    Text,
    Null,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Boolean => "bool",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Text => "text",
            Self::Null => "null",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Key(String),
    Index(usize),
    Document(usize),
}

pub struct Parameter {
    pub name: String,
    pub group: String,
    pub label: String,
    pub kind: Kind,
    pub value: String,
    pub description: String,
    pub readonly: Option<String>,
    path: Vec<Part>,
    original_value: String,
    span: Option<Range<usize>>,
    style: TScalarStyle,
}

impl Parameter {
    pub fn changed(&self) -> bool {
        self.value != self.original_value
    }

    fn encoded(&self) -> String {
        if self.kind != Kind::Text {
            return self.value.clone();
        }
        if self.style == TScalarStyle::SingleQuoted {
            return format!("'{}'", self.value.replace('\'', "''"));
        }
        let mut result = String::from("\"");
        for c in self.value.chars() {
            match c {
                '"' => result.push_str("\\\""),
                '\\' => result.push_str("\\\\"),
                '\n' => result.push_str("\\n"),
                '\r' => result.push_str("\\r"),
                '\t' => result.push_str("\\t"),
                c if c.is_control() => result.push_str(&format!("\\u{:04x}", c as u32)),
                c => result.push(c),
            }
        }
        result.push('"');
        result
    }
}

pub struct ValueInput {
    pub text: String,
    pub cursor: usize,
    /// Typing replaces the selected current value; cursor movement ends selection.
    pub replace: bool,
}

pub struct Form {
    pub path: PathBuf,
    pub parameters: Vec<Parameter>,
    pub visible: Vec<usize>,
    pub scroll: ScrollState,
    pub editing: Option<ValueInput>,
    pub filter: String,
    pub filtering: bool,
    pub notice: String,
    original: String,
}

impl Form {
    pub fn open(path: PathBuf) -> Result<Self> {
        if !matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("yaml" | "yml")
        ) {
            bail!("only YAML parameter forms are supported");
        }
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 {
            bail!("only regular UTF-8 YAML files up to 2 MiB are supported");
        }
        let original = fs::read_to_string(&path).context("reading YAML configuration")?;
        let parameters = parameters(&original).context("parsing YAML configuration")?;
        let visible = (0..parameters.len()).collect();
        let scroll = ScrollState::new(parameters.len());
        Ok(Self {
            path,
            parameters,
            visible,
            scroll,
            editing: None,
            filter: String::new(),
            filtering: false,
            notice: "Choose a parameter; Enter edits its value or toggles a boolean".into(),
            original,
        })
    }

    pub fn selected(&self) -> Option<&Parameter> {
        self.visible
            .get(self.scroll.selected())
            .and_then(|&i| self.parameters.get(i))
    }

    pub fn dirty(&self) -> bool {
        self.parameters.iter().any(Parameter::changed)
            || self
                .editing
                .as_ref()
                .is_some_and(|input| self.selected().is_some_and(|p| input.text != p.value))
    }

    pub fn changed_count(&self) -> usize {
        self.parameters.iter().filter(|p| p.changed()).count()
    }

    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.visible = self
            .parameters
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                (p.name.to_lowercase().contains(&needle)
                    || p.description.to_lowercase().contains(&needle))
                .then_some(i)
            })
            .collect();
        self.scroll = ScrollState::new(self.visible.len());
    }

    pub fn activate(&mut self) {
        let Some(&i) = self.visible.get(self.scroll.selected()) else {
            return;
        };
        let p = &mut self.parameters[i];
        if let Some(reason) = &p.readonly {
            self.notice = reason.clone();
            return;
        }
        self.notice.clear();
        if p.kind == Kind::Boolean {
            p.value = if p.value == "true" { "false" } else { "true" }.into();
        } else {
            self.editing = Some(ValueInput {
                text: p.value.clone(),
                cursor: p.value.chars().count(),
                replace: true,
            });
        }
    }

    pub fn commit_input(&mut self) -> Result<()> {
        let Some(input) = &self.editing else {
            return Ok(());
        };
        let i = self.visible[self.scroll.selected()];
        let p = &mut self.parameters[i];
        let value = match p.kind {
            Kind::Text => input.text.clone(),
            Kind::Integer => match Yaml::from_str(input.text.trim()) {
                Yaml::Integer(n) => n.to_string(),
                _ => bail!("{} needs a whole number (64-bit integer)", p.label),
            },
            Kind::Number => {
                let value = input.text.trim();
                let n = value
                    .parse::<f64>()
                    .context("enter a valid number, e.g. 0.5 or 1e-3")?;
                if !n.is_finite() {
                    bail!("enter a finite number");
                }
                if value.contains(['.', 'e', 'E']) {
                    value.to_owned()
                } else {
                    format!("{value}.0")
                }
            }
            _ => bail!("this parameter cannot be edited as text"),
        };
        p.value = value;
        self.editing = None;
        self.notice = "Value updated · Ctrl+S writes changes to the YAML file".into();
        Ok(())
    }

    pub fn key(&mut self, code: KeyCode, modifiers: KeyModifiers, page: usize) {
        if self.editing.is_some() {
            if code == KeyCode::Enter {
                if let Err(e) = self.commit_input() {
                    self.notice = format!("{e:#}");
                }
            } else if code == KeyCode::Esc {
                self.editing = None;
                self.notice = "Parameter edit canceled".into();
            } else {
                input_key(self.editing.as_mut().unwrap(), code, modifiers);
            }
            return;
        }
        if self.filtering {
            match code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.clear()
                }
                KeyCode::Char(c)
                    if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.filter.push(c)
                }
                _ => {}
            }
            self.apply_filter();
            return;
        }
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll.move_by(1),
            KeyCode::PageUp => self.scroll.page_by(page, -1),
            KeyCode::PageDown => self.scroll.page_by(page, 1),
            KeyCode::Home => self.scroll.first(),
            KeyCode::End => self.scroll.last(),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('r') => {
                if let Some(&i) = self.visible.get(self.scroll.selected()) {
                    let p = &mut self.parameters[i];
                    p.value = p.original_value.clone();
                    self.notice = "Parameter reset to its last saved value".into();
                }
            }
            _ => {}
        }
        self.scroll.ensure_visible(page.max(1));
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(input) = &mut self.editing {
            if text.contains(['\n', '\r']) {
                self.notice = "Paste a single parameter value, not YAML".into();
                return;
            }
            if input.replace {
                input.text.clear();
                input.cursor = 0;
                input.replace = false;
            }
            let byte = byte_at(&input.text, input.cursor);
            input.text.insert_str(byte, text);
            input.cursor += text.chars().count();
        }
    }

    pub fn text(&self) -> Result<String> {
        let mut text = self.original.clone();
        let mut changes: Vec<_> = self.parameters.iter().filter(|p| p.changed()).collect();
        changes.sort_by_key(|p| std::cmp::Reverse(p.span.as_ref().map(|r| r.start).unwrap_or(0)));
        for p in changes {
            let span = p.span.clone().context("parameter is read-only")?;
            text.replace_range(span, &p.encoded());
        }
        // Check every path, type and resulting value before touching the disk.
        let parsed = parameters(&text)?;
        if parsed.len() != self.parameters.len()
            || parsed
                .iter()
                .zip(&self.parameters)
                .any(|(a, b)| a.path != b.path || a.kind != b.kind || a.value != b.value)
        {
            bail!("parameter changes would alter the YAML schema; nothing was saved");
        }
        Ok(text)
    }

    pub fn save(&mut self) -> Result<()> {
        self.commit_input()?;
        let text = self.text()?;
        atomic_save(&self.path, &self.original, &text)?;
        self.parameters = parameters(&text)?;
        self.original = text;
        self.notice = "Saved parameter values to YAML file".into();
        Ok(())
    }
}

fn byte_at(text: &str, col: usize) -> usize {
    text.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

fn input_key(input: &mut ValueInput, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            input.text.clear();
            input.cursor = 0;
            input.replace = false;
        }
        KeyCode::Left => {
            input.cursor = input.cursor.saturating_sub(1);
            input.replace = false;
        }
        KeyCode::Right => {
            input.cursor = (input.cursor + 1).min(input.text.chars().count());
            input.replace = false;
        }
        KeyCode::Home => {
            input.cursor = 0;
            input.replace = false;
        }
        KeyCode::End => {
            input.cursor = input.text.chars().count();
            input.replace = false;
        }
        KeyCode::Backspace | KeyCode::Delete if input.replace => {
            input.text.clear();
            input.cursor = 0;
            input.replace = false;
        }
        KeyCode::Backspace if input.cursor > 0 => {
            let end = byte_at(&input.text, input.cursor);
            input.cursor -= 1;
            input
                .text
                .replace_range(byte_at(&input.text, input.cursor)..end, "");
        }
        KeyCode::Delete if input.cursor < input.text.chars().count() => {
            let start = byte_at(&input.text, input.cursor);
            input
                .text
                .replace_range(start..byte_at(&input.text, input.cursor + 1), "");
        }
        KeyCode::Char(c)
            if !c.is_control()
                && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if input.replace {
                input.text.clear();
                input.cursor = 0;
                input.replace = false;
            }
            input.text.insert(byte_at(&input.text, input.cursor), c);
            input.cursor += 1;
        }
        _ => {}
    }
}

#[derive(Default)]
struct Events(Vec<(Event, Marker)>);
impl MarkedEventReceiver for Events {
    fn on_event(&mut self, event: Event, marker: Marker) {
        self.0.push((event, marker));
    }
}

fn path_name(path: &[Part]) -> String {
    let mut name = String::new();
    for part in path {
        match part {
            Part::Key(key) => {
                if !name.is_empty() {
                    name.push('.');
                }
                name.push_str(key);
            }
            Part::Index(i) => name.push_str(&format!("[{i}]")),
            Part::Document(i) => name.push_str(&format!("document {}", i + 1)),
        }
    }
    name
}

fn parameters(text: &str) -> Result<Vec<Parameter>> {
    let mut events = Events::default();
    Parser::new_from_str(text).load(&mut events, true)?;
    // yaml-rust2 0.11's markers count characters, despite its index() docs.
    let bytes: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let mut result = Vec::new();
    let mut cursor = 0;
    let documents = events
        .0
        .iter()
        .filter(|(e, _)| *e == Event::DocumentStart)
        .count();
    let mut document = 0;
    let mut previous_end = 0;
    while cursor < events.0.len() {
        if events.0[cursor].0 == Event::DocumentStart {
            let start = bytes[events.0[cursor].1.index()];
            // ROS/PyYAML files use legacy Yes/No and On/Off booleans.
            // An explicit YAML 1.2 directive selects the core scalar schema.
            let legacy = !text[previous_end..start]
                .lines()
                .any(|line| line.trim() == "%YAML 1.2");
            cursor += 1;
            let mut path = if documents > 1 {
                vec![Part::Document(document)]
            } else {
                Vec::new()
            };
            collect_node(
                text,
                &bytes,
                &events.0,
                &mut cursor,
                &mut path,
                false,
                false,
                legacy,
                &mut result,
            )?;
            document += 1;
        } else {
            if events.0[cursor].0 == Event::DocumentEnd {
                previous_end = bytes[events.0[cursor].1.index()];
            }
            cursor += 1;
        }
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn collect_node(
    text: &str,
    bytes: &[usize],
    events: &[(Event, Marker)],
    cursor: &mut usize,
    path: &mut Vec<Part>,
    flow: bool,
    inherited_readonly: bool,
    legacy: bool,
    result: &mut Vec<Parameter>,
) -> Result<()> {
    if path.len() > 64 {
        bail!("YAML nesting exceeds 64 levels");
    }
    let (event, marker) = events.get(*cursor).context("incomplete YAML node")?;
    let start = *bytes
        .get(marker.index())
        .context("invalid YAML source position")?;
    *cursor += 1;
    match event {
        Event::MappingStart(anchor, tag) => {
            let flow = flow || text[start..].starts_with('{');
            let readonly = inherited_readonly || *anchor != 0 || tag.is_some();
            let mut keys = HashSet::new();
            while events
                .get(*cursor)
                .is_some_and(|(e, _)| *e != Event::MappingEnd)
            {
                let (Event::Scalar(key, _, _, _), _) = &events[*cursor] else {
                    bail!("complex YAML keys are not supported in parameter forms");
                };
                if !keys.insert(key.clone()) {
                    bail!("duplicate YAML key `{key}`");
                }
                if key == "<<" {
                    bail!("YAML merge keys are not supported in parameter forms");
                }
                *cursor += 1;
                path.push(Part::Key(key.clone()));
                collect_node(
                    text, bytes, events, cursor, path, flow, readonly, legacy, result,
                )?;
                path.pop();
            }
            *cursor += 1;
        }
        Event::SequenceStart(anchor, tag) => {
            let flow = flow || text[start..].starts_with('[');
            let readonly = inherited_readonly || *anchor != 0 || tag.is_some();
            let mut i = 0;
            while events
                .get(*cursor)
                .is_some_and(|(e, _)| *e != Event::SequenceEnd)
            {
                path.push(Part::Index(i));
                collect_node(
                    text, bytes, events, cursor, path, flow, readonly, legacy, result,
                )?;
                path.pop();
                i += 1;
            }
            *cursor += 1;
        }
        Event::Scalar(value, style, anchor, tag) => {
            let (kind, value) = if *style != TScalarStyle::Plain {
                (Kind::Text, value.clone())
            } else {
                match resolve(value, legacy) {
                    Yaml::Boolean(b) => (Kind::Boolean, b.to_string()),
                    Yaml::Integer(n) => (Kind::Integer, n.to_string()),
                    Yaml::Real(s) => (Kind::Number, s),
                    Yaml::Null => (Kind::Null, value.clone()),
                    _ => (Kind::Text, value.clone()),
                }
            };
            let span = scalar_span(text, start, *style, flow, &value, legacy);
            let readonly = if inherited_readonly || *anchor != 0 || tag.is_some() {
                Some(
                    "Anchored or tagged values are read-only to preserve shared YAML semantics"
                        .into(),
                )
            } else if kind == Kind::Null {
                Some("Null has no specified value type; this parameter is read-only".into())
            } else if span.is_none() || value.contains(['\n', '\r']) {
                Some("Multiline or unsupported YAML values are read-only".into())
            } else {
                None
            };
            let name = path_name(path);
            let label = path
                .last()
                .map(|part| match part {
                    Part::Key(k) => k.clone(),
                    Part::Index(i) => format!("[{i}]"),
                    Part::Document(_) => "value".into(),
                })
                .unwrap_or_else(|| "value".into());
            let group = path_name(&path[..path.len().saturating_sub(1)]);
            let description = description(text, start, span.as_ref().map(|s| s.end));
            result.push(Parameter {
                name,
                group,
                label,
                kind,
                original_value: value.clone(),
                value,
                description,
                readonly,
                path: path.clone(),
                span,
                style: *style,
            });
        }
        Event::Alias(_) => {
            let name = path_name(path);
            result.push(Parameter {
                name: name.clone(),
                label: name,
                group: String::new(),
                kind: Kind::Text,
                value: "<shared alias>".into(),
                original_value: "<shared alias>".into(),
                path: path.clone(),
                description: String::new(),
                readonly: Some("Aliases are read-only; shared values are not modified".into()),
                span: None,
                style: TScalarStyle::Plain,
            });
        }
        _ => bail!("unexpected YAML node"),
    }
    Ok(())
}

fn scalar_span(
    text: &str,
    start: usize,
    style: TScalarStyle,
    flow: bool,
    value: &str,
    legacy: bool,
) -> Option<Range<usize>> {
    let tail = &text[start..];
    let end = match style {
        TScalarStyle::SingleQuoted | TScalarStyle::DoubleQuoted => {
            let quote = if style == TScalarStyle::SingleQuoted {
                '\''
            } else {
                '"'
            };
            if !tail.starts_with(quote) {
                return None;
            }
            let mut chars = tail.char_indices().skip(1).peekable();
            let mut end = None;
            while let Some((i, c)) = chars.next() {
                if matches!(c, '\r' | '\n') {
                    return None;
                }
                if c == '\\' && quote == '"' {
                    chars.next();
                } else if c == quote {
                    if quote == '\'' && chars.peek().is_some_and(|(_, c)| *c == '\'') {
                        chars.next();
                    } else {
                        end = Some(i + c.len_utf8());
                        break;
                    }
                }
            }
            end?
        }
        TScalarStyle::Plain => {
            let mut previous = ' ';
            let mut end = tail.len();
            for (i, c) in tail.char_indices() {
                if matches!(c, '\r' | '\n')
                    || (flow && matches!(c, ',' | ']' | '}'))
                    || (c == '#' && previous.is_whitespace())
                {
                    end = i;
                    break;
                }
                previous = c;
            }
            let scalar = tail[..end].trim_end();
            // Plain scalars spanning multiple lines must never be partially replaced.
            let decoded = match resolve(scalar, legacy) {
                Yaml::Integer(n) => n.to_string(),
                Yaml::Boolean(b) => b.to_string(),
                _ => scalar.to_owned(),
            };
            if decoded != value || scalar.is_empty() {
                return None;
            }
            scalar.len()
        }
        _ => return None,
    };
    Some(start..start + end)
}

fn resolve(value: &str, legacy: bool) -> Yaml {
    if legacy {
        match value.to_ascii_lowercase().as_str() {
            "yes" | "on" => return Yaml::Boolean(true),
            "no" | "off" => return Yaml::Boolean(false),
            _ => {}
        }
    }
    Yaml::from_str(value)
}

fn description(text: &str, start: usize, end: Option<usize>) -> String {
    let line_start = text[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let mut comments = Vec::new();
    for line in text[..line_start].lines().rev() {
        let Some(comment) = line.trim().strip_prefix('#') else {
            break;
        };
        comments.push(comment.trim().to_owned());
    }
    comments.reverse();
    if let Some(end) = end {
        if let Some(comment) = text[end..]
            .split(['\n', '\r'])
            .next()
            .unwrap_or("")
            .trim()
            .strip_prefix('#')
        {
            comments.push(comment.trim().to_owned());
        }
    }
    comments.join(" ")
}

fn atomic_save(path: &Path, original: &str, text: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        bail!("config file was replaced; reopen it before saving");
    }
    if metadata.permissions().readonly() {
        bail!("config file is read-only");
    }
    if fs::read_to_string(path)? != original {
        bail!("config file changed on disk; reopen it before saving");
    }
    let parent = path.parent().context("config has no parent directory")?;
    let (temporary, mut file) = loop {
        let id = SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".taid-{}-{id}.tmp", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("creating temporary config file"),
        }
    };
    let result = (|| -> Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::set_permissions(&temporary, metadata.permissions())?;
        drop(file);
        if !fs::symlink_metadata(path)?.is_file() || fs::read_to_string(path)? != original {
            bail!("config file changed on disk; reopen it before saving");
        }
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::TempDir;
    use yaml_rust2::YamlLoader;

    fn form(tag: &str, yaml: &str) -> Result<(TempDir, Form)> {
        let temp = TempDir::new(tag)?;
        let path = temp.path().join("params.yaml");
        fs::write(&path, yaml)?;
        let form = Form::open(path)?;
        Ok((temp, form))
    }

    #[test]
    fn discovers_only_nested_yaml_configs() -> Result<()> {
        let temp = TempDir::new("configs")?;
        for path in [
            "config/root.yaml",
            "pkg/config/nested.yml",
            "thirdparty/config/vendor.yaml",
            "target/config/build.yaml",
            "config/view.rviz",
            "config/readme.txt",
        ] {
            let file = temp.path().join(path);
            fs::create_dir_all(file.parent().unwrap())?;
            fs::write(file, "key: 1\n")?;
        }
        assert_eq!(discover(temp.path())?.len(), 2);
        Ok(())
    }

    #[test]
    fn typed_form_preserves_comments_crlf_and_structure() -> Result<()> {
        let original = "node:\r\n  ros__parameters:\r\n    # tuning (meters)\r\n    gain: 1.0  # inline\r\n    enabled: false\r\n    name: 'λ driver'\r\n    limits: [1, 2, 3]\r\n";
        let (_temp, mut f) = form("typed", original)?;
        assert_eq!(f.parameters.len(), 6);
        assert_eq!(f.parameters[0].name, "node.ros__parameters.gain");
        assert_eq!(f.parameters[0].description, "tuning (meters) inline");
        f.activate();
        f.paste("2");
        f.commit_input()?;
        assert_eq!(f.parameters[0].value, "2.0");
        f.scroll.select(1);
        f.activate();
        f.scroll.select(4);
        f.activate();
        f.paste("9");
        f.commit_input()?;
        f.save()?;
        assert_eq!(
            fs::read_to_string(&f.path)?,
            original
                .replace("gain: 1.0", "gain: 2.0")
                .replace("enabled: false", "enabled: true")
                .replace("[1, 2, 3]", "[1, 9, 3]")
        );
        assert!(!f.dirty());
        f.scroll.select(0);
        f.activate();
        f.paste("3.0");
        f.save()?;
        assert!(fs::read_to_string(&f.path)?.contains("gain: 3.0"));
        Ok(())
    }

    #[test]
    fn rejects_invalid_numbers_and_quotes_text_without_yaml_injection() -> Result<()> {
        let (_temp, mut f) = form("validation", "count: 1\ngain: 1.0\nname: original\n")?;
        f.activate();
        f.paste("1.5");
        assert!(f.commit_input().is_err());
        assert!(f.save().is_err());
        assert_eq!(
            fs::read_to_string(&f.path)?,
            "count: 1\ngain: 1.0\nname: original\n"
        );
        f.key(KeyCode::Esc, KeyModifiers::NONE, 10);
        f.scroll.select(1);
        f.activate();
        f.paste("not a number");
        assert!(f.commit_input().is_err());
        f.key(KeyCode::Esc, KeyModifiers::NONE, 10);
        f.scroll.select(2);
        f.activate();
        f.paste("true # [something]: null");
        f.save()?;
        let yaml = YamlLoader::load_from_str(&fs::read_to_string(&f.path)?)?;
        assert_eq!(yaml[0]["name"].as_str(), Some("true # [something]: null"));
        assert_eq!(f.parameters[2].kind, Kind::Text);
        Ok(())
    }

    #[test]
    fn handles_nested_flow_sequences_and_unicode_source_positions() -> Result<()> {
        let original = "λ: '日本語'\nparams: {speed: 2.5, flags: [true, false], labels: ['a,b', \"a\\\"b\"]}\n";
        let (_temp, mut f) = form("flow", original)?;
        f.scroll.select(1);
        f.activate();
        f.paste("4.5");
        f.commit_input()?;
        f.scroll.select(4);
        f.activate();
        f.paste("a'b");
        f.commit_input()?;
        f.save()?;
        assert_eq!(
            fs::read_to_string(&f.path)?,
            original
                .replace("speed: 2.5", "speed: 4.5")
                .replace("'a,b'", "'a''b'")
        );
        Ok(())
    }

    #[test]
    fn protects_readonly_nodes_and_external_edits() -> Result<()> {
        let (_temp, mut f) = form(
            "readonly",
            "shared: &x 1\ncopy: *x\nempty: null\nmessage: |\n  hello\n  there\nplain: first\n  second\ncount: 1\n",
        )?;
        for i in 0..5 {
            assert!(f.parameters[i].readonly.is_some());
            f.scroll.select(i);
            f.activate();
            assert!(f.editing.is_none());
        }
        f.scroll.select(5);
        f.activate();
        f.paste("2");
        f.commit_input()?;
        fs::write(&f.path, "count: 100\n")?;
        assert!(f.save().is_err());
        assert_eq!(fs::read_to_string(&f.path)?, "count: 100\n");
        assert!(parameters("gain: 1\ngain: 2\n").is_err());
        assert!(parameters("gain: [1,\n").is_err());
        Ok(())
    }

    #[test]
    fn legacy_booleans_and_explicit_yaml_12_keep_their_schema() -> Result<()> {
        let (_temp, mut f) = form("legacy-bool", "checked_on_default: No\nquoted: 'No'\n")?;
        assert_eq!(f.parameters[0].kind, Kind::Boolean);
        assert_eq!(f.parameters[1].kind, Kind::Text);
        f.activate();
        f.save()?;
        assert_eq!(
            fs::read_to_string(&f.path)?,
            "checked_on_default: true\nquoted: 'No'\n"
        );
        let strict = parameters("%YAML 1.2\n---\nchecked: No\n")?;
        assert_eq!(strict[0].kind, Kind::Text);
        let multi =
            parameters("%YAML 1.1\n---\nchecked: Yes\n...\n%YAML 1.2\n---\nchecked: Yes\n")?;
        assert_eq!(multi[0].kind, Kind::Boolean);
        assert_eq!(multi[1].kind, Kind::Text);
        assert_ne!(multi[0].name, multi[1].name);
        Ok(())
    }

    #[test]
    fn form_search_reset_and_cancel_do_not_change_parameter_structure() -> Result<()> {
        let (_temp, mut f) = form(
            "search-reset",
            "speed: 1.0\n# steering limits\nangle: 0.5\n",
        )?;
        f.key(KeyCode::Char('/'), KeyModifiers::NONE, 10);
        for c in "steering".chars() {
            f.key(KeyCode::Char(c), KeyModifiers::NONE, 10);
        }
        f.key(KeyCode::Enter, KeyModifiers::NONE, 10);
        assert_eq!(f.visible, vec![1]);
        f.activate();
        f.paste("2.0");
        f.commit_input()?;
        assert!(f.dirty());
        f.key(KeyCode::Char('r'), KeyModifiers::NONE, 10);
        assert!(!f.dirty());
        f.activate();
        f.paste("9.0\nnew_key: 3");
        assert_eq!(f.editing.as_ref().unwrap().text, "0.5");
        f.paste("1.5");
        f.key(KeyCode::Esc, KeyModifiers::NONE, 10);
        assert_eq!(f.text()?, "speed: 1.0\n# steering limits\nangle: 0.5\n");
        Ok(())
    }
}
