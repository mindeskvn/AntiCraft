//! Per-conversation Markdown artifacts (ORCHESTRATION.md section B.2).
//!
//! Layout under `<data-dir>/conversations/<id>/artifacts/`:
//! - `<name>.md`: the current revision of each artifact.
//! - `meta.json`: [`ArtifactMeta`] for every artifact (version, summary, review state).
//! - `comments.json`: pending user [`ArtifactComment`]s not yet sent to the agent.
//! - `.history/<name>.v<N>.md`: earlier revisions, newest [`MAX_REVISIONS`] kept.
//!
//! The conversation's `<id>.json` sits next to the `<id>/` directory; the conversation
//! store's `list()` only scans `*.json`, so the directory is ignored there.
//! Names are restricted to `[a-z0-9_-]{1,64}` (minus Windows device names) so the model can
//! never escape the directory. All writes go through [`atomic_write`]. Every method is
//! blocking file I/O: call it from `spawn_blocking` or the backend task, never the UI thread.
//! One writer per conversation is assumed (the turn task while a turn runs).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::Content;
use crate::store::atomic_write;

/// Well-known artifact roles. Unknown names are [`ArtifactKind::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// `task.md`: checklist (`- [ ]`, `- [/]` in progress, `- [x]` done).
    TaskList,
    /// `implementation_plan.md`: goal, proposed changes per file, verification plan.
    ImplementationPlan,
    /// `walkthrough.md`: what changed and how it was verified, written at the end.
    Walkthrough,
    Other,
}

impl ArtifactKind {
    pub fn file_stem(self) -> Option<&'static str> {
        match self {
            ArtifactKind::TaskList => Some("task"),
            ArtifactKind::ImplementationPlan => Some("implementation_plan"),
            ArtifactKind::Walkthrough => Some("walkthrough"),
            ArtifactKind::Other => None,
        }
    }
    pub fn from_stem(stem: &str) -> Self {
        match stem {
            "task" => ArtifactKind::TaskList,
            "implementation_plan" => ArtifactKind::ImplementationPlan,
            "walkthrough" => ArtifactKind::Walkthrough,
            _ => ArtifactKind::Other,
        }
    }
    /// Plans need review under `ArtifactReviewPolicy::RequestReview`; others never do.
    pub fn is_reviewable(self) -> bool {
        matches!(self, ArtifactKind::ImplementationPlan)
    }
}

/// Review state shown on the artifact header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    #[default]
    None,
    Pending,
    Approved,
    ChangesRequested,
}

/// Metadata for one artifact (kept in `Conversation.artifacts` and in `meta.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub name: String,
    pub kind: ArtifactKind,
    /// Short one-line summary given by the agent.
    pub summary: String,
    pub version: u32,
    pub review: ReviewState,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// User comment on an artifact; sent back to the agent as part of a user turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactComment {
    pub artifact: String,
    /// Quoted line or selection the comment refers to (optional).
    pub anchor: Option<String>,
    pub text: String,
}

/// Longest anchor quote kept in feedback text (chars).
const ANCHOR_MAX_CHARS: usize = 200;

/// Format comments as the feedback text the agent receives.
pub fn comments_to_feedback(comments: &[ArtifactComment]) -> String {
    let mut s = String::from("Review comments on the artifacts:\n");
    for c in comments {
        let text = one_line(&c.text, usize::MAX);
        match c.anchor.as_deref().map(|a| one_line(a, ANCHOR_MAX_CHARS)).filter(|a| !a.is_empty()) {
            Some(a) => s.push_str(&format!("- {} (on \"{}\"): {}\n", c.artifact, a, text)),
            None => s.push_str(&format!("- {}: {}\n", c.artifact, text)),
        }
    }
    s
}

/// Collapse whitespace (including newlines) and cut at `max` chars with an ellipsis.
fn one_line(s: &str, max: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > max {
        let cut: String = joined.chars().take(max.saturating_sub(1)).collect();
        format!("{}\u{2026}", cut.trim_end())
    } else {
        joined
    }
}

