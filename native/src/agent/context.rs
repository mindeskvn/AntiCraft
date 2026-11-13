//! Prompt context sources (ORCHESTRATION.md section D): rules files, workflow files,
//! `@` mentions and context compaction.
//!
//! All file reads here are blocking and bounded; call them from `spawn_blocking` (never on
//! the UI thread). [`compact_history`] is async and only talks to the [`ModelBackend`].

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent::history::{estimate_tokens, user_turn_starts, History};
use crate::agent::plugins::{self, Plugin};
use crate::agent::tools::Workspace;
use crate::api::{CaError, Content, GenerateContentRequest, GenerationConfig, ModelBackend, Part, Role};

/// Max bytes read from one rules/workflow file.
pub const MAX_RULE_BYTES: usize = 24 * 1024;
/// Max total bytes of rules injected into the system prompt.
pub const MAX_RULES_TOTAL: usize = 64 * 1024;
/// Compact when the last prompt token count reaches this fraction of the model limit.
pub const COMPACT_THRESHOLD: f32 = 0.7;
/// Recent user turns kept verbatim when compacting.
pub const COMPACT_KEEP_TURNS: usize = 6;
/// Max bytes attached for one `@` file mention.
pub const MAX_MENTION_FILE_BYTES: usize = 64 * 1024;
/// Max bytes attached for all `@` mentions of one prompt.
pub const MAX_MENTION_TOTAL: usize = 256 * 1024;
/// Max entries listed for one `@` folder mention.
pub const MAX_LISTING_ENTRIES: usize = 200;
/// Input token limit assumed for Gemini models.
pub const DEFAULT_TOKEN_LIMIT: u64 = 1_048_576;

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

/// Max nested `AGENTS.md`-style files loaded from workspace subdirectories.
pub const MAX_NESTED_RULES: usize = 32;
/// Deepest directory level searched for nested rules files (root = 0).
pub const NESTED_RULES_DEPTH: usize = 4;
/// Rules file names recognized at the workspace root and in subdirectories.
const RULE_FILE_NAMES: [&str; 3] = ["AGENTS.md", "GEMINI.md", "ANTICRAFT.md"];
/// Directories never searched for nested rules (build output, dependencies).
const SKIPPED_DIRS: [&str; 6] = ["node_modules", "target", "dist", "build", "vendor", "out"];

/// Where a rules file comes from. Later scopes are more specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleScope {
    /// `<data-dir>/AGENTS.md` and `<data-dir>/rules/*.md`: applies to every workspace.
    Global,
    /// Workspace root files and `.agents/rules/*.md`.
    Workspace,
    /// `AGENTS.md`-style files in a workspace subdirectory; they apply to that subtree.
    Subdirectory,
}

/// A rules file found for the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleFile {
    pub path: PathBuf,
    pub text: String,
    pub scope: RuleScope,
    /// Glob patterns from the frontmatter `globs:` key (non-empty when `trigger: glob`).
    /// The rule is injected only when the active file path matches at least one pattern.
    pub globs: Vec<String>,
}

/// Collect `AGENTS.md`, `GEMINI.md`, `ANTICRAFT.md` at the workspace root and
/// `.agents/rules/*.md` (top level only, sorted). Oversized files are truncated; files past
/// the total budget are skipped. Only regular files that stay inside the workspace are
/// loaded: a symlink or junction at one of those names (or a rules-directory entry that is
/// one) is ignored, so a checkout cannot redirect the agent to read a file outside the
/// workspace merely by naming it `AGENTS.md`.
///
/// Frontmatter is stripped. A rule whose frontmatter says `trigger: manual` is not loaded here
/// (it only enters a turn when the user `@`-mentions it). `@[label](path)` inside a rule is
/// replaced by the target file's text (path relative to the rule file; the target must stay
/// inside the workspace, see [`expand_includes`]; one level, frontmatter stripped). A bare
/// `@name` is left as written.
pub fn load_rules(workspace: &Path) -> Vec<RuleFile> {
    load_all_rules(workspace, None)
}

/// [`load_rules`] plus nested and global rules. Sources, in budget priority order:
/// workspace root files and `.agents/rules/*.md`; `AGENTS.md`-style files in subdirectories
/// (up to [`NESTED_RULES_DEPTH`] levels, at most [`MAX_NESTED_RULES`], hidden and build
/// directories skipped, symlinks not followed); then `<data-dir>/AGENTS.md` and
/// `<data-dir>/rules/*.md` when `data_dir` is given. A file that does not fit the remaining
/// [`MAX_RULES_TOTAL`] budget is skipped. The result is ordered general to specific
/// (global, workspace, subdirectory) so later sections can refine earlier ones.
pub fn load_all_rules(workspace: &Path, data_dir: Option<&Path>) -> Vec<RuleFile> {
    let mut total = 0;
    let mut out = Vec::new();
    if let Ok(root) = dunce::canonicalize(workspace) {
        let mut paths: Vec<PathBuf> = RULE_FILE_NAMES.iter().map(|f| root.join(f)).collect();
        paths.extend(md_files(&root.join(".agents").join("rules")));
        load_rule_paths(&paths, &root, RuleScope::Workspace, &mut total, &mut out);
        load_rule_paths(&nested_rule_files(&root), &root, RuleScope::Subdirectory, &mut total, &mut out);
    }
    if let Some(groot) = data_dir.and_then(|d| dunce::canonicalize(d).ok()) {
        let mut paths = vec![groot.join("AGENTS.md")];
        paths.extend(md_files(&groot.join("rules")));
        load_rule_paths(&paths, &groot, RuleScope::Global, &mut total, &mut out);
    }
    out.sort_by_key(|r| r.scope);
    out
}

fn load_rule_paths(paths: &[PathBuf], root: &Path, scope: RuleScope, total: &mut usize, out: &mut Vec<RuleFile>) {
    for p in paths {
        let Some(p) = contained_regular_file(p, root) else { continue };
        if out.iter().any(|r| r.path == p) {
            continue;
        }
        let Some(raw) = read_capped(&p, MAX_RULE_BYTES) else { continue };
        let fm = parse_frontmatter(&raw);
        if matches!(fm.trigger.as_deref(), Some("manual") | Some("model_decision")) {
            continue;
        }
        let mut text = expand_includes(&fm.body, p.parent().unwrap_or(root), root);
        truncate_utf8(&mut text, MAX_RULE_BYTES);
        if text.trim().is_empty() || *total + text.len() > MAX_RULES_TOTAL {
            continue;
        }
        *total += text.len();
        out.push(RuleFile { path: p, text, scope, globs: fm.globs });
    }
}

/// `AGENTS.md`-style files below the workspace root (depth 1..=[`NESTED_RULES_DEPTH`]),
/// sorted by path, at most [`MAX_NESTED_RULES`].
fn nested_rule_files(root: &Path) -> Vec<PathBuf> {
    let walker = walkdir::WalkDir::new(root)
        .min_depth(1)
        .max_depth(NESTED_RULES_DEPTH + 1)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !name.starts_with('.') && !SKIPPED_DIRS.contains(&name.as_ref())
        });
    let mut out = Vec::new();
    for e in walker.flatten() {
        if e.depth() >= 2 && e.file_type().is_file() && RULE_FILE_NAMES.iter().any(|n| e.file_name() == *n) {
            out.push(e.into_path());
            if out.len() >= MAX_NESTED_RULES {
                break;
            }
        }
    }
    out
}

/// The `RULES` system-prompt section for `rules` (empty when there are none). Paths are
/// shown relative to `workspace` with `/` separators.
pub fn rules_section(rules: &[RuleFile], workspace: &Path) -> String {
    if rules.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "# Project rules\n\nThe user keeps the instructions below in the workspace. Treat them as standing \
         guidance for this project. When one conflicts with an explicit request in the conversation, the \
         request wins.\n",
    );
    for r in rules {
        s.push_str(&format!("\n## {}\n\n{}\n", rel_display(&r.path, workspace), r.text.trim_end()));
    }
    s
}

/// Tool the model calls to load a `model_decision` rule's body into the conversation.
pub const READ_RULE: &str = "read_rule";
/// Max on-demand rules listed in the system prompt.
pub const MAX_DECISION_RULES: usize = 32;

/// A rule with `trigger: model_decision`: only its name and description enter the system
/// prompt; the body is loaded on demand by [`READ_RULE`]. Behaves like a skill
/// (progressive disclosure) but lives with the rules files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRule {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: RuleScope,
    /// Canonical root (workspace or data dir) used as the `expand_includes` boundary.
    pub root: PathBuf,
}

/// Collect rules whose frontmatter says `trigger: model_decision`. Same discovery as
/// [`load_all_rules`] but keeps only those, and never counts against the always-on budget.
pub fn load_decision_rules(workspace: &Path, data_dir: Option<&Path>) -> Vec<DecisionRule> {
    let mut out = Vec::new();
    if let Ok(root) = dunce::canonicalize(workspace) {
        let mut paths: Vec<PathBuf> = RULE_FILE_NAMES.iter().map(|f| root.join(f)).collect();
        paths.extend(md_files(&root.join(".agents").join("rules")));
        load_decision_rule_paths(&paths, &root, RuleScope::Workspace, &mut out);
        load_decision_rule_paths(&nested_rule_files(&root), &root, RuleScope::Subdirectory, &mut out);
    }
    if let Some(groot) = data_dir.and_then(|d| dunce::canonicalize(d).ok()) {
        let mut paths = vec![groot.join("AGENTS.md")];
        paths.extend(md_files(&groot.join("rules")));
        load_decision_rule_paths(&paths, &groot, RuleScope::Global, &mut out);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.truncate(MAX_DECISION_RULES);
    out
}

/// The `model_decision` rules of one data directory, without the workspace sources. Used by
/// [`load_decision_rules_with_plugins`] when there is no workspace, so the global rules still
/// load exactly as [`load_decision_rules`] would load them.
fn load_global_decision_rules(data_dir: &Path) -> Vec<DecisionRule> {
    let mut out = Vec::new();
    if let Ok(groot) = dunce::canonicalize(data_dir) {
        let mut paths = vec![groot.join("AGENTS.md")];
        paths.extend(md_files(&groot.join("rules")));
        load_decision_rule_paths(&paths, &groot, RuleScope::Global, &mut out);
    }
    out
}

/// [`load_decision_rules`] plus the `trigger: model_decision` rules every enabled plugin ships.
///
/// A plugin rule keeps its own path (so `read_rule_text` reads the plugin's file) but is
/// addressed under a namespaced name, for the same reason plugin skills are: a plugin may name
/// a rule `perf` when the workspace already has one. `root` is the plugin directory, so the
/// `@[label](path)` include expansion is bounded by the plugin rather than by the workspace and
/// a plugin cannot pull a neighbouring file into the prompt through a relative target.
///
/// Rules that are not `model_decision` are not loaded here: plugin rules meant to apply always
/// arrive through the plugin's `rules/` files read as ordinary rules.
pub fn load_decision_rules_with_plugins(
    workspace: Option<&Path>,
    data_dir: &Path,
    plugins: &[Plugin],
) -> Vec<DecisionRule> {
    let mut out = match workspace {
        Some(ws) => load_decision_rules(ws, Some(data_dir)),
        // No workspace: the data dir is all there is, and it loads the same way it would for
        // the plain loader, so a plugin rule is simply added to the global set.
        None => load_global_decision_rules(data_dir),
    };
    for plugin in plugins.iter().filter(|p| p.is_user_enabled()) {
        for path in &plugin.rule_files {
            let Some(path) = contained_regular_file(path, &plugin.root) else { continue };
            let Some(raw) = read_capped(&path, MAX_RULE_BYTES) else { continue };
            let fm = parse_frontmatter(&raw);
            if fm.trigger.as_deref() != Some("model_decision") {
                continue;
            }
            let inner = fm.name.unwrap_or_else(|| {
                path.file_stem().and_then(|s| s.to_str()).map(str::to_owned).unwrap_or_default()
            });
            if inner.is_empty() {
                continue;
            }
            let name = plugins::namespaced(&plugin.id, &inner);
            if out.iter().any(|r| r.name == name) {
                continue;
            }
            let description = fm.description.unwrap_or_else(|| first_line_summary(&fm.body));
            if description.is_empty() {
                continue;
            }
            out.push(DecisionRule {
                name,
                description,
                path,
                scope: RuleScope::Workspace,
                root: plugin.root.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.truncate(MAX_DECISION_RULES);
    out
}

fn load_decision_rule_paths(
    paths: &[PathBuf],
    root: &Path,
    scope: RuleScope,
    out: &mut Vec<DecisionRule>,
) {
    for p in paths {
        let Some(p) = contained_regular_file(p, root) else { continue };
        if out.iter().any(|r| r.path == p) {
            continue;
        }
        let Some(raw) = read_capped(&p, MAX_RULE_BYTES) else { continue };
        let fm = parse_frontmatter(&raw);
        if fm.trigger.as_deref() != Some("model_decision") {
            continue;
        }
        let name = fm.name.unwrap_or_else(|| {
            p.file_stem().and_then(|s| s.to_str()).map(str::to_owned).unwrap_or_default()
        });
        if name.is_empty() {
            continue;
        }
        let description = fm.description.unwrap_or_else(|| first_line_summary(&fm.body));
        if description.is_empty() {
            continue;
        }
        out.push(DecisionRule { name, description, path: p, scope, root: root.to_path_buf() });
    }
}

/// The system-prompt section for on-demand rules (empty when there are none).
pub fn decision_rules_section(rules: &[DecisionRule]) -> String {
    if rules.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "# On-demand rules\n\nThe rules below match this project but are not loaded yet. When a \
         request seems to match one, call `{READ_RULE}` with its name to load the full text, then \
         follow those instructions.\n\n"
    );
    for r in rules {
        s.push_str(&format!("- `{}`: {}\n", r.name, r.description));
    }
    s
}

/// Parsed `---` frontmatter of a rules or workflow file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    /// Lowercased `trigger` value (`always_on`, `manual`, `glob`, ...).
    pub trigger: Option<String>,
    /// Glob patterns from `globs:` (used when `trigger: glob`; empty means apply always).
    pub globs: Vec<String>,
    pub body: String,
}

/// Parse frontmatter of a rules, workflow, or skill file; delegates to the full YAML-subset
/// parser in [`super::frontmatter`]. Without a complete block the whole text is the body.
pub fn parse_frontmatter(raw: &str) -> Frontmatter {
    let fm = super::frontmatter::parse(raw);
    Frontmatter {
        name: fm.str("name"),
        description: fm.str("description"),
        trigger: fm.str("trigger").map(|t| t.to_lowercase()),
        globs: fm.list("globs"),
        body: fm.body,
    }
}

/// Split `---` frontmatter; returns (`description` value, body).
pub fn split_frontmatter(raw: &str) -> (String, String) {
    let fm = parse_frontmatter(raw);
    (fm.description.unwrap_or_default(), fm.body)
}

fn mention_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"@\[([^\]\n]{1,200})\]\(([^)\n]{1,400})\)").expect("static regex"))
}

/// Replace `@[label](path)` in a rule body with the referenced file (one level, capped).
/// Targets are resolved relative to the rule file and must stay inside `root` (the
/// canonicalized workspace): `~/...`, absolute paths, `..` escapes and symlinks that leave the
/// workspace are not followed, and the token is left as written. A rules file is plain
/// workspace content, so it must not be able to pull `~/.ssh/id_rsa` or `C:\Windows\...`
/// into the system prompt just by mentioning it.
fn expand_includes(body: &str, base: &Path, root: &Path) -> String {
    mention_regex()
        .replace_all(body, |c: &regex::Captures| {
            let Some(path) = resolve_include(&c[2], base, root) else { return c[0].to_owned() };
            match read_capped(&path, MAX_RULE_BYTES) {
                Some(raw) => parse_frontmatter(&raw).body.trim_end().to_owned(),
                None => c[0].to_owned(),
            }
        })
        .into_owned()
}