/// Turn review feedback into the user turn sent to the agent (Review / Request changes
/// outside a blocking `request_review`). `note` is an optional free-form message typed with
/// the review. `None` when there is nothing to send.
pub fn feedback_turn(comments: &[ArtifactComment], note: Option<&str>) -> Option<Content> {
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    let comments: Vec<ArtifactComment> = comments.iter().filter(|c| !c.text.trim().is_empty()).cloned().collect();
    if comments.is_empty() && note.is_none() {
        return None;
    }
    let mut s = String::new();
    if let Some(n) = note {
        s.push_str(n);
        s.push_str("\n\n");
    }
    if !comments.is_empty() {
        s.push_str(&comments_to_feedback(&comments));
        s.push_str("\nRevise the affected artifacts to address these comments, then ask for review again.");
    }
    Some(Content::user_text(s.trim_end().to_owned()))
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("invalid artifact name {0:?} (use a-z, 0-9, '_' or '-', at most 64 chars)")]
    BadName(String),
    #[error("artifact too large ({0} bytes, limit {MAX_ARTIFACT_BYTES})")]
    TooLarge(usize),
    #[error("artifact not found: {0}")]
    NotFound(String),
    #[error("too many artifacts in this conversation (limit {MAX_ARTIFACTS})")]
    TooMany,
    #[error("comment too long ({0} bytes, limit {MAX_COMMENT_BYTES})")]
    CommentTooLong(usize),
    #[error("too many pending comments (limit {MAX_COMMENTS})")]
    TooManyComments,
    #[error("corrupt artifact metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub const MAX_ARTIFACT_BYTES: usize = 256 * 1024;
/// Distinct artifacts per conversation.
pub const MAX_ARTIFACTS: usize = 64;
/// Previous revisions kept per artifact (older ones are pruned).
pub const MAX_REVISIONS: u32 = 20;
/// Size limit of one comment (text + anchor).
pub const MAX_COMMENT_BYTES: usize = 8 * 1024;
/// Pending comments per conversation.
pub const MAX_COMMENTS: usize = 200;

const META_FILE: &str = "meta.json";
const COMMENTS_FILE: &str = "comments.json";
const HISTORY_DIR: &str = ".history";

/// Windows device names: `con.md` etc. are unsafe file names on Windows.
const RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1",
    "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Validate an artifact name (without `.md`).
pub fn validate_name(name: &str) -> Result<(), ArtifactError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && !RESERVED.contains(&name);
    if ok { Ok(()) } else { Err(ArtifactError::BadName(name.to_owned())) }
}

/// Best-effort conversion of a model-supplied name (`"Implementation Plan.md"`) into a
/// valid one (`implementation_plan`). `None` when nothing usable remains.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let raw = raw.strip_suffix(".md").or_else(|| raw.strip_suffix(".MD")).unwrap_or(raw);
    let mut out = String::new();
    for ch in raw.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
            out.push(c);
        } else if (c.is_whitespace() || c == '.') && !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let mut out = out.trim_matches('_').to_owned();
    out.truncate(64);
    let out = out.trim_end_matches('_').to_owned();
    validate_name(&out).is_ok().then_some(out)
}

// ---------------------------------------------------------------------------
// Task list checkboxes
// ---------------------------------------------------------------------------

/// Checkbox state of a task item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskMark {
    /// `[ ]`
    Todo,
    /// `[/]` (also accepts `[-]` and `[~]` when reading)
    InProgress,
    /// `[x]` / `[X]`
    Done,
}

impl TaskMark {
    fn from_char(c: char) -> Option<Self> {
        match c {
            ' ' => Some(TaskMark::Todo),
            '/' | '-' | '~' => Some(TaskMark::InProgress),
            'x' | 'X' => Some(TaskMark::Done),
            _ => None,
        }
    }
    pub fn as_char(self) -> char {
        match self {
            TaskMark::Todo => ' ',
            TaskMark::InProgress => '/',
            TaskMark::Done => 'x',
        }
    }
}

/// One checklist item of a task list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskItem {
    /// 0-based line number in the Markdown.
    pub line: usize,
    /// Leading whitespace width (spaces; a tab counts as 4). Nesting depth = indent / 2.
    pub indent: usize,
    pub mark: TaskMark,
    pub text: String,
}