/// Resolve one include target to a regular file inside `root`, or `None` when it is not one.
fn resolve_include(target: &str, base: &Path, root: &Path) -> Option<PathBuf> {
    let t = target.trim();
    if t.is_empty() || t.starts_with('~') || t.contains("://") {
        return None;
    }
    let raw = Path::new(t);
    // A Windows path with a drive letter or a leading separator is absolute regardless of the
    // platform's `is_absolute` (which is true on Windows only); an absolute target would be
    // `base.join(abs)` == `abs`, i.e. read a file outside the workspace.
    let is_abs = raw.is_absolute()
        || t.starts_with('\\')
        || t.starts_with('/')
        || t.as_bytes().get(1) == Some(&b':');
    if is_abs {
        return None;
    }
    contained_regular_file(&base.join(raw), root)
}

/// True when `p` is a regular file (not a symlink or junction) whose canonical path lies
/// inside `root`. The canonical path is returned so callers read exactly what was checked.
fn contained_regular_file(p: &Path, root: &Path) -> Option<PathBuf> {
    if !std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file()) {
        return None;
    }
    let c = dunce::canonicalize(p).ok()?;
    (c.starts_with(root) && c.is_file()).then_some(c)
}

// ---------------------------------------------------------------------------
// Workflows
// ---------------------------------------------------------------------------

/// Where a workflow came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowScope {
    Workspace,
    Global,
}

/// A `/name` workflow from a Markdown file with optional `name:` / `description:` frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    /// Frontmatter `name` when it is a valid command name, else the file stem; invoked as `/<name>`.
    pub name: String,
    /// Frontmatter `description`, else the first non-empty body line (max 80 bytes).
    pub description: String,
    pub body: String,
    pub scope: WorkflowScope,
    pub path: PathBuf,
}

/// Workflows from `<workspace>/.agents/workflows/*.md` and `<data-dir>/workflows/*.md`.
/// A workspace workflow shadows a global one with the same name. Files whose name is not a
/// valid command name (see [`is_command_name`]) are skipped.
pub fn load_workflows(workspace: Option<&Path>, data_dir: &Path) -> Vec<Workflow> {
    let mut dirs = Vec::new();
    if let Some(ws) = workspace {
        dirs.push((ws.join(".agents").join("workflows"), WorkflowScope::Workspace));
    }
    dirs.push((data_dir.join("workflows"), WorkflowScope::Global));
    let mut out: Vec<Workflow> = Vec::new();
    for (dir, scope) in dirs {
        for p in md_files(&dir) {
            let Some(raw) = read_capped(&p, MAX_RULE_BYTES) else { continue };
            let fm = parse_frontmatter(&raw);
            let stem = p.file_stem().and_then(|s| s.to_str()).map(str::to_owned);
            let Some(name) = fm.name.filter(|n| is_command_name(n)).or(stem.filter(|n| is_command_name(n))) else {
                continue;
            };
            if out.iter().any(|w| w.name == name) {
                continue;
            }
            let description = fm.description.unwrap_or_else(|| first_line_summary(&fm.body));
            out.push(Workflow { name, description, body: fm.body, scope, path: p });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `[A-Za-z0-9_-]{1,64}`: what may follow `/`.
pub fn is_command_name(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn first_line_summary(body: &str) -> String {
    let line = body.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("");
    let mut l = line.to_owned();
    if l.len() > 80 {
        truncate_utf8(&mut l, 77);
        l.push_str("...");
    }
    l
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

/// Tool the model calls to load a skill's instructions into the conversation.
pub const ACTIVATE_SKILL: &str = "activate_skill";
/// Max skills listed in the system prompt.
pub const MAX_SKILLS: usize = 64;
/// Max bytes of one `SKILL.md` returned by [`ACTIVATE_SKILL`].
pub const MAX_SKILL_BYTES: usize = 48 * 1024;
/// Max entries of a skill directory listed next to its instructions.
const MAX_SKILL_FILES: usize = 50;

/// A skill: `<dir>/<name>/SKILL.md` with `name:` / `description:` frontmatter. Only the name
/// and description enter the system prompt; the body is loaded on demand by [`ACTIVATE_SKILL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Frontmatter `name` when it is a valid command name, else the directory name.
    pub name: String,
    pub description: String,
    /// Canonical path of `SKILL.md`.
    pub path: PathBuf,
    pub scope: WorkflowScope,
}

/// Skills from `<workspace>/.agents/skills/*/SKILL.md` and `<data-dir>/skills/*/SKILL.md`.
/// A workspace skill shadows a global one with the same name. A skill needs a description
/// (frontmatter, else the first body line); `SKILL.md` must be a regular file inside its
/// skills directory (symlinks out of it are ignored). At most [`MAX_SKILLS`], sorted by name.
pub fn load_skills(workspace: Option<&Path>, data_dir: &Path) -> Vec<Skill> {
    let mut dirs = Vec::new();
    if let Some(ws) = workspace {
        dirs.push((ws.join(".agents").join("skills"), WorkflowScope::Workspace));
    }
    dirs.push((data_dir.join("skills"), WorkflowScope::Global));
    let mut out: Vec<Skill> = Vec::new();
    for (dir, scope) in dirs {
        let Ok(root) = dunce::canonicalize(&dir) else { continue };
        let mut subdirs: Vec<PathBuf> = std::fs::read_dir(&root)
            .map(|rd| rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect())
            .unwrap_or_default();
        subdirs.sort();
        for sub in subdirs {
            let Some(path) = contained_regular_file(&sub.join("SKILL.md"), &root) else { continue };
            let Some(raw) = read_capped(&path, MAX_RULE_BYTES) else { continue };
            let fm = parse_frontmatter(&raw);
            let dir_name = sub.file_name().and_then(|s| s.to_str()).map(str::to_owned);
            let Some(name) = fm.name.filter(|n| is_command_name(n)).or(dir_name.filter(|n| is_command_name(n))) else {
                continue;
            };
            if out.iter().any(|s| s.name == name) {
                continue;
            }
            let description = fm.description.unwrap_or_else(|| first_line_summary(&fm.body));
            if description.is_empty() {
                continue;
            }
            out.push(Skill { name, description, path, scope });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.truncate(MAX_SKILLS);
    out
}

/// [`load_skills`] plus the skills every enabled plugin contributes.
///
/// A plugin keeps its content in `skills/<name>/SKILL.md`, the same layout as the user's own
/// directories, so the frontmatter, description and size rules are the existing ones and a
/// plugin cannot mean something different by the same file name. The contributed name is
/// namespaced (`<plugin>__<name>`): two plugins, or a plugin and a workspace skill, may well
/// call their skill `review`, and the model addresses skills by name. Plugin content is
/// workspace-scoped whatever its discovery root, because only the workspace plugin list is
/// visible to the agent (a global plugin's skills appear only once a workspace plugin of the
/// same id does not shadow it, and claiming `Global` would let it look like an installed one).
///
/// A plugin skill that fails the existing checks (no `SKILL.md`, no description, a name that
/// does not sanitize) is skipped like any other; the result stays sorted and capped at
/// [`MAX_SKILLS`] overall.
pub fn load_skills_with_plugins(workspace: Option<&Path>, data_dir: &Path, plugins: &[Plugin]) -> Vec<Skill> {
    let mut out = load_skills(workspace, data_dir);
    for plugin in plugins.iter().filter(|p| p.is_user_enabled()) {
        for dir in &plugin.skill_dirs {
            let Some(path) = contained_regular_file(&dir.join("SKILL.md"), &plugin.root) else { continue };
            let Some(raw) = read_capped(&path, MAX_RULE_BYTES) else { continue };
            let fm = parse_frontmatter(&raw);
            let dir_name = dir.file_name().and_then(|s| s.to_str()).map(str::to_owned);
            let Some(inner) = fm.name.filter(|n| is_command_name(n)).or(dir_name.filter(|n| is_command_name(n))) else {
                continue;
            };
            let name = plugins::namespaced(&plugin.id, &inner);
            if out.iter().any(|s| s.name == name) {
                continue;
            }
            let description = fm.description.unwrap_or_else(|| first_line_summary(&fm.body));
            if description.is_empty() {
                continue;
            }
            out.push(Skill { name, description, path, scope: WorkflowScope::Workspace });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.truncate(MAX_SKILLS);
    out
}

/// The skills system-prompt section (empty when there are none).
pub fn skills_section(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = format!(
        "# Skills\n\nSkills are instruction packages the user installed. When a request matches a skill's \
         description, call `{ACTIVATE_SKILL}` with its name before starting, then follow the returned \
         instructions. Do not activate a skill that does not fit the request.\n\n"
    );
    for sk in skills {
        s.push_str(&format!("- `{}`: {}\n", sk.name, sk.description));
    }
    s
}

/// The text [`ACTIVATE_SKILL`] returns: the `SKILL.md` body (frontmatter stripped, capped at
/// [`MAX_SKILL_BYTES`]) followed by the skill directory's other files, so the model can read
/// referenced scripts or documents with its normal file tools. `None` when the file is gone.
pub fn skill_instructions(skill: &Skill) -> Option<String> {
    let raw = read_capped(&skill.path, MAX_SKILL_BYTES)?;
    let body = parse_frontmatter(&raw).body;
    let dir = skill.path.parent()?;
    let mut s = format!("Skill `{}` ({})\n\n{}\n", skill.name, dir.display(), body.trim_end());
    let files: Vec<String> = walkdir::WalkDir::new(dir)
        .min_depth(1)
        .max_depth(3)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file() && e.path() != skill.path)
        .take(MAX_SKILL_FILES + 1)
        .map(|e| rel_display(e.path(), dir))
        .collect();
    if !files.is_empty() {
        s.push_str("\nFiles in this skill directory (paths relative to it):\n");
        for f in files.iter().take(MAX_SKILL_FILES) {
            s.push_str(&format!("- {f}\n"));
        }
        if files.len() > MAX_SKILL_FILES {
            s.push_str("- ...\n");
        }
    }
    Some(s)
}

/// Declaration of [`ACTIVATE_SKILL`], restricted to the given skill names.
pub fn skill_declaration(skills: &[Skill]) -> crate::api::FunctionDeclaration {
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    crate::api::FunctionDeclaration {
        name: ACTIVATE_SKILL.into(),
        description: "Load the full instructions of one installed skill listed in the system prompt.".into(),
        parameters_json_schema: serde_json::json!({"type":"object","properties":{
            "name":{"type":"string","enum":names}
        },"required":["name"]}),
    }
}

/// Declaration of [`READ_RULE`], restricted to the given on-demand rule names.
pub fn read_rule_declaration(rules: &[DecisionRule]) -> crate::api::FunctionDeclaration {
    let names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
    crate::api::FunctionDeclaration {
        name: READ_RULE.into(),
        description: "Load the full text of one on-demand rule listed in the system prompt.".into(),
        parameters_json_schema: serde_json::json!({"type":"object","properties":{
            "name":{"type":"string","enum":names}
        },"required":["name"]}),
    }
}

/// The text [`READ_RULE`] returns: the rule's body (frontmatter stripped, includes expanded,
/// capped at [`MAX_RULE_BYTES`]). `None` when the file is gone.
pub fn read_rule_text(rule: &DecisionRule) -> Option<String> {
    let raw = read_capped(&rule.path, MAX_RULE_BYTES)?;
    let fm = parse_frontmatter(&raw);
    let text = expand_includes(&fm.body, rule.path.parent().unwrap_or(&rule.root), &rule.root);
    Some(text.trim_end().to_owned())
}

// ---------------------------------------------------------------------------
// @ mentions
// ---------------------------------------------------------------------------

/// An `@` reference inserted by the mention popup: `@[label](relative/path)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub label: String,
    /// Workspace-relative path with `/` separators; folders end with `/`.
    pub path: String,
    pub is_dir: bool,
}

impl Mention {
    pub fn to_token(&self) -> String {
        format!("@[{}]({})", self.label, self.path)
    }
}

/// Find `@[label](path)` tokens in a prompt. The agent resolves them through `Workspace`
/// (files are attached as context, folders as a listing) before sending the turn.
pub fn parse_mentions(prompt: &str) -> Vec<Mention> {
    mention_regex()
        .captures_iter(prompt)
        .map(|c| {
            let path = c[2].to_owned();
            Mention { label: c[1].to_owned(), is_dir: path.ends_with('/'), path }
        })
        .collect()
}

/// What one mention resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionContent {
    /// File text (frontmatter kept), cut to the per-file or remaining total budget.
    File { text: String, size: u64, truncated: bool },
    /// Depth-1 listing; folders end with `/`.
    Folder { entries: Vec<String>, truncated: bool },
    /// Not attached (outside the workspace, missing, binary, budget used up).
    Skipped(String),
}

/// One resolved mention, in prompt order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionAttachment {
    pub mention: Mention,
    pub content: MentionContent,
}

/// Resolve mentions through `ws` (escapes are rejected by [`Workspace::resolve`]). Duplicate
/// paths are attached once. Blocking: run in `spawn_blocking`.
pub fn resolve_mentions(ws: &Workspace, mentions: &[Mention]) -> Vec<MentionAttachment> {
    let mut seen = HashSet::new();
    let mut budget = MAX_MENTION_TOTAL;
    let mut out = Vec::new();
    for m in mentions {
        let key = m.path.trim_end_matches('/').to_owned();
        if !seen.insert(key.clone()) {
            continue;
        }
        let content = resolve_one(ws, &key, &mut budget);
        out.push(MentionAttachment { mention: m.clone(), content });
    }
    out
}

fn resolve_one(ws: &Workspace, rel: &str, budget: &mut usize) -> MentionContent {
    let target = if rel.is_empty() { "." } else { rel };
    let path = match ws.resolve(target) {
        Ok(p) => p,
        Err(e) => return MentionContent::Skipped(e.to_string()),
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return MentionContent::Skipped("not found".into());
    };
    if meta.is_dir() {
        if *budget == 0 {
            return MentionContent::Skipped("attachment budget used up".into());
        }
        // A folder listing is attachment content too: charge it, and cut it to what is left
        // of MAX_MENTION_TOTAL, so many folder mentions (each up to MAX_LISTING_ENTRIES names
        // of arbitrary length) cannot push the prompt past the budget.
        let mut listing = list_folder(&path);
        if let MentionContent::Folder { entries, truncated } = &mut listing {
            let mut cost = 0usize;
            let mut keep = 0usize;
            for e in entries.iter() {
                if cost + e.len() + 1 > *budget {
                    *truncated = true;
                    break;
                }
                cost += e.len() + 1;
                keep += 1;
            }
            entries.truncate(keep);
            *budget -= cost;
        }
        return listing;
    }
    if *budget == 0 {
        return MentionContent::Skipped("attachment budget used up".into());
    }
    let cap = MAX_MENTION_FILE_BYTES.min(*budget);
    let mut buf = Vec::new();
    let read = std::fs::File::open(&path).and_then(|f| f.take(cap as u64 + 1).read_to_end(&mut buf));
    if let Err(e) = read {
        return MentionContent::Skipped(format!("unreadable: {e}"));
    }
    if buf[..buf.len().min(8192)].contains(&0) {
        return MentionContent::Skipped("binary file".into());
    }
    let truncated = buf.len() > cap || meta.len() > cap as u64;
    buf.truncate(cap);
    let text = utf8_prefix(&buf);
    *budget -= text.len().min(*budget);
    MentionContent::File { text, size: meta.len(), truncated }
}

fn list_folder(dir: &Path) -> MentionContent {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return MentionContent::Skipped("unreadable folder".into());
    };
    let mut entries: Vec<String> = rd
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                name + "/"
            } else {
                name
            }
        })
        .collect();
    // Folders first, then files, each alphabetical (case-insensitive).
    entries.sort_by_key(|e| (!e.ends_with('/'), e.to_lowercase()));
    let truncated = entries.len() > MAX_LISTING_ENTRIES;
    entries.truncate(MAX_LISTING_ENTRIES);
    MentionContent::Folder { entries, truncated }
}