/// If `line` is a checklist item (optional indent, a bullet `-`/`*`/`+` or `1.`/`1)`, a
/// space, `[c]`, then end of line or whitespace + text), return
/// `(byte offset of the mark char, indent, mark, text)`.
fn parse_task_line(line: &str) -> Option<(usize, usize, TaskMark, &str)> {
    let trimmed = line.trim_start_matches([' ', '\t']);
    let lead = &line[..line.len() - trimmed.len()];
    let indent = lead.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum();
    let after_bullet = if let Some(r) = trimmed.strip_prefix(['-', '*', '+']) {
        r
    } else {
        let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || digits > 9 {
            return None;
        }
        trimmed[digits..].strip_prefix(['.', ')'])?
    };
    let rest = after_bullet.strip_prefix([' ', '\t'])?.trim_start_matches([' ', '\t']);
    let mut chars = rest.chars();
    if chars.next()? != '[' {
        return None;
    }
    let mark = TaskMark::from_char(chars.next()?)?;
    if chars.next()? != ']' {
        return None;
    }
    let text = chars.as_str();
    if !(text.is_empty() || text.starts_with([' ', '\t'])) {
        return None;
    }
    let mark_offset = line.len() - rest.len() + 1;
    Some((mark_offset, indent, mark, text.trim()))
}

/// Parse every checklist item, skipping fenced code blocks.
pub fn parse_tasks(markdown: &str) -> Vec<TaskItem> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    for (i, line) in markdown.lines().enumerate() {
        let t = line.trim_start();
        if let Some(f) = fence {
            if t.starts_with(f) {
                fence = None;
            }
            continue;
        }
        if t.starts_with("```") {
            fence = Some("```");
            continue;
        }
        if t.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }
        if let Some((_, indent, mark, text)) = parse_task_line(line) {
            out.push(TaskItem { line: i, indent, mark, text: text.to_owned() });
        }
    }
    out
}

/// `(done, total)` over all checklist items.
pub fn task_progress(markdown: &str) -> (usize, usize) {
    let items = parse_tasks(markdown);
    (items.iter().filter(|t| t.mark == TaskMark::Done).count(), items.len())
}

/// Set the mark of the `index`-th checklist item (in [`parse_tasks`] order), keeping every
/// other byte (line endings included). `None` if there is no such item.
pub fn set_task_mark(markdown: &str, index: usize, mark: TaskMark) -> Option<String> {
    let item = parse_tasks(markdown).into_iter().nth(index)?;
    let mut offset = 0;
    for (i, seg) in markdown.split_inclusive('\n').enumerate() {
        if i == item.line {
            let (m, ..) = parse_task_line(seg.trim_end_matches(['\n', '\r']))?;
            let pos = offset + m;
            let mut s = String::with_capacity(markdown.len());
            s.push_str(&markdown[..pos]);
            s.push(mark.as_char());
            s.push_str(&markdown[pos + 1..]);
            return Some(s);
        }
        offset += seg.len();
    }
    None
}

/// Set the mark of the first item whose text equals `text` (trimmed, case-insensitive).
pub fn set_task_mark_by_text(markdown: &str, text: &str, mark: TaskMark) -> Option<String> {
    let want = text.trim().to_lowercase();
    let idx = parse_tasks(markdown).iter().position(|t| t.text.to_lowercase() == want)?;
    set_task_mark(markdown, idx, mark)
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// Artifact directory of one conversation.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    dir: PathBuf,
}