/// Text block appended to the user prompt for `attachments` (empty when there are none).
pub fn attachments_block(attachments: &[MentionAttachment]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\n<attached_context note=\"Content of the items the user referenced with @.\">\n");
    for a in attachments {
        let path = &a.mention.path;
        match &a.content {
            MentionContent::File { text, size, truncated } => {
                let fence = fence_for(text);
                s.push_str(&format!("<file path=\"{path}\" bytes=\"{size}\"{}>\n{fence}\n", if *truncated { " truncated=\"true\"" } else { "" }));
                s.push_str(text);
                if !text.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(&format!("{fence}\n</file>\n"));
            }
            MentionContent::Folder { entries, truncated } => {
                s.push_str(&format!("<folder path=\"{path}\"{}>\n", if *truncated { " truncated=\"true\"" } else { "" }));
                for e in entries {
                    s.push_str(e);
                    s.push('\n');
                }
                s.push_str("</folder>\n");
            }
            MentionContent::Skipped(why) => {
                s.push_str(&format!("<not_attached path=\"{path}\" reason=\"{}\"/>\n", why.replace('"', "'")));
            }
        }
    }
    s.push_str("</attached_context>");
    s
}

/// `prompt` plus the attachments for its mentions; unchanged when it has none.
/// Blocking: run in `spawn_blocking`.
pub fn expand_mentions(ws: &Workspace, prompt: &str) -> String {
    let mentions = parse_mentions(prompt);
    if mentions.is_empty() {
        return prompt.to_owned();
    }
    format!("{prompt}{}", attachments_block(&resolve_mentions(ws, &mentions)))
}

/// A backtick fence longer than any backtick run in `text`.
fn fence_for(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    "`".repeat((longest + 1).max(3))
}

/// Fuzzy subsequence score (higher is better); `None` when `query` is not a subsequence.
/// Consecutive matches and matches after `/ _ - .` score extra; long paths score less.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let cand: Vec<char> = candidate.to_lowercase().chars().collect();
    let mut score = 0;
    let mut last: Option<usize> = None;
    let mut i = 0;
    for q in query.to_lowercase().chars() {
        let pos = cand[i..].iter().position(|&c| c == q)? + i;
        score += if last.is_some_and(|l| l + 1 == pos) { 5 } else { 1 };
        if pos == 0 || matches!(cand[pos - 1], '/' | '_' | '-' | '.') {
            score += 3;
        }
        last = Some(pos);
        i = pos + 1;
    }
    Some(score - cand.len() as i32 / 16)
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

/// Plan for compacting history: summarize everything before `keep_from` into one note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionPlan {
    /// Index of the first history content kept verbatim (always a user text turn).
    pub keep_from: usize,
}

/// Decide whether to compact given the last prompt token count and the model's limit.
pub fn should_compact(prompt_tokens: u64, token_limit: u64) -> bool {
    token_limit > 0 && prompt_tokens as f64 >= token_limit as f64 * COMPACT_THRESHOLD as f64
}

/// Like [`should_compact`] with an explicit threshold (the `compact_threshold` setting). When
/// the server has not reported a count yet, the local estimate of `contents` is used.
pub fn needs_compaction(last_prompt_tokens: Option<u64>, contents: &[Content], token_limit: u64, threshold: f32) -> bool {
    let tokens = last_prompt_tokens.unwrap_or_else(|| estimate_tokens(contents));
    token_limit > 0 && tokens as f64 >= token_limit as f64 * threshold.clamp(0.05, 1.0) as f64
}

/// Input token limit for `model`.
pub fn model_token_limit(_model: &str) -> u64 {
    DEFAULT_TOKEN_LIMIT
}

/// Where to cut: keep the last `keep_turns` user text turns verbatim. `None` when there is
/// nothing older to summarize.
pub fn plan_compaction(contents: &[Content], keep_turns: usize) -> Option<CompactionPlan> {
    let starts = user_turn_starts(contents);
    let keep = keep_turns.max(1);
    if starts.len() <= keep {
        return None;
    }
    let keep_from = starts[starts.len() - keep];
    (keep_from > 0).then_some(CompactionPlan { keep_from })
}

/// Result of [`compact_history`].
#[derive(Debug, Clone, PartialEq)]
pub struct Compacted {
    /// New history: summary user/model pair, then the kept contents verbatim.
    pub contents: Vec<Content>,
    /// Number of original contents replaced by the summary.
    pub removed: usize,
    pub summary: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
}

/// Header of the user content that carries a compaction summary.
pub const SUMMARY_HEADER: &str = "[Conversation summary] Earlier turns of this conversation were condensed to save context. \
Treat this summary as what happened before the messages that follow.";

/// Model reply paired with the summary so the history keeps alternating.
pub const SUMMARY_ACK: &str = "Understood. I will continue from this summary.";

const SUMMARY_INSTRUCTION: &str = "Stop the task for a moment. Your context is about to be reduced, so write a summary of \
the conversation so far that will replace it. Do not call tools. Use these Markdown headings:\n\
## Goal\nWhat the user wants overall, including constraints and preferences they stated.\n\
## Done so far\nWhat was completed, with exact file paths, commands and results that matter.\n\
## Decisions\nChoices made and why, plus anything the user approved or rejected.\n\
## Open items\nWhat is unfinished, failing or still to verify, and the next step.\n\
Be specific and complete but compact. Keep identifiers, paths and error messages verbatim.";

/// Summarize the contents before the cut with one extra model call and return the new
/// history. `preserved` lines (approved plan path, artifact list, ...) are repeated verbatim
/// in the summary note. `Ok(None)` when there is nothing to compact.
pub async fn compact_history(
    backend: &dyn ModelBackend,
    model: &str,
    prompt_id: &str,
    contents: &[Content],
    keep_turns: usize,
    preserved: &[String],
    cancel: &CancellationToken,
) -> Result<Option<Compacted>, CaError> {
    let Some(plan) = plan_compaction(contents, keep_turns) else { return Ok(None) };
    let request = summary_request(&contents[..plan.keep_from], preserved);
    let mut stream = backend.stream_turn(model.to_owned(), prompt_id.to_owned(), request, cancel.clone());
    let mut summary = String::new();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(CaError::Cancelled),
            n = stream.next() => n,
        };
        let Some(chunk) = next else { break };
        for p in chunk?.first_parts() {
            if !p.is_thought() {
                if let Some(t) = &p.text {
                    summary.push_str(t);
                }
            }
        }
    }
    let summary = summary.trim().to_owned();
    if summary.is_empty() {
        return Err(CaError::Decode("compaction produced an empty summary".into()));
    }
    let mut note = format!("{SUMMARY_HEADER}\n\n{summary}");
    if !preserved.is_empty() {
        note.push_str("\n\n## Still relevant\n");
        for l in preserved {
            note.push_str(&format!("- {l}\n"));
        }
    }
    let mut out = vec![Content::user_text(note), Content::model(vec![Part::text(SUMMARY_ACK)])];
    out.extend_from_slice(&contents[plan.keep_from..]);
    Ok(Some(Compacted {
        removed: plan.keep_from,
        summary,
        tokens_before: estimate_tokens(contents),
        tokens_after: estimate_tokens(&out),
        contents: out,
    }))
}

/// Request that asks the model to summarize `older` (no tools, low temperature).
fn summary_request(older: &[Content], preserved: &[String]) -> GenerateContentRequest {
    let mut contents = History::from_contents(older.to_vec()).for_request();
    if contents.last().is_some_and(|c| c.role == Some(Role::User)) {
        contents.push(Content::model(vec![Part::text("(no reply)")]));
    }
    let mut instruction = SUMMARY_INSTRUCTION.to_owned();
    if !preserved.is_empty() {
        instruction.push_str("\nThese items stay available after the summary; mention them only when needed:\n");
        for l in preserved {
            instruction.push_str(&format!("- {l}\n"));
        }
    }
    contents.push(Content::user_text(instruction));
    GenerateContentRequest {
        contents,
        system_instruction: Some(Content {
            role: Some(Role::User),
            parts: vec![Part::text("You condense coding-agent conversations into accurate working summaries.")],
        }),
        generation_config: Some(GenerationConfig { temperature: Some(0.2), ..Default::default() }),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// `.md` files directly inside `dir`, sorted. Symlinks and junctions are skipped so a checkout
/// cannot point the loader at an arbitrary file outside the workspace. Only the name is
/// filtered here; [`contained_regular_file`] re-checks the canonical path.
fn md_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Read at most `cap` bytes of a text file (lossy UTF-8, cut at a char boundary).
fn read_capped(path: &Path, cap: usize) -> Option<String> {
    let mut buf = Vec::new();
    std::fs::File::open(path).ok()?.take(cap as u64).read_to_end(&mut buf).ok()?;
    Some(utf8_prefix(&buf))
}

/// Decode bytes, dropping a partial UTF-8 sequence at the end (from a byte cap).
fn utf8_prefix(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(e) if e.error_len().is_none() => String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
}

fn rel_display(path: &Path, base: &Path) -> String {
    path.strip_prefix(base).unwrap_or(path).display().to_string().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{CaGenerateResponse, Candidate, FunctionCall, FunctionResponse, GenerateContentResponse};
    use futures::stream::BoxStream;
    use std::sync::Mutex;

    #[test]
    fn frontmatter_description() {
        let (d, b) = split_frontmatter("---\ndescription: Run tests\n---\n1. cargo test\n");
        assert_eq!(d, "Run tests");
        assert_eq!(b.trim(), "1. cargo test");
        assert_eq!(split_frontmatter("plain").0, "");
        let fm = parse_frontmatter("\u{feff}---\r\nname: \"ship\"\r\ntrigger: Manual\r\n---\r\nbody\r\n");
        assert_eq!(fm.name.as_deref(), Some("ship"));
        assert_eq!(fm.trigger.as_deref(), Some("manual"));
        assert_eq!(fm.body.trim(), "body");
        assert_eq!(parse_frontmatter("---\n---\nx").body, "x");
        assert_eq!(parse_frontmatter("---\nno close").body, "---\nno close");
    }

    #[test]
    fn mentions_and_fuzzy() {
        let m = parse_mentions("look at @[main.rs](src/main.rs) and @[ui](src/ui/)");
        assert_eq!(m.len(), 2);
        assert!(m[1].is_dir);
        assert!(fuzzy_score("mrs", "src/main.rs").is_some());
        assert!(fuzzy_score("zz", "src/main.rs").is_none());
        assert!(fuzzy_score("main", "src/main.rs") > fuzzy_score("main", "src/m_a_i_n.rs"));
    }

    #[test]
    fn rules_load_strip_manual_and_include() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        std::fs::create_dir_all(root.join(".agents/rules")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "---\ntrigger: always_on\n---\nUse tabs.\n").unwrap();
        std::fs::write(root.join(".agents/rules/b.md"), "---\ntrigger: manual\n---\nsecret style\n").unwrap();
        std::fs::write(root.join(".agents/rules/style.txt"), "---\nx: y\n---\nincluded text").unwrap();
        std::fs::write(root.join(".agents/rules/a.md"), "See @[style](style.txt) and @README.md\n").unwrap();
        std::fs::write(root.join(".agents/rules/empty.md"), "---\ndescription: x\n---\n").unwrap();
        let rules = load_rules(root);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].text.trim(), "Use tabs.");
        assert_eq!(rules[1].text.trim(), "See included text and @README.md");
        let sec = rules_section(&rules, root);
        assert!(sec.contains("## AGENTS.md"));
        assert!(sec.contains("## .agents/rules/a.md"));
        assert!(!sec.contains("secret style"));
        assert_eq!(rules_section(&[], root), "");
    }

    #[test]
    fn rules_budget() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        std::fs::create_dir_all(root.join(".agents/rules")).unwrap();
        let big = "a".repeat(MAX_RULE_BYTES * 2);
        for n in ["1.md", "2.md", "3.md", "4.md"] {
            std::fs::write(root.join(".agents/rules").join(n), &big).unwrap();
        }
        let rules = load_rules(root);
        assert!(rules.iter().all(|r| r.text.len() <= MAX_RULE_BYTES));
        assert!(rules.iter().map(|r| r.text.len()).sum::<usize>() <= MAX_RULES_TOTAL);
        assert_eq!(rules.len(), 2);
    }

    #[test]
    fn workflows_shadow_global() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".agents/workflows")).unwrap();
        std::fs::create_dir_all(data.path().join("workflows")).unwrap();
        std::fs::write(ws.path().join(".agents/workflows/test.md"), "---\ndescription: ws\n---\nbody").unwrap();
        std::fs::write(data.path().join("workflows/test.md"), "global").unwrap();
        std::fs::write(data.path().join("workflows/deploy.md"), "# Ship it\n\ngo").unwrap();
        std::fs::write(data.path().join("workflows/bad name.md"), "x").unwrap();
        std::fs::write(data.path().join("workflows/x.md"), "---\nname: release\n---\nr").unwrap();
        let w = load_workflows(Some(ws.path()), data.path());
        let names: Vec<_> = w.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["deploy", "release", "test"]);
        assert_eq!(w[2].scope, WorkflowScope::Workspace);
        assert_eq!(w[2].description, "ws");
        assert_eq!(w[0].description, "Ship it");
    }

    #[test]
    fn skills_load_shadow_and_activate() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let wsk = ws.path().join(".agents/skills");
        let gsk = data.path().join("skills");
        for d in [wsk.join("review"), gsk.join("review"), gsk.join("deploy"), gsk.join("empty"), gsk.join("nofile")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(gsk.join("bad name")).unwrap();
        std::fs::write(wsk.join("review/SKILL.md"), "---\ndescription: ws review\n---\nReview carefully.\n").unwrap();
        std::fs::write(wsk.join("review/checklist.md"), "- item").unwrap();
        std::fs::write(gsk.join("review/SKILL.md"), "---\ndescription: global review\n---\nx").unwrap();
        // Invalid frontmatter name falls back to the directory name; description from first line.
        std::fs::write(gsk.join("deploy/SKILL.md"), "---\nname: not valid!\n---\n# Ship the build\n\nsteps").unwrap();
        std::fs::write(gsk.join("empty/SKILL.md"), "---\nname: empty\n---\n").unwrap();
        std::fs::write(gsk.join("bad name/SKILL.md"), "---\ndescription: d\n---\nx").unwrap();

        let skills = load_skills(Some(ws.path()), data.path());
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["deploy", "review"]);
        assert_eq!(skills[0].description, "Ship the build");
        assert_eq!(skills[0].scope, WorkflowScope::Global);
        assert_eq!(skills[1].description, "ws review");
        assert_eq!(skills[1].scope, WorkflowScope::Workspace);

        let sec = skills_section(&skills);
        assert!(sec.contains(ACTIVATE_SKILL));
        assert!(sec.contains("- `review`: ws review"));
        assert_eq!(skills_section(&[]), "");

        let text = skill_instructions(&skills[1]).unwrap();
        assert!(text.contains("Review carefully."));
        assert!(!text.contains("description:"), "frontmatter not stripped: {text}");
        assert!(text.contains("- checklist.md"), "{text}");
        assert!(!text.contains("- SKILL.md"), "{text}");

        let decl = skill_declaration(&skills);
        assert_eq!(decl.name, ACTIVATE_SKILL);
        assert_eq!(decl.parameters_json_schema["properties"]["name"]["enum"], serde_json::json!(["deploy", "review"]));

        std::fs::remove_file(&skills[1].path).unwrap();
        assert!(skill_instructions(&skills[1]).is_none());
    }

    #[test]
    fn plugin_skills_and_rules_are_namespaced_and_joined_to_the_local_ones() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = ws.path();
        // One ordinary workspace skill, so the merged list keeps both and stays sorted.
        std::fs::create_dir_all(root.join(".agents/skills/review")).unwrap();
        std::fs::write(root.join(".agents/skills/review/SKILL.md"), "---\ndescription: ws review\n---\nbody\n").unwrap();
        let plug = root.join(".agents/plugins/p");
        std::fs::create_dir_all(plug.join("skills/review")).unwrap();
        std::fs::create_dir_all(plug.join("rules")).unwrap();
        std::fs::write(plug.join("plugin.json"), r#"{"name":"p"}"#).unwrap();
        std::fs::write(plug.join("skills/review/SKILL.md"), "---\ndescription: plugin review\n---\nbody\n").unwrap();
        std::fs::write(
            plug.join("rules/perf.md"),
            "---\ntrigger: model_decision\ndescription: plugin perf\n---\nUse SIMD.\n",
        )
        .unwrap();
        // An always-on plugin rule is not an on-demand rule.
        std::fs::write(plug.join("rules/always.md"), "---\ntrigger: always_on\n---\nUse tabs.\n").unwrap();

        let found = plugins::discover(Some(root), Some(data.path()), &std::collections::BTreeMap::new());
        assert_eq!(found.len(), 1);
        assert!(found[0].enabled);

        // Simulate the user explicitly enabling the plugin (is_user_enabled requires an explicit choice).
        let choices = std::collections::BTreeMap::from([("p".to_owned(), plugins::PluginChoice::Enabled)]);
        let found = plugins::discover(Some(root), Some(data.path()), &choices);
        assert_eq!(found.len(), 1);
        assert!(found[0].is_user_enabled());

        let skills = load_skills_with_plugins(Some(root), data.path(), &found);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["p__review", "review"]);
        assert_eq!(skills[0].description, "plugin review");
        assert_eq!(skills[0].scope, WorkflowScope::Workspace);

        let rules = load_decision_rules_with_plugins(Some(root), data.path(), &found);
        let names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["p__perf"]);
        assert_eq!(rules[0].description, "plugin perf");
        // The path stays the plugin's own file, so read_rule_text loads its body.
        assert!(rules[0].path.ends_with("perf.md"));
        assert!(read_rule_text(&rules[0]).is_some_and(|t| t.contains("Use SIMD.")));

        // Nothing changes when no plugin is present.
        assert_eq!(
            load_skills_with_plugins(Some(root), data.path(), &[]).len(),
            load_skills(Some(root), data.path()).len()
        );
    }

    #[test]
    fn disabled_plugins_contribute_nothing() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = ws.path();
        let plug = root.join(".agents/plugins/off");
        std::fs::create_dir_all(plug.join("skills/one")).unwrap();
        std::fs::write(plug.join("plugin.json"), r#"{"name":"off"}"#).unwrap();
        std::fs::write(plug.join("skills/one/SKILL.md"), "---\ndescription: d\n---\nbody\n").unwrap();
        let choices = std::collections::BTreeMap::from([("off".to_owned(), plugins::PluginChoice::Disabled)]);
        let found = plugins::discover(Some(root), Some(data.path()), &choices);
        assert!(load_skills_with_plugins(Some(root), data.path(), &found).is_empty());
    }

    #[test]
    fn decision_rules_load_and_render() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        std::fs::create_dir_all(root.join(".agents/rules")).unwrap();
        // always_on rule — excluded from decision rules
        std::fs::write(root.join("AGENTS.md"), "---\ntrigger: always_on\n---\nAlways use tabs.\n").unwrap();
        // model_decision rule with description
        std::fs::write(
            root.join(".agents/rules/perf.md"),
            "---\ntrigger: model_decision\ndescription: >-\n  Apply when working on performance-critical paths.\n---\nUse SIMD.\n",
        ).unwrap();
        // model_decision rule without description — uses first body line
        std::fs::write(
            root.join(".agents/rules/security.md"),
            "---\ntrigger: model_decision\n---\n# Security guidelines\n\nNever log secrets.\n",
        ).unwrap();
        // manual rule — excluded
        std::fs::write(root.join(".agents/rules/manual.md"), "---\ntrigger: manual\n---\nSecret stuff.\n").unwrap();

        // always_on rules still load normally
        let always_on = load_rules(root);
        assert_eq!(always_on.len(), 1);
        assert_eq!(always_on[0].text.trim(), "Always use tabs.");

        let rules = load_decision_rules(root, None);
        let names: Vec<_> = rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["perf", "security"]);
        assert_eq!(rules[0].description, "Apply when working on performance-critical paths.");
        assert_eq!(rules[1].description, "Security guidelines");

        let sec = decision_rules_section(&rules);
        assert!(sec.contains(READ_RULE));
        assert!(sec.contains("- `perf`: Apply when working on performance-critical paths."));
        assert_eq!(decision_rules_section(&[]), "");

        let text = read_rule_text(&rules[0]).unwrap();
        assert!(text.contains("Use SIMD."));
        assert!(!text.contains("trigger:"), "frontmatter not stripped: {text}");

        let decl = read_rule_declaration(&rules);
        assert_eq!(decl.name, READ_RULE);
        assert_eq!(decl.parameters_json_schema["properties"]["name"]["enum"], serde_json::json!(["perf", "security"]));
    }

    #[test]
    fn mention_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/ui")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("src/b.bin"), [1u8, 0, 2]).unwrap();
        std::fs::write(root.join("big.txt"), "é".repeat(MAX_MENTION_FILE_BYTES)).unwrap();
        let ws = Workspace::new(root).unwrap();
        let prompt = "fix @[main.rs](src/main.rs) @[src](src/) @[main.rs](src/main.rs) @[x](../escape.txt) @[b](src/b.bin) @[big](big.txt) @[no](nope.rs)";
        let att = resolve_mentions(&ws, &parse_mentions(prompt));
        assert_eq!(att.len(), 6, "duplicate dropped");
        assert!(matches!(&att[0].content, MentionContent::File { text, truncated: false, .. } if text == "fn main() {}\n"));
        match &att[1].content {
            MentionContent::Folder { entries, truncated } => {
                assert_eq!(entries, &["ui/", "b.bin", "main.rs"]);
                assert!(!truncated);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&att[2].content, MentionContent::Skipped(_)), "escape rejected");
        assert!(matches!(&att[3].content, MentionContent::Skipped(r) if r == "binary file"));
        match &att[4].content {
            MentionContent::File { text, truncated, .. } => {
                assert!(*truncated);
                assert!(text.len() <= MAX_MENTION_FILE_BYTES);
                assert!(text.chars().all(|c| c == 'é'));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&att[5].content, MentionContent::Skipped(_)));
        let out = expand_mentions(&ws, prompt);
        assert!(out.starts_with(prompt));
        assert!(out.contains("<file path=\"src/main.rs\" bytes=\"13\">"));
        assert!(out.contains("<folder path=\"src/\">"));
        assert!(out.contains("truncated=\"true\""));
        assert!(out.ends_with("</attached_context>"));
        assert_eq!(expand_mentions(&ws, "no mentions"), "no mentions");
        assert_eq!(fence_for("a ```` b"), "`````");
    }

    // --- compaction ---

    fn fr(name: &str) -> Content {
        Content {
            role: Some(Role::User),
            parts: vec![Part::function_response(FunctionResponse { id: None, name: name.into(), response: serde_json::json!({"output":"ok"}) })],
        }
    }

    fn call(name: &str) -> Content {
        Content::model(vec![Part { function_call: Some(FunctionCall { id: Some("c".into()), name: name.into(), args: serde_json::json!({}) }), ..Default::default() }])
    }

    /// u0 m0 | u1 call fr m1 | u2 m2
    fn sample() -> Vec<Content> {
        vec![
            Content::user_text("first"),
            Content::model(vec![Part::text("a")]),
            Content::user_text("second"),
            call("read_file"),
            fr("read_file"),
            Content::model(vec![Part::text("b")]),
            Content::user_text("third"),
            Content::model(vec![Part::text("c")]),
        ]
    }

    struct SummaryModel {
        reply: Vec<Result<Vec<Part>, CaError>>,
        seen: Mutex<Vec<GenerateContentRequest>>,
        hang: bool,
    }

    impl ModelBackend for SummaryModel {
        fn stream_turn(&self, _m: String, _p: String, r: GenerateContentRequest, _c: CancellationToken) -> BoxStream<'static, Result<CaGenerateResponse, CaError>> {
            self.seen.lock().unwrap().push(r);
            if self.hang {
                return futures::stream::pending().boxed();
            }
            let items: Vec<_> = self
                .reply
                .iter()
                .map(|r| match r {
                    Ok(parts) => Ok(CaGenerateResponse {
                        response: Some(GenerateContentResponse {
                            candidates: vec![Candidate { content: Some(Content::model(parts.clone())), ..Default::default() }],
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    Err(_) => Err(CaError::Transport("boom".into())),
                })
                .collect();
            futures::stream::iter(items).boxed()
        }
    }

    fn model(reply: Vec<Result<Vec<Part>, CaError>>) -> SummaryModel {
        SummaryModel { reply, seen: Mutex::new(vec![]), hang: false }
    }

    #[test]
    fn compaction_threshold() {
        assert!(should_compact(700, 1000));
        assert!(!should_compact(699, 1000));
        assert!(!should_compact(10, 0));
        assert!(needs_compaction(Some(500), &[], 1000, 0.5));
        assert!(!needs_compaction(None, &[Content::user_text("tiny")], 1000, 0.5));
        assert!(needs_compaction(None, &[Content::user_text("x".repeat(4000))], 1000, 0.5));
        assert_eq!(model_token_limit("gemini-2.5-pro"), DEFAULT_TOKEN_LIMIT);
    }

    #[test]
    fn plan_keeps_recent_turns() {
        let c = sample();
        assert_eq!(plan_compaction(&c, 1), Some(CompactionPlan { keep_from: 6 }));
        assert_eq!(plan_compaction(&c, 2), Some(CompactionPlan { keep_from: 2 }));
        assert_eq!(plan_compaction(&c, 3), None);
        assert_eq!(plan_compaction(&c, 0), Some(CompactionPlan { keep_from: 6 }));
        assert_eq!(plan_compaction(&[], 1), None);
    }

    #[tokio::test]
    async fn compacts_older_turns_via_backend() {
        let c = sample();
        let m = model(vec![
            Ok(vec![Part { text: Some("thinking".into()), thought: Some(true), ..Default::default() }]),
            Ok(vec![Part::text("## Goal\nship ")]),
            Ok(vec![Part::text("it")]),
        ]);
        let preserved = vec!["Approved plan: implementation_plan.md".to_owned()];
        let out = compact_history(&m, "gemini-2.5-pro", "p", &c, 2, &preserved, &CancellationToken::new()).await.unwrap().unwrap();
        assert_eq!(out.removed, 2);
        assert_eq!(out.summary, "## Goal\nship it");
        assert_eq!(out.contents.len(), 2 + 6);
        assert!(out.contents[0].text().starts_with(SUMMARY_HEADER));
        assert!(out.contents[0].text().contains("- Approved plan: implementation_plan.md"));
        assert_eq!(out.contents[1].role, Some(Role::Model));
        assert_eq!(&out.contents[2..], &c[2..]);
        // Alternation holds: roles go user, model, user, ...
        for w in out.contents.windows(2) {
            assert_ne!(w[0].role, w[1].role);
        }
        let seen = m.seen.lock().unwrap();
        let req = &seen[0];
        assert!(req.tools.is_none());
        assert_eq!(req.contents.len(), 3, "older pair plus the instruction");
        assert_eq!(req.contents[0].text(), "first");
        assert!(req.contents[2].text().contains("## Open items"));
        assert!(req.contents[2].text().contains("implementation_plan.md"));
    }

    #[tokio::test]
    async fn compaction_signs_old_calls_and_closes_dangling_user() {
        let c = vec![Content::user_text("a"), call("glob"), fr("glob"), Content::user_text("b"), Content::model(vec![Part::text("x")])];
        let m = model(vec![Ok(vec![Part::text("sum")])]);
        let out = compact_history(&m, "m", "p", &c, 1, &[], &CancellationToken::new()).await.unwrap().unwrap();
        assert_eq!(out.removed, 3);
        assert!(!out.contents[0].text().contains("Still relevant"));
        let seen = m.seen.lock().unwrap();
        let req = &seen[0].contents;
        assert!(req[1].parts[0].thought_signature.is_some());
        assert_eq!(req[3].role, Some(Role::Model), "dangling user content closed");
        assert_eq!(req[4].role, Some(Role::User));
        assert!(c[1].parts[0].thought_signature.is_none(), "input untouched");
    }

    #[tokio::test]
    async fn compaction_errors_and_noop() {
        let cancel = CancellationToken::new();
        let few = vec![Content::user_text("a"), Content::model(vec![Part::text("b")])];
        assert_eq!(compact_history(&model(vec![]), "m", "p", &few, 1, &[], &cancel).await.unwrap(), None);

        let empty = model(vec![Ok(vec![Part::text("  ")])]);
        assert!(matches!(compact_history(&empty, "m", "p", &sample(), 1, &[], &cancel).await, Err(CaError::Decode(_))));

        let failing = model(vec![Ok(vec![Part::text("partial")]), Err(CaError::Cancelled)]);
        assert!(matches!(compact_history(&failing, "m", "p", &sample(), 1, &[], &cancel).await, Err(CaError::Transport(_))));

        let hang = SummaryModel { reply: vec![], seen: Mutex::new(vec![]), hang: true };
        let c2 = CancellationToken::new();
        let t = c2.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            t.cancel();
        });
        assert!(matches!(compact_history(&hang, "m", "p", &sample(), 1, &[], &c2).await, Err(CaError::Cancelled)));
    }

    // --- security regressions ---------------------------------------------------------

    #[test]
    fn includes_cannot_leave_the_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = ws.path();
        std::fs::create_dir_all(root.join(".agents/rules")).unwrap();
        std::fs::write(outside.path().join("secret.txt"), "TOP SECRET").unwrap();
        let abs = outside.path().join("secret.txt").display().to_string().replace('\\', "/");
        // A home reference and an absolute path are both refused; a ../ escape is refused.
        let body = format!(
            "keep @[s]({abs}) and @[h](~/secret.txt) and @[u](../escape.txt) and @[ok](local.txt) bare @name\n"
        );
        std::fs::write(root.join(".agents/rules/a.md"), &body).unwrap();
        std::fs::write(root.join(".agents/rules/local.txt"), "LOCAL").unwrap();
        let rules = load_rules(root);
        assert_eq!(rules.len(), 1);
        assert!(!rules[0].text.contains("TOP SECRET"), "{}", rules[0].text);
        assert!(rules[0].text.contains(&format!("@[s]({abs})")), "absolute include left as written");
        assert!(rules[0].text.contains("@[h](~/secret.txt)"), "home include left as written");
        assert!(rules[0].text.contains("LOCAL"), "in-workspace include still expands");
    }

    #[cfg(unix)]
    #[test]
    fn rule_symlink_to_outside_is_not_loaded() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("AGENTS.md");
        std::fs::write(&secret, "outside rule").unwrap();
        std::os::unix::fs::symlink(&secret, ws.path().join("AGENTS.md")).unwrap();
        let rules = load_rules(ws.path());
        assert!(rules.iter().all(|r| !r.text.contains("outside rule")));
    }

    #[test]
    fn folder_mentions_are_charged_against_the_budget() {
        // Enough folders that MAX_MENTION_TOTAL cannot cover them all: 60 folders x 200 entries
        // of 31 bytes is ~372 KiB against a 256 KiB budget.
        const FOLDERS: usize = 60;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for d in 0..FOLDERS {
            let sub = root.join(format!("d{d}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..MAX_LISTING_ENTRIES {
                std::fs::write(sub.join(format!("{f:04}_file_with_a_long_name.txt")), "").unwrap();
            }
        }
        let ws = Workspace::new(root).unwrap();
        let mentions = (0..FOLDERS)
            .map(|d| Mention { label: format!("d{d}"), path: format!("d{d}/"), is_dir: true })
            .collect::<Vec<_>>();
        let att = resolve_mentions(&ws, &mentions);
        let bytes = |a: &&MentionAttachment| match &a.content {
            MentionContent::Folder { entries, .. } => entries.iter().map(|e| e.len() + 1).sum::<usize>(),
            _ => 0,
        };
        let total: usize = att.iter().map(|a| bytes(&a)).sum();
        assert!(total <= MAX_MENTION_TOTAL, "folder mentions bypass the mention budget: {total}");
        // The listings that did fit are charged: later ones are empty or skipped, not full.
        let listed: usize = att
            .iter()
            .map(|a| match &a.content {
                MentionContent::Folder { entries, .. } => entries.len(),
                _ => 0,
            })
            .sum();
        assert!(listed < FOLDERS * MAX_LISTING_ENTRIES, "every listing was attached in full");
        let cut = att.iter().any(|a| matches!(&a.content, MentionContent::Skipped(_)))
            || att.iter().any(|a| matches!(&a.content, MentionContent::Folder { truncated: true, .. }));
        assert!(cut, "later folders were neither trimmed nor skipped");
    }
}