impl ArtifactStore {
    /// `conversations_dir` is `<data-dir>/conversations`.
    pub fn new(conversations_dir: &Path, conversation: uuid::Uuid) -> Self {
        Self { dir: conversations_dir.join(conversation.to_string()).join("artifacts") }
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn path(&self, name: &str) -> Result<PathBuf, ArtifactError> {
        validate_name(name)?;
        Ok(self.dir.join(format!("{name}.md")))
    }
    fn revision_path(&self, name: &str, version: u32) -> PathBuf {
        self.dir.join(HISTORY_DIR).join(format!("{name}.v{version}.md"))
    }

    /// Write (create or replace) an artifact, keeping the summary of an existing one.
    /// Blocking; call from `spawn_blocking`. See [`Self::write_versioned`].
    pub fn write(&self, name: &str, markdown: &str) -> Result<PathBuf, ArtifactError> {
        self.write_versioned(name, markdown, None)?;
        self.path(name)
    }

    /// Create or update an artifact and return its new metadata. The previous content is
    /// kept as revision `version - 1` (newest [`MAX_REVISIONS`] kept). Writing identical
    /// content does not bump the version. A changed artifact goes back to
    /// [`ReviewState::None`]. `summary: None` keeps the previous summary.
    pub fn write_versioned(&self, name: &str, markdown: &str, summary: Option<&str>) -> Result<ArtifactMeta, ArtifactError> {
        if markdown.len() > MAX_ARTIFACT_BYTES {
            return Err(ArtifactError::TooLarge(markdown.len()));
        }
        let p = self.path(name)?;
        let mut metas = self.load_meta()?;
        let previous = match std::fs::read_to_string(&p) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let pos = metas.iter().position(|m| m.name == name);
        if pos.is_none() && previous.is_none() && self.count_existing(&metas) >= MAX_ARTIFACTS {
            return Err(ArtifactError::TooMany);
        }
        let summary = summary.map(|s| one_line(s, 200));
        let now = chrono::Utc::now();
        let meta = match pos {
            Some(i) => {
                let m = &mut metas[i];
                if previous.as_deref() == Some(markdown) {
                    if let Some(s) = summary {
                        m.summary = s;
                    }
                    let m = m.clone();
                    self.save_meta(&metas)?;
                    return Ok(m);
                }
                if let Some(prev) = &previous {
                    atomic_write(&self.revision_path(name, m.version), prev.as_bytes())?;
                }
                m.version = m.version.saturating_add(1).max(1);
                if let Some(s) = summary {
                    m.summary = s;
                }
                m.review = ReviewState::None;
                m.updated_at = now;
                m.clone()
            }
            None => {
                // File without metadata (older build or lost meta.json): it counts as v1.
                let version = match &previous {
                    Some(prev) if prev == markdown => 1,
                    Some(prev) => {
                        atomic_write(&self.revision_path(name, 1), prev.as_bytes())?;
                        2
                    }
                    None => 1,
                };
                let m = ArtifactMeta {
                    name: name.to_owned(),
                    kind: ArtifactKind::from_stem(name),
                    summary: summary.unwrap_or_default(),
                    version,
                    review: ReviewState::None,
                    updated_at: now,
                };
                metas.push(m.clone());
                m
            }
        };
        atomic_write(&p, markdown.as_bytes())?;
        self.save_meta(&metas)?;
        self.prune_revisions(name, meta.version);
        Ok(meta)
    }

    fn count_existing(&self, metas: &[ArtifactMeta]) -> usize {
        let mut names = self.list();
        names.extend(metas.iter().map(|m| m.name.clone()));
        names.sort();
        names.dedup();
        names.len()
    }

    /// Delete revisions older than `current - MAX_REVISIONS` (best effort).
    fn prune_revisions(&self, name: &str, current: u32) {
        let keep_from = current.saturating_sub(MAX_REVISIONS);
        for v in self.revisions(name).unwrap_or_default() {
            if v < keep_from {
                let _ = std::fs::remove_file(self.revision_path(name, v));
            }
        }
    }

    pub fn read(&self, name: &str) -> Result<String, ArtifactError> {
        let p = self.path(name)?;
        read_or_not_found(&p, name)
    }

    /// Content of an earlier revision (or the current one when `version` matches).
    pub fn read_revision(&self, name: &str, version: u32) -> Result<String, ArtifactError> {
        validate_name(name)?;
        if self.meta(name)?.is_some_and(|m| m.version == version) {
            return self.read(name);
        }
        read_or_not_found(&self.revision_path(name, version), &format!("{name} v{version}"))
    }

    /// Versions of the stored previous revisions, ascending (current version excluded).
    pub fn revisions(&self, name: &str) -> Result<Vec<u32>, ArtifactError> {
        validate_name(name)?;
        let prefix = format!("{name}.v");
        let mut v: Vec<u32> = std::fs::read_dir(self.dir.join(HISTORY_DIR))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let f = e.file_name().to_str()?.to_owned();
                        f.strip_prefix(&prefix)?.strip_suffix(".md")?.parse().ok()
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort_unstable();
        Ok(v)
    }

    /// Names (without `.md`) of existing artifacts, sorted.
    pub fn list(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .filter_map(|e| {
                        let p = e.path();
                        (p.extension().and_then(|x| x.to_str()) == Some("md"))
                            .then(|| p.file_stem()?.to_str().map(str::to_owned))
                            .flatten()
                    })
                    .filter(|n| validate_name(n).is_ok())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Metadata of every existing artifact, sorted by name. Files without a `meta.json`
    /// entry get synthesized v1 metadata from the file's modification time.
    pub fn list_meta(&self) -> Result<Vec<ArtifactMeta>, ArtifactError> {
        let metas = self.load_meta()?;
        let mut out = Vec::new();
        for name in self.list() {
            match metas.iter().find(|m| m.name == name) {
                Some(m) => out.push(m.clone()),
                None => {
                    let updated_at = std::fs::metadata(self.dir.join(format!("{name}.md")))
                        .and_then(|m| m.modified())
                        .map(chrono::DateTime::<chrono::Utc>::from)
                        .unwrap_or_else(|_| chrono::Utc::now());
                    out.push(ArtifactMeta {
                        kind: ArtifactKind::from_stem(&name),
                        name,
                        summary: String::new(),
                        version: 1,
                        review: ReviewState::None,
                        updated_at,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Metadata of one artifact, `None` if it does not exist.
    pub fn meta(&self, name: &str) -> Result<Option<ArtifactMeta>, ArtifactError> {
        validate_name(name)?;
        Ok(self.list_meta()?.into_iter().find(|m| m.name == name))
    }

    /// Change the review state of an existing artifact.
    pub fn set_review(&self, name: &str, review: ReviewState) -> Result<ArtifactMeta, ArtifactError> {
        let current = self.meta(name)?.ok_or_else(|| ArtifactError::NotFound(name.to_owned()))?;
        let mut metas = self.load_meta()?;
        let idx = match metas.iter().position(|m| m.name == name) {
            Some(i) => i,
            None => {
                metas.push(current);
                metas.len() - 1
            }
        };
        metas[idx].review = review;
        let out = metas[idx].clone();
        self.save_meta(&metas)?;
        Ok(out)
    }

    /// Set the mark of one task item in a task-list artifact and store it as a new version.
    pub fn update_task(&self, name: &str, index: usize, mark: TaskMark) -> Result<ArtifactMeta, ArtifactError> {
        let md = self.read(name)?;
        let new = set_task_mark(&md, index, mark).ok_or_else(|| ArtifactError::NotFound(format!("{name} item {index}")))?;
        self.write_versioned(name, &new, None)
    }

    fn load_meta(&self) -> Result<Vec<ArtifactMeta>, ArtifactError> {
        match std::fs::read(self.dir.join(META_FILE)) {
            Ok(b) => {
                let mut v: Vec<ArtifactMeta> = serde_json::from_slice(&b)?;
                v.retain(|m| validate_name(&m.name).is_ok());
                Ok(v)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save_meta(&self, metas: &[ArtifactMeta]) -> Result<(), ArtifactError> {
        let mut v = metas.to_vec();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        atomic_write(&self.dir.join(META_FILE), &serde_json::to_vec_pretty(&v)?)?;
        Ok(())
    }

    // --- comments ---------------------------------------------------------------

    /// Pending (not yet sent) comments, in insertion order.
    pub fn comments(&self) -> Result<Vec<ArtifactComment>, ArtifactError> {
        match std::fs::read(self.dir.join(COMMENTS_FILE)) {
            Ok(b) => Ok(serde_json::from_slice(&b)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Store a comment. The artifact must exist; blank comments are ignored. Returns the
    /// number of pending comments.
    pub fn add_comment(&self, comment: ArtifactComment) -> Result<usize, ArtifactError> {
        validate_name(&comment.artifact)?;
        let size = comment.text.len() + comment.anchor.as_ref().map_or(0, String::len);
        if size > MAX_COMMENT_BYTES {
            return Err(ArtifactError::CommentTooLong(size));
        }
        let mut all = self.comments()?;
        if comment.text.trim().is_empty() {
            return Ok(all.len());
        }
        if !self.path(&comment.artifact)?.is_file() {
            return Err(ArtifactError::NotFound(comment.artifact));
        }
        if all.len() >= MAX_COMMENTS {
            return Err(ArtifactError::TooManyComments);
        }
        all.push(comment);
        self.save_comments(&all)?;
        Ok(all.len())
    }

    /// Remove the comment at `index` (in [`Self::comments`] order).
    pub fn remove_comment(&self, index: usize) -> Result<(), ArtifactError> {
        let mut all = self.comments()?;
        if index >= all.len() {
            return Err(ArtifactError::NotFound(format!("comment {index}")));
        }
        all.remove(index);
        self.save_comments(&all)
    }

    /// Take the pending comments (all, or only those on `artifact`), removing them.
    pub fn take_comments(&self, artifact: Option<&str>) -> Result<Vec<ArtifactComment>, ArtifactError> {
        let all = self.comments()?;
        let (taken, kept): (Vec<_>, Vec<_>) = all.into_iter().partition(|c| artifact.is_none_or(|a| c.artifact == a));
        if !taken.is_empty() {
            self.save_comments(&kept)?;
        }
        Ok(taken)
    }

    /// Build the feedback user turn ([`feedback_turn`]) from the pending comments. They are
    /// removed only when a turn is produced.
    pub fn take_feedback_turn(&self, note: Option<&str>) -> Result<Option<Content>, ArtifactError> {
        let pending = self.comments()?;
        let turn = feedback_turn(&pending, note);
        if turn.is_some() && !pending.is_empty() {
            self.save_comments(&[])?;
        }
        Ok(turn)
    }

    fn save_comments(&self, all: &[ArtifactComment]) -> Result<(), ArtifactError> {
        let p = self.dir.join(COMMENTS_FILE);
        if all.is_empty() {
            return match std::fs::remove_file(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            };
        }
        atomic_write(&p, &serde_json::to_vec_pretty(all)?)?;
        Ok(())
    }

    /// Delete one artifact: its file, its revisions and its metadata entry. Used by the
    /// edit/retry rollback for an artifact written by the turn that was replaced. Blocking.
    pub fn remove(&self, name: &str) -> Result<bool, ArtifactError> {
        validate_name(name)?;
        let mut removed = false;
        match std::fs::remove_file(self.path(name)?) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        for v in self.revisions(name)? {
            let _ = std::fs::remove_file(self.revision_path(name, v));
            removed = true;
        }
        let mut metas = self.load_meta()?;
        let before = metas.len();
        metas.retain(|m| m.name != name);
        if metas.len() != before {
            self.save_meta(&metas)?;
            removed = true;
        }
        Ok(removed)
    }

    /// Remove the whole conversation directory (used by `ConversationStore::delete`).
    pub fn remove_all(&self) -> std::io::Result<()> {
        match self.dir.parent().map(std::fs::remove_dir_all) {
            Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn read_or_not_found(p: &Path, what: &str) -> Result<String, ArtifactError> {
    std::fs::read_to_string(p).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ArtifactError::NotFound(what.to_owned()),
        _ => e.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ArtifactStore) {
        let t = tempfile::tempdir().unwrap();
        let s = ArtifactStore::new(t.path(), uuid::Uuid::new_v4());
        (t, s)
    }

    #[test]
    fn names_are_confined() {
        assert!(validate_name("implementation_plan").is_ok());
        for bad in ["", "../x", "a/b", "A", "x.md", &"a".repeat(65), "con", "nul", "com1", "meta.json"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_name("Implementation Plan.md").as_deref(), Some("implementation_plan"));
        assert_eq!(sanitize_name("../../etc/Task.md").as_deref(), Some("task"));
        assert_eq!(sanitize_name("C:\\x\\notes v2").as_deref(), Some("notes_v2"));
        assert_eq!(sanitize_name("..."), None);
        assert_eq!(sanitize_name("CON"), None);
        assert!(sanitize_name(&"ab ".repeat(40)).unwrap().len() <= 64);
    }

    #[test]
    fn write_read_list() {
        let (_t, s) = store();
        s.write("task", "- [ ] one").unwrap();
        assert_eq!(s.read("task").unwrap(), "- [ ] one");
        assert_eq!(s.list(), vec!["task".to_string()]);
        assert!(matches!(s.read("nope"), Err(ArtifactError::NotFound(_))));
        s.remove_all().unwrap();
        assert!(s.list().is_empty());
        s.remove_all().unwrap();
    }

    #[test]
    fn versioning_keeps_previous_revision() {
        let (_t, s) = store();
        let m1 = s.write_versioned("implementation_plan", "v1", Some("first")).unwrap();
        assert_eq!((m1.version, m1.kind, m1.summary.as_str()), (1, ArtifactKind::ImplementationPlan, "first"));
        s.set_review("implementation_plan", ReviewState::Approved).unwrap();
        let same = s.write_versioned("implementation_plan", "v1", None).unwrap();
        assert_eq!((same.version, same.review), (1, ReviewState::Approved), "identical content is not a new version");
        let m2 = s.write_versioned("implementation_plan", "v2", None).unwrap();
        assert_eq!((m2.version, m2.review, m2.summary.as_str()), (2, ReviewState::None, "first"));
        assert_eq!(s.read_revision("implementation_plan", 1).unwrap(), "v1");
        assert_eq!(s.read_revision("implementation_plan", 2).unwrap(), "v2");
        assert_eq!(s.revisions("implementation_plan").unwrap(), vec![1]);
        assert!(matches!(s.read_revision("implementation_plan", 7), Err(ArtifactError::NotFound(_))));
        // meta.json and the history dir are not listed as artifacts.
        assert_eq!(s.list(), vec!["implementation_plan".to_string()]);
        assert_eq!(s.list_meta().unwrap(), vec![m2]);
    }

    #[test]
    fn revisions_are_pruned() {
        let (_t, s) = store();
        for i in 0..(MAX_REVISIONS + 5) {
            s.write_versioned("notes", &format!("rev {i}"), None).unwrap();
        }
        let current = s.meta("notes").unwrap().unwrap().version;
        assert_eq!(current, MAX_REVISIONS + 5);
        let revs = s.revisions("notes").unwrap();
        assert_eq!(revs.len() as u32, MAX_REVISIONS);
        assert_eq!(*revs.last().unwrap(), current - 1);
        assert_eq!(s.read_revision("notes", current - 1).unwrap(), format!("rev {}", MAX_REVISIONS + 3));
    }

    #[test]
    fn size_and_count_caps() {
        let (_t, s) = store();
        let big = "x".repeat(MAX_ARTIFACT_BYTES + 1);
        assert!(matches!(s.write("big", &big), Err(ArtifactError::TooLarge(_))));
        assert!(s.write("ok", &"x".repeat(MAX_ARTIFACT_BYTES)).is_ok());
        for i in 1..MAX_ARTIFACTS {
            s.write(&format!("a{i}"), "x").unwrap();
        }
        assert!(matches!(s.write("one-more", "x"), Err(ArtifactError::TooMany)));
        assert!(s.write("a1", "updated").is_ok(), "updates are allowed at the cap");
        assert!(matches!(s.write("../evil", "x"), Err(ArtifactError::BadName(_))));
    }

    #[test]
    fn legacy_file_without_meta() {
        let (_t, s) = store();
        std::fs::create_dir_all(s.dir()).unwrap();
        std::fs::write(s.dir().join("walkthrough.md"), "old").unwrap();
        let m = s.meta("walkthrough").unwrap().unwrap();
        assert_eq!((m.version, m.kind), (1, ArtifactKind::Walkthrough));
        let m = s.write_versioned("walkthrough", "new", Some("done")).unwrap();
        assert_eq!(m.version, 2);
        assert_eq!(s.read_revision("walkthrough", 1).unwrap(), "old");
        assert_eq!(s.set_review("walkthrough", ReviewState::Pending).unwrap().review, ReviewState::Pending);
        assert!(matches!(s.set_review("ghost", ReviewState::Pending), Err(ArtifactError::NotFound(_))));
    }

    #[test]
    fn no_temp_files_left() {
        let (_t, s) = store();
        s.write("task", "a").unwrap();
        s.write("task", "b").unwrap();
        let stray = std::fs::read_dir(s.dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(stray, 0);
    }

    const TASKS: &str = "# Task\r\n\r\n- [ ] Read code\n  - [/] Write tests\n* [x] Done thing\n1. [X] numbered\n- [] not a task\n- [ ]no space\n```\n- [ ] in code\n```\n\t- [-] tabbed\n";

    #[test]
    fn task_parsing() {
        let items = parse_tasks(TASKS);
        let got: Vec<_> = items.iter().map(|t| (t.line, t.indent, t.mark, t.text.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (2, 0, TaskMark::Todo, "Read code"),
                (3, 2, TaskMark::InProgress, "Write tests"),
                (4, 0, TaskMark::Done, "Done thing"),
                (5, 0, TaskMark::Done, "numbered"),
                (11, 4, TaskMark::InProgress, "tabbed"),
            ]
        );
        assert_eq!(task_progress(TASKS), (2, 5));
        assert_eq!(task_progress("no tasks"), (0, 0));
    }

    #[test]
    fn task_updating_preserves_bytes() {
        let out = set_task_mark(TASKS, 0, TaskMark::Done).unwrap();
        assert_eq!(out, TASKS.replacen("- [ ] Read code", "- [x] Read code", 1));
        assert!(out.contains("\r\n"), "CRLF kept");
        let out = set_task_mark(&out, 4, TaskMark::Done).unwrap();
        assert!(out.contains("\t- [x] tabbed"));
        assert!(out.contains("- [ ] in code"), "fenced code untouched");
        assert_eq!(set_task_mark(TASKS, 5, TaskMark::Done), None);
        let out = set_task_mark_by_text(TASKS, "write TESTS", TaskMark::Done).unwrap();
        assert!(out.contains("  - [x] Write tests"));
        assert_eq!(set_task_mark_by_text(TASKS, "nope", TaskMark::Done), None);
        assert_eq!(set_task_mark("- [ ] é ü\n", 0, TaskMark::InProgress).unwrap(), "- [/] é ü\n");
    }

    #[test]
    fn store_update_task_bumps_version() {
        let (_t, s) = store();
        s.write_versioned("task", "- [ ] a\n- [ ] b\n", None).unwrap();
        let m = s.update_task("task", 1, TaskMark::Done).unwrap();
        assert_eq!(m.version, 2);
        assert_eq!(s.read("task").unwrap(), "- [ ] a\n- [x] b\n");
        assert!(matches!(s.update_task("task", 9, TaskMark::Done), Err(ArtifactError::NotFound(_))));
    }

    fn c(artifact: &str, anchor: Option<&str>, text: &str) -> ArtifactComment {
        ArtifactComment { artifact: artifact.into(), anchor: anchor.map(Into::into), text: text.into() }
    }

    #[test]
    fn comments_storage() {
        let (_t, s) = store();
        s.write("implementation_plan", "plan").unwrap();
        s.write("task", "- [ ] x").unwrap();
        assert!(s.comments().unwrap().is_empty());
        assert_eq!(s.add_comment(c("implementation_plan", Some("step 2"), "use a channel")).unwrap(), 1);
        assert_eq!(s.add_comment(c("task", None, "split this")).unwrap(), 2);
        assert_eq!(s.add_comment(c("task", None, "   ")).unwrap(), 2, "blank ignored");
        assert!(matches!(s.add_comment(c("ghost", None, "x")), Err(ArtifactError::NotFound(_))));
        assert!(matches!(s.add_comment(c("../x", None, "x")), Err(ArtifactError::BadName(_))));
        let long = "x".repeat(MAX_COMMENT_BYTES + 1);
        assert!(matches!(s.add_comment(c("task", None, &long)), Err(ArtifactError::CommentTooLong(_))));
        let taken = s.take_comments(Some("task")).unwrap();
        assert_eq!(taken, vec![c("task", None, "split this")]);
        assert_eq!(s.comments().unwrap().len(), 1);
        s.add_comment(c("task", None, "again")).unwrap();
        s.remove_comment(1).unwrap();
        assert!(s.remove_comment(5).is_err());
        assert_eq!(s.comments().unwrap(), vec![c("implementation_plan", Some("step 2"), "use a channel")]);
    }

    #[test]
    fn comment_cap() {
        let (_t, s) = store();
        s.write("task", "x").unwrap();
        for _ in 0..MAX_COMMENTS {
            s.add_comment(c("task", None, "n")).unwrap();
        }
        assert!(matches!(s.add_comment(c("task", None, "n")), Err(ArtifactError::TooManyComments)));
    }

    #[test]
    fn feedback_text_and_turn() {
        let fb = comments_to_feedback(&[c("plan", Some("  line\n two "), " fix\nit "), c("task", Some("  "), "ok")]);
        assert_eq!(fb, "Review comments on the artifacts:\n- plan (on \"line two\"): fix it\n- task: ok\n");
        let long = comments_to_feedback(&[c("p", Some(&"a".repeat(500)), "t")]);
        assert!(long.chars().count() < 300 && long.contains('\u{2026}'));

        assert!(feedback_turn(&[], None).is_none());
        assert!(feedback_turn(&[c("p", None, "  ")], Some("  ")).is_none());
        let turn = feedback_turn(&[c("plan", None, "smaller steps")], Some("Looks close")).unwrap();
        assert_eq!(turn.role, Some(crate::api::Role::User));
        let text = turn.text();
        assert!(text.starts_with("Looks close\n\nReview comments"));
        assert!(text.contains("- plan: smaller steps"));
        assert_eq!(feedback_turn(&[], Some("just a note")).unwrap().text(), "just a note");
    }

    #[test]
    fn take_feedback_turn_clears_pending() {
        let (_t, s) = store();
        s.write("implementation_plan", "p").unwrap();
        assert!(s.take_feedback_turn(None).unwrap().is_none());
        s.add_comment(c("implementation_plan", None, "more tests")).unwrap();
        let t = s.take_feedback_turn(None).unwrap().unwrap();
        assert!(t.text().contains("more tests"));
        assert!(s.comments().unwrap().is_empty());
        assert!(!s.dir().join(COMMENTS_FILE).exists());
    }
}
