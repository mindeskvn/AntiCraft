//! Plugins: bundled skills, rules, hooks and MCP configuration shipped as a directory.
//!
//! Discovery order is precedence order, so earlier sources win:
//! `<workspace>/.agents/plugins/*`, `<workspace>/.agents/plugins.json`,
//! `<data-dir>/plugins/*`, `<data-dir>/plugins.json`. A plugin is identified by the
//! canonical directory it resolves to; ids are first-wins and the result is sorted by id,
//! so the caller sees a stable list no matter how the filesystem enumerates entries.
//!
//! The whole module is a security boundary. A decoded repo, or a repo checked out on a
//! machine whose `git config core.symlinks` is true, is untrusted input: a plugin directory
//! reached through a symlink or junction is rejected outright, every plugin directory must
//! still resolve inside the workspace or data directory that declared it (so a junction in a
//! *parent* component cannot move it), every file a plugin contributes must canonicalize back
//! inside its own root, and `plugins.json` may only name relative paths with no `..`, no drive
//! prefix, no alternate data stream and no Windows device name. Without those checks a clone
//! could add `"path": "C:/Windows"` to `plugins.json` and have the agent feed arbitrary host
//! files into its system prompt.
//!
//! Enable state is resolved per plugin as: an explicit user choice wins, else a manifest
//! `"disabled": true` disables it, else it is enabled. This module only reports the state;
//! deciding which of those capabilities are safe to run without user consent is the caller's
//! job, not ours.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

// ---------------------------------------------------------------------------
// Limits and separators
// ---------------------------------------------------------------------------

/// Most plugins one discovery source contributes. A cloned repo must not be able to make the
/// prompt section unbounded by dropping a thousand directories into `.agents/plugins`.
///
/// The cap is per source (workspace directory scan, workspace config, global directory scan,
/// global config) and never lets one source starve another: a clone that ships [`MAX_PLUGINS`]
/// junk directories must not evict the plugins the user installed for themselves. The total is
/// therefore at most `4 * MAX_PLUGINS` before the final id-sorted [`MAX_PLUGINS`] truncation.
pub const MAX_PLUGINS: usize = 64;
/// A manifest over this size is refused rather than parsed; 256 KiB is far more than a
/// manifest needs and keeps the read itself bounded.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// A `plugins.json` over this size is refused. It lists ids, so it stays tiny in practice.
pub const MAX_CONFIG_BYTES: usize = 8 * 1024;
/// Largest `hooks.json` a plugin may ship.
pub const MAX_HOOKS_BYTES: usize = 64 * 1024;
/// Largest `mcp_config.json` a plugin may ship.
pub const MAX_MCP_BYTES: usize = 64 * 1024;
/// Deepest subdirectory under `rules/` that still contributes rule files.
pub const MAX_RULE_DEPTH: usize = 3;
/// Most skills, rules and `hooks.json` files collected from one plugin, in that order.
pub const MAX_PLUGIN_SKILLS: usize = 64;
pub const MAX_PLUGIN_RULES: usize = 256;
pub const MAX_PLUGIN_HOOKS: usize = 16;
/// Separator between plugin id and inner name. `__` and not `:` or `/` because the model
/// only ever sees command names matching `[A-Za-z0-9_-]` (`context::is_command_name`).
pub const NS: &str = "__";
/// Longest sanitized name [`namespaced`] will produce, in bytes. Matches the command-name cap.
pub const MAX_NAMESPACED_BYTES: usize = 64;

/// Manifest file locations inside a plugin root, highest precedence first.
///
/// `.claude-plugin/plugin.json` and `gemini-extension.json` are deliberately absent: loading
/// them would require the upstream agy install staging step, and reading them un-staged
/// would claim content the plugin cannot actually provide.
const MANIFEST_CANDIDATES: [&str; 3] = [
    "plugin.json",
    ".antigravity-plugin/plugin.json",
    ".gemini-plugin/plugin.json",
];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Where a plugin directory was found. `Workspace` content is untrusted (it arrives with a
/// clone) and `Global` content is content the user installed for themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    /// `<workspace>/.agents/plugins*`.
    Workspace,
    /// `<data-dir>/plugins*`.
    Global,
}

/// Per-plugin user choice, persisted in `settings.json` under `"plugins"`.
///
/// `Default` keeps the choice out of the persisted map's hands: the manifest decides. The
/// enum is kebab-case on disk (`"enabled"`, `"disabled"`) so a hand-edited settings file
/// reads naturally next to the other settings keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginChoice {
    /// No explicit choice: the manifest's `"disabled"` field decides.
    #[default]
    Default,
    /// The user switched the plugin on, overriding a manifest that disables it.
    Enabled,
    /// The user switched the plugin off, overriding a manifest that enables it.
    Disabled,
}

impl PluginChoice {
    /// True when the user made this choice themselves. Callers that grant extra trust (for
    /// example running a plugin's hooks) should require this: otherwise cloning a repo that
    /// ships `hooks.json` would execute its commands on the user's first prompt.
    pub fn is_user(self) -> bool {
        matches!(self, PluginChoice::Enabled | PluginChoice::Disabled)
    }
}

/// One discovered plugin and the content it contributes.
///
/// `PartialEq` only: `mcp_configs` holds [`Value`], which is not `Eq`.
#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    /// Directory-name identity, first-wins across sources; also the settings key.
    pub id: String,
    /// Canonical plugin directory; every contributed path is inside it.
    pub root: PathBuf,
    /// Which of the two discovery roots found this plugin.
    pub source: PluginSource,
    /// Manifest `name`, else the directory name (sanitized to a command name).
    pub name: String,
    /// Manifest `version`, verbatim. Display only: it is not compared or parsed.
    pub version: Option<String>,
    /// Manifest `description`, trimmed. Shown in the customizations UI.
    pub description: Option<String>,
    /// Resolved enable state, see [`resolve_state`].
    pub enabled: bool,
    /// The choice this plugin's state was resolved from, including [`PluginChoice::Default`].
    ///
    /// Kept alongside `enabled` because the two answer different questions: callers that grant
    /// extra trust (running hooks, spawning servers) must require [`PluginChoice::is_user`] on
    /// this field, or a clone shipping `hooks.json` executes its commands on the first prompt.
    pub choice: PluginChoice,
    /// Directories that hold `SKILL.md`, sorted. One entry per skill directory.
    pub skill_dirs: Vec<PathBuf>,
    /// Rule files, sorted. At most [`MAX_RULE_DEPTH`] levels below `rules/`.
    pub rule_files: Vec<PathBuf>,
    /// `hooks.json` files, sorted.
    pub hook_files: Vec<PathBuf>,
    /// Raw, parsed `mcp_config.json` documents. Kept as JSON rather than a typed struct so a
    /// caller can surface them without this module promising a shape it does not launch.
    pub mcp_configs: Vec<Value>,
}

impl Plugin {
    /// Id with the inner name namespaced for the model-visible global namespace.
    pub fn ns(&self, name: &str) -> String {
        namespaced(&self.id, name)
    }

    /// True when the user themselves switched this plugin on.
    ///
    /// Distinct from [`Plugin::enabled`]: a plugin that is enabled by default, because it sits
    /// in a cloned repo and its manifest does not opt out, is `enabled` but not user-enabled.
    /// Anything that executes plugin-supplied code (hooks, MCP servers) must gate on this.
    pub fn is_user_enabled(&self) -> bool {
        self.enabled && self.choice.is_user()
    }

    /// The servers this plugin's `mcp_config.json` documents contribute, parsed and bounded.
    ///
    /// Empty for a plugin whose config is absent or malformed; a broken server list is logged
    /// and skipped rather than failing discovery. Parsing lives in [`crate::agent::mcp::config`]
    /// so plugin servers obey exactly the same caps and skip rules as the user's own config.
    pub fn mcp_servers(&self) -> Vec<crate::agent::mcp::McpServerConfig> {
        let mut out = Vec::new();
        for (i, doc) in self.mcp_configs.iter().enumerate() {
            let source = format!("{} mcp_config.json #{i}", self.id);
            out.extend(crate::agent::mcp::config::parse_servers(doc, &source));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Discover plugins from the four sources, in precedence order, deduplicated by canonical
/// directory and sorted by id.
///
/// `data_dir` is optional so a caller without a data directory (tests, an unconfigured
/// install) can still discover workspace plugins. Symlinked or untrusted plugin directories
/// are logged at debug level and skipped: one bad entry must not abort discovery.
///
/// A plugin found by a source must still canonicalize inside that source's own root, so a
/// junction placed in a parent component (`plugins/link/`, with `link` pointing at
/// `C:/Windows`) is refused even though the plugin directory itself is a plain directory.
pub fn discover(
    workspace: Option<&Path>,
    data_dir: Option<&Path>,
    choices: &BTreeMap<String, PluginChoice>,
) -> Vec<Plugin> {
    let mut plugins: Vec<Plugin> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    if let Some(root) = canonical_root(workspace) {
        let agents = root.join(".agents");
        scan_plugin_root(&agents.join("plugins"), &root, PluginSource::Workspace, choices, &mut seen, &mut plugins);
        scan_config(&agents.join("plugins.json"), &root, PluginSource::Workspace, choices, &mut seen, &mut plugins);
    }
    if let Some(root) = canonical_root(data_dir) {
        scan_plugin_root(&root.join("plugins"), &root, PluginSource::Global, choices, &mut seen, &mut plugins);
        scan_config(&root.join("plugins.json"), &root, PluginSource::Global, choices, &mut seen, &mut plugins);
    }
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    plugins.truncate(MAX_PLUGINS);
    plugins
}

/// Canonical form of a discovery root, or `None` when it does not exist.
///
/// Canonicalizing once, up front, is what makes the later containment checks exact: both sides
/// are then in the same form, so a drive-letter case difference or a `..` in the root cannot
/// make a legitimate plugin look foreign, and a workspace reached through a link compares by
/// where it really is. `dunce` avoids the Windows `\\?\` prefix that would otherwise break
/// every prefix comparison against a path the caller built itself.
fn canonical_root(root: Option<&Path>) -> Option<PathBuf> {
    dunce::canonicalize(root?).ok().filter(|p| p.is_dir())
}

/// Register every plugin directory directly inside `dir`.
///
/// Entries are sorted before registration so a filesystem that returns them in hash order
/// still produces a deterministic "first id wins" result. The cap is applied to this source
/// only, measured from `inserted`, so a source that starts empty always gets its full budget.
fn scan_plugin_root(
    dir: &Path,
    trusted_root: &Path,
    source: PluginSource,
    choices: &BTreeMap<String, PluginChoice>,
    seen: &mut BTreeSet<PathBuf>,
    out: &mut Vec<Plugin>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    // The directory a plugin must resolve inside is the *real* one, so a junction used as
    // `.agents/plugins` itself does not relocate every entry beneath it.
    let Ok(parent) = dunce::canonicalize(dir) else { return };
    let inserted = out.len();
    let mut entries: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| !e.file_type().is_ok_and(|t| t.is_symlink()))
        .map(|e| e.path())
        .collect();
    entries.sort_by_key(|p| entry_name(p));
    for entry in entries {
        if out.len() - inserted >= MAX_PLUGINS {
            break;
        }
        register(&entry, &parent, trusted_root, source, choices, seen, out);
    }
}

/// The plugin directories named by a `plugins.json` file, one entry per path.
///
/// The file accepts a bare array (`["a", "b"]`), an object with that array under `"plugins"`,
/// or an object whose keys are the paths (`{"a": {}, "b": {}}`), each item also allowed to be
/// an object with a `"path"` field. Every named path must be a safe relative path: absolute
/// and home-relative entries are skipped, not followed.
fn scan_config(
    conf: &Path,
    trusted_root: &Path,
    source: PluginSource,
    choices: &BTreeMap<String, PluginChoice>,
    seen: &mut BTreeSet<PathBuf>,
    out: &mut Vec<Plugin>,
) {
    let Some(text) = read_capped(conf, MAX_CONFIG_BYTES) else { return };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        tracing::warn!("plugins: {} is not valid JSON", conf.display());
        return;
    };
    let mut names: Vec<&str> = match &value {
        Value::Array(items) => items.iter().filter_map(item_path).collect(),
        Value::Object(map) => match map.get("plugins") {
            Some(Value::Array(items)) => items.iter().filter_map(item_path).collect(),
            // Keyed map: the key is the path unless the value names one explicitly.
            _ => map.iter().map(|(k, v)| item_path(v).unwrap_or(k.as_str())).collect(),
        },
        _ => return,
    };
    let Some(base) = conf.parent() else { return };
    // The config file itself must live in the root it is trusted for; otherwise a config found
    // through a junction could name paths relative to a directory the user never installed.
    let Ok(base) = dunce::canonicalize(base) else { return };
    if !base.starts_with(trusted_root) {
        tracing::debug!("plugins: {} is outside {}", conf.display(), trusted_root.display());
        return;
    }
    names.sort_unstable();
    let inserted = out.len();
    for name in names {
        if out.len() - inserted >= MAX_PLUGINS {
            tracing::debug!("plugins: {} declares more than {MAX_PLUGINS} plugins; the rest are ignored", conf.display());
            break;
        }
        let Some(rel) = trusted_relative(Path::new(name)) else {
            tracing::debug!("plugins: refusing {name:?} in {}", conf.display());
            continue;
        };
        // No canonicalize here: `register` refuses a symlinked entry and re-checks that the
        // canonical directory sits directly inside the lexical parent it was named through, so
        // a junction anywhere in `rel` is caught with the exact component that moved.
        let candidate = base.join(rel);
        let Some(parent) = candidate.parent() else { continue };
        register(&candidate, parent, trusted_root, source, choices, seen, out);
    }
}

/// The path an item of `plugins.json` names: a string, or an object's `"path"` field.
fn item_path(item: &Value) -> Option<&str> {
    match item {
        Value::String(s) => Some(s.as_str()),
        Value::Object(m) => m.get("path").and_then(Value::as_str),
        _ => None,
    }
}

/// Validate one plugin directory and append it when it is a new, trusted plugin.
///
/// A directory counts as a plugin when it has a manifest, a `skills/` directory, or a
/// `SKILL.md` of its own; anything else in the plugins directory is ignored rather than
/// reported, because clones routinely carry stray files there.
fn register(
    candidate: &Path,
    expected_parent: &Path,
    trusted_root: &Path,
    source: PluginSource,
    choices: &BTreeMap<String, PluginChoice>,
    seen: &mut BTreeSet<PathBuf>,
    out: &mut Vec<Plugin>,
) {
    let Some(canonical) = trusted_dir(candidate, expected_parent, trusted_root) else { return };
    if !seen.insert(canonical.clone()) {
        return;
    }
    if !looks_like_plugin(&canonical) {
        seen.remove(&canonical);
        return;
    }
    let plugin = build_plugin(&canonical, source, choices);
    if out.iter().any(|p| p.id == plugin.id) {
        // The id already belongs to a higher-precedence source.
        tracing::debug!("plugins: {} shadowed by an earlier plugin with id {:?}", plugin.root.display(), plugin.id);
        return;
    }
    out.push(plugin);
}

/// The plugin structure for an already-validated canonical directory.
///
/// The id is the directory name and the display name is the manifest's: keeping the settings
/// key tied to the directory (rather than to a manifest field a plugin can rename at will)
/// means an unrelated update to a clone cannot inherit another plugin's enable choice. The id
/// is kept verbatim, not sanitized, for the same reason; [`crate::settings`] refuses an id that
/// could be a path, and [`namespaced`] sanitizes when the id enters a model-visible name.
fn build_plugin(root: &Path, source: PluginSource, choices: &BTreeMap<String, PluginChoice>) -> Plugin {
    let manifest = load_manifest(root).unwrap_or_default();
    let id = entry_name(root);
    let name = manifest.name.as_deref().and_then(sanitize_token).unwrap_or_else(|| id.clone());
    let choice = choices.get(&id).copied().unwrap_or_default();
    Plugin {
        enabled: resolve_state(choice, manifest.disabled),
        choice,
        id,
        name,
        version: manifest.version,
        description: manifest.description,
        skill_dirs: skill_dirs(root),
        rule_files: rule_files(root),
        hook_files: hook_files(root),
        mcp_configs: mcp_configs(root),
        root: root.to_path_buf(),
        source,
    }
}

/// The fields of a plugin manifest this build understands. Unknown fields are ignored, and
/// values of the wrong type count as absent: a malformed manifest must not take the whole
/// plugins list down with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Manifest {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    disabled: bool,
}

/// Read the highest-precedence manifest in `root`.
///
/// Returns `None` when the plugin ships no manifest at all, and also when the first manifest
/// present is unreadable or malformed: a plugin must not be able to fall back to a
/// lower-precedence manifest by shipping a deliberately broken one.
///
/// The manifest is opened through [`contained_file`], so a `plugin.json` symlinked to a host
/// file is ignored: otherwise a clone could display another plugin's name while shipping its
/// own hooks and rules under that name.
fn load_manifest(root: &Path) -> Option<Manifest> {
    for rel in MANIFEST_CANDIDATES {
        let Some(path) = contained_file(&root.join(rel), root) else { continue };
        let Some(text) = read_capped(&path, MAX_MANIFEST_BYTES) else {
            tracing::warn!("plugins: cannot read manifest {}", path.display());
            return None;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            tracing::warn!("plugins: manifest {} is not valid JSON", path.display());
            return None;
        };
        return Some(Manifest {
            name: field(&value, "name"),
            version: field(&value, "version"),
            description: field(&value, "description"),
            disabled: value.get("disabled").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    None
}

/// A trimmed string field of a JSON value, or `None` when absent, empty or not a string.
fn field(value: &Value, key: &str) -> Option<String> {
    let s = value.get(key)?.as_str()?.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

/// Whether `root` carries anything plugin-shaped. See [`register`].
///
/// A symlinked marker does not count: [`looks_like_plugin`] answers "can this directory be
/// loaded", and [`load_manifest`] would ignore that link anyway.
fn looks_like_plugin(root: &Path) -> bool {
    MANIFEST_CANDIDATES.iter().any(|rel| contained_file(&root.join(rel), root).is_some())
        || contained_dir(&root.join("skills"), root).is_some()
        || contained_file(&root.join("SKILL.md"), root).is_some()
}

/// Resolve the enable state: a user choice wins, then the manifest's `"disabled"` flag, then
/// enabled.
pub fn resolve_state(choice: PluginChoice, manifest_disabled: bool) -> bool {
    match choice {
        PluginChoice::Enabled => true,
        PluginChoice::Disabled => false,
        PluginChoice::Default => !manifest_disabled,
    }
}

// ---------------------------------------------------------------------------
// Contributed content
// ---------------------------------------------------------------------------

/// Skill directories under `skills/`, sorted. Each contains a `SKILL.md`, so the loader can
/// apply its own frontmatter and size rules unchanged.
fn skill_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(skills) = contained_dir(&root.join("skills"), root) else { return out };
    let Ok(rd) = std::fs::read_dir(&skills) else { return out };
    let mut entries: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| !e.file_type().is_ok_and(|t| t.is_symlink()))
        .map(|e| e.path())
        .collect();
    entries.sort_by_key(|p| entry_name(p));
    for entry in entries {
        if out.len() >= MAX_PLUGIN_SKILLS {
            break;
        }
        let Some(dir) = contained_dir(&entry, root) else { continue };
        if contained_file(&dir.join("SKILL.md"), root).is_some() {
            out.push(dir);
        }
    }
    // A plugin may also ship a single skill directly at its root.
    if out.is_empty() && contained_file(&root.join("SKILL.md"), root).is_some() {
        out.push(root.to_path_buf());
    }
    out
}

/// Rule files under `rules/`, at most [`MAX_RULE_DEPTH`] levels deep, sorted.
///
/// `follow_links(false)` plus the per-file containment check means a symlinked subdirectory
/// or file pointing out of the plugin is skipped instead of pulling in host content.
fn rule_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(rules) = contained_dir(&root.join("rules"), root) else { return out };
    for entry in walkdir::WalkDir::new(&rules)
        .min_depth(1)
        .max_depth(MAX_RULE_DEPTH)
        .follow_links(false)
        .sort_by_file_name()
    {
        if out.len() >= MAX_PLUGIN_RULES {
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let is_md = entry.path().extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("md"));
        if !is_md {
            continue;
        }
        if let Some(path) = contained_file(entry.path(), root) {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// `hooks.json` files at the plugin root or one directory deep, sorted.
fn hook_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for rel in ["hooks.json", ".agents/hooks.json"] {
        if out.len() >= MAX_PLUGIN_HOOKS {
            break;
        }
        let Some(path) = contained_file(&root.join(rel), root) else { continue };
        if read_capped(&path, MAX_HOOKS_BYTES).is_some() {
            out.push(path);
        }
    }
    out
}

/// Parsed `mcp_config.json` documents found at the plugin root.
///
/// Kept raw: this build has no MCP client, so the caller surfaces them as "declared but not
/// running" rather than pretending the servers exist.
fn mcp_configs(root: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    for rel in ["mcp_config.json", ".agents/mcp_config.json"] {
        let Some(path) = contained_file(&root.join(rel), root) else { continue };
        let Some(text) = read_capped(&path, MAX_MCP_BYTES) else {
            tracing::warn!("plugins: cannot read mcp config {}", path.display());
            continue;
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => out.push(value),
            Err(e) => tracing::warn!("plugins: mcp config {} is not valid JSON: {e}", path.display()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Namespacing
// ---------------------------------------------------------------------------

/// `<plugin>__<name>`, sanitized to the `[A-Za-z0-9_-]` alphabet the model-visible namespace
/// uses, and capped at [`MAX_NAMESPACED_BYTES`].
///
/// Anything outside the alphabet becomes `_`, and runs collapse to one `_`, so a hostile
/// plugin id cannot inject a separator look-alike or a shell fragment into a name the model
/// echoes back. The id is sanitized on both sides of the separator, so a call with an id that
/// already ends in `__` cannot produce an ambiguous `a____b`.
pub fn namespaced(plugin_id: &str, name: &str) -> String {
    let mut id = sanitize_token(plugin_id).unwrap_or_else(|| fallback_token(plugin_id));
    let inner = sanitize_token(name).unwrap_or_else(|| "item".to_owned());
    // Keep at least a four-byte inner name so two long ids cannot collide after trimming.
    let id_cap = MAX_NAMESPACED_BYTES.saturating_sub(NS.len() + 4);
    if id.len() > id_cap {
        id = trim_token(&id, id_cap);
    }
    let budget = MAX_NAMESPACED_BYTES.saturating_sub(id.len() + NS.len());
    let inner = if inner.len() > budget { trim_token(&inner, budget) } else { inner };
    let inner = if inner.is_empty() { "item".to_owned() } else { inner };
    format!("{id}{NS}{inner}")
}

/// Sanitize to `[A-Za-z0-9_-]`, collapsing runs of other bytes to a single `_` and trimming
/// `_` from both ends. `None` when nothing is left.
fn sanitize_token(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len().min(64));
    let mut pending_sep = false;
    for b in raw.bytes() {
        let keep = b.is_ascii_alphanumeric() || b == b'-';
        if keep {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.push(b as char);
        } else {
            pending_sep = true;
        }
    }
    // Truncate on an ASCII boundary; sanitize only ever emits ASCII.
    while out.len() > MAX_NAMESPACED_BYTES {
        out.pop();
    }
    while out.ends_with('_') || out.ends_with('-') {
        out.pop();
    }
    (!out.is_empty()).then_some(out)
}

/// Cut a sanitized token down to `max` bytes, dropping a trailing connector.
fn trim_token(token: &str, max: usize) -> String {
    let mut s = token.get(..max).unwrap_or(token).to_owned();
    while s.ends_with('_') || s.ends_with('-') {
        s.pop();
    }
    s
}

/// A stable id for a plugin whose name sanitizes to nothing, derived from a hash of the raw
/// name so it does not depend on enumeration order.
fn fallback_token(raw: &str) -> String {
    format!("p{}", hash_u64(raw))
}

/// FNV-1a over the raw bytes, as fixed-width lowercase hex.
///
/// Only used to name a plugin that has no usable characters at all; the settings key stays the
/// raw directory name, so nothing about enable state depends on this value.
fn hash_u64(raw: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in raw.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:08x}")
}

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

/// Canonical form of a plugin directory, or `None` when it must not be used.
///
/// `symlink_metadata` is checked before canonicalizing because canonicalizing first would
/// erase the evidence that the directory was a link: `canonicalize` happily resolves a
/// junction into `C:/Windows` and the result would look like an ordinary directory.
///
/// That check only covers the final component, so two more checks follow. The canonical parent
/// must equal the canonical form of `expected_parent`, which catches a junction in any
/// intermediate component (`plugins/link/evil`), and the canonical directory must lie inside
/// `trusted_root`, which catches a caller that reached an unexpected place some other way.
/// Both comparisons are between canonical paths, so a drive-letter case difference cannot make
/// a legitimate plugin look foreign.
fn trusted_dir(p: &Path, expected_parent: &Path, trusted_root: &Path) -> Option<PathBuf> {
    let md = std::fs::symlink_metadata(p).ok()?;
    if md.file_type().is_symlink() {
        tracing::debug!("plugins: {} is a symlink or junction, skipped", p.display());
        return None;
    }
    if !md.is_dir() {
        return None;
    }
    let canonical = dunce::canonicalize(p).ok()?;
    let parent = dunce::canonicalize(expected_parent).ok()?;
    if canonical.parent() != Some(parent.as_path()) {
        tracing::debug!("plugins: {} does not resolve directly inside {}", p.display(), parent.display());
        return None;
    }
    if !canonical.starts_with(trusted_root) {
        tracing::debug!("plugins: {} resolves outside {}", p.display(), trusted_root.display());
        return None;
    }
    canonical.is_dir().then_some(canonical)
}

/// `p` canonicalized, provided it is a regular file that stays inside `root`.
///
/// The returned path is what the caller must read, so the check and the read cannot disagree.
fn contained_file(p: &Path, root: &Path) -> Option<PathBuf> {
    let md = std::fs::symlink_metadata(p).ok()?;
    if md.file_type().is_symlink() || !md.is_file() {
        return None;
    }
    let canonical = dunce::canonicalize(p).ok()?;
    (canonical.starts_with(root) && canonical.is_file()).then_some(canonical)
}

/// `p` canonicalized, provided it is a real directory that stays inside `root`.
fn contained_dir(p: &Path, root: &Path) -> Option<PathBuf> {
    let md = std::fs::symlink_metadata(p).ok()?;
    if md.file_type().is_symlink() || !md.is_dir() {
        return None;
    }
    let canonical = dunce::canonicalize(p).ok()?;
    (canonical.starts_with(root) && canonical.is_dir()).then_some(canonical)
}

/// Validate a path read from `plugins.json` and return it relative to its config directory.
///
/// Rejects anything absolute, home-relative or escaping, including forms that
/// `Path::is_absolute` misses on a non-Windows host when the file was written for Windows:
/// `C:/x`, `\\server\share` and a bare leading separator. Rejects `..` and Windows device
/// names in any component, because `CON` and friends alias devices instead of files, and
/// rejects a `:` in a component because on NTFS that opens an alternate data stream of a file
/// (`notes.md:hidden`) which a text reader never sees.
///
/// A component whose last character is a dot or a space is refused rather than normalized:
/// Windows silently strips both when it creates a name, so `foo.` would resolve to `foo` and
/// the entry would describe a different directory than it names.
fn trusted_relative(raw: &Path) -> Option<PathBuf> {
    if raw.as_os_str().is_empty() || path_absolute(raw) {
        return None;
    }
    let mut out = PathBuf::new();
    for comp in raw.components() {
        match comp {
            Component::Normal(seg) => {
                if is_device_name(seg) || has_windows_tail(seg) || seg.to_string_lossy().contains(':') {
                    return None;
                }
                out.push(seg);
            }
            // `./a` and `a` name the same directory; ignoring `.` keeps them deduplicated.
            Component::CurDir => {}
            // `..` is rejected before any join on the path as entered, so it cannot be
            // normalized away by `canonicalize` later.
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// True when a component ends in a dot or a space, which Windows strips on creation.
fn has_windows_tail(seg: &std::ffi::OsStr) -> bool {
    let bytes = seg.to_string_lossy().into_owned();
    bytes.ends_with('.') || bytes.ends_with(' ')
}

/// True for absolute and home-relative paths on any platform.
///
/// `Path::is_absolute` alone is not enough: on Unix it says `C:/x` is relative, and a
/// `plugins.json` written on Windows then joined onto a Unix base would be harmless there
/// but is still an untrusted entry we refuse to interpret.
fn path_absolute(p: &Path) -> bool {
    if p.is_absolute() {
        return true;
    }
    let text = p.to_string_lossy();
    if text.starts_with('~') || text.starts_with('/') || text.starts_with('\\') {
        return true;
    }
    // `C:` followed by a separator, or drive-relative `C:x`; both escape a base directory.
    let bytes = text.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Windows device names are reserved in every directory and with any extension, so
/// `nul.txt` is still the null device. Compared case-insensitively, as Windows does.
fn is_device_name(seg: &std::ffi::OsStr) -> bool {
    let text = seg.to_string_lossy();
    let stem = text.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    if stem == "CON" || stem == "PRN" || stem == "AUX" || stem == "NUL" || stem == "CLOCK$" {
        return true;
    }
    // COM1-9 and LPT1-9, but not COM0, COM10 or a lookalike such as "console".
    let bytes = stem.as_bytes();
    if bytes.len() == 4 && (bytes[0] == b'C' || bytes[0] == b'L') {
        let word = &stem[..3];
        return (word == "COM" || word == "LPT") && (b'1'..=b'9').contains(&bytes[3]);
    }
    false
}

// ---------------------------------------------------------------------------
// File reading
// ---------------------------------------------------------------------------

/// A directory entry's name, lossily, for sorting.
fn entry_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Read at most `cap` bytes as UTF-8. `None` on IO error, invalid UTF-8, or a file larger than
/// `cap`: reading one byte past the cap is what tells the two cases apart.
fn read_capped(path: &Path, cap: usize) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take((cap as u64) + 1).read_to_end(&mut buf).ok()?;
    if buf.len() > cap {
        tracing::debug!("plugins: {} exceeds {cap} bytes, skipped", path.display());
        return None;
    }
    String::from_utf8(buf).ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context::is_command_name;

    fn choices(pairs: &[(&str, PluginChoice)]) -> BTreeMap<String, PluginChoice> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }
    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    /// A plugin directory with `name` as its manifest name and one skill and one rule, so it
    /// is recognised even when the manifest is the only marker.
    fn make_plugin(dir: &Path, name: &str) {
        write(&dir.join("plugin.json"), &format!(r#"{{"name":"{name}"}}"#));
        write(&dir.join("skills").join("one").join("SKILL.md"), "---\nname: one\ndescription: d\n---\nbody\n");
        write(&dir.join("rules").join("r.md"), "# rule\n");
    }

    #[test]
    fn manifest_fields_are_read_and_unknown_fields_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p");
        write(
            &dir.join("plugin.json"),
            r#"{"name":"alpha","version":"1.2.3","description":"  does things  ",
                "disabled":true,"author":"x","homepage":"https://example.invalid"}"#,
        );
        let m = load_manifest(&dir).expect("manifest");
        assert_eq!(m.name.as_deref(), Some("alpha"));
        assert_eq!(m.version.as_deref(), Some("1.2.3"));
        assert_eq!(m.description.as_deref(), Some("does things"));
        assert!(m.disabled);
    }

    #[test]
    fn plugin_json_beats_dot_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p");
        write(&dir.join("plugin.json"), r#"{"name":"root-name"}"#);
        write(&dir.join(".antigravity-plugin").join("plugin.json"), r#"{"name":"agy-name"}"#);
        write(&dir.join(".gemini-plugin").join("plugin.json"), r#"{"name":"gem-name"}"#);
        assert_eq!(load_manifest(&dir).unwrap().name.as_deref(), Some("root-name"));

        let tmp2 = tempfile::tempdir().unwrap();
        let dir2 = tmp2.path().join("q");
        write(&dir2.join(".antigravity-plugin").join("plugin.json"), r#"{"name":"agy-name"}"#);
        write(&dir2.join(".gemini-plugin").join("plugin.json"), r#"{"name":"gem-name"}"#);
        assert_eq!(load_manifest(&dir2).unwrap().name.as_deref(), Some("agy-name"));
    }

    #[test]
    fn claude_plugin_manifests_are_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p");
        write(&dir.join(".claude-plugin").join("plugin.json"), r#"{"name":"claude"}"#);
        write(&dir.join("gemini-extension.json"), r#"{"name":"gem-ext"}"#);
        assert_eq!(load_manifest(&dir), None);
    }

    #[test]
    fn a_broken_high_precedence_manifest_does_not_fall_through() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p");
        write(&dir.join("plugin.json"), "{not json");
        write(&dir.join(".gemini-plugin").join("plugin.json"), r#"{"name":"later"}"#);
        assert_eq!(load_manifest(&dir), None);
    }

    #[test]
    fn an_oversized_manifest_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p");
        let mut body = String::from(r#"{"name":"big","pad":""#);
        body.push_str(&"x".repeat(MAX_MANIFEST_BYTES));
        body.push_str(r#""}"#);
        write(&dir.join("plugin.json"), &body);
        assert_eq!(load_manifest(&dir), None);
    }

    #[test]
    fn a_user_choice_beats_manifest_disabled() {
        // Manifest disabled, user silent: off.
        assert!(!resolve_state(PluginChoice::Default, true));
        // Manifest disabled, user switched it on: on.
        assert!(resolve_state(PluginChoice::Enabled, true));
        // Manifest enabled, user switched it off: off.
        assert!(!resolve_state(PluginChoice::Disabled, false));
        // Manifest enabled, user silent: on.
        assert!(resolve_state(PluginChoice::Default, false));
        assert!(PluginChoice::Enabled.is_user());
        assert!(PluginChoice::Disabled.is_user());
        assert!(!PluginChoice::Default.is_user());
    }

    #[test]
    fn choices_round_trip_as_kebab_case() {
        let json = serde_json::to_string(&PluginChoice::Disabled).unwrap();
        assert_eq!(json, r#""disabled""#);
        let back: PluginChoice = serde_json::from_str(r#""enabled""#).unwrap();
        assert_eq!(back, PluginChoice::Enabled);
        assert_eq!(PluginChoice::default(), PluginChoice::Default);
    }

    #[test]
    fn discover_applies_source_precedence_and_dedups() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let data = tmp.path().join("data");
        // Same id in both sources: the workspace copy wins and the global one is not listed.
        make_plugin(&ws.join(".agents").join("plugins").join("shadowed"), "from-workspace");
        make_plugin(&data.join("plugins").join("shadowed"), "from-data");
        make_plugin(&data.join("plugins").join("global-only"), "global-only");

        let found = discover(Some(&ws), Some(&data), &BTreeMap::new());
        let ids: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["global-only", "shadowed"]);
        let shadowed = &found[1];
        assert_eq!(shadowed.source, PluginSource::Workspace);
        assert_eq!(shadowed.name, "from-workspace");
        assert!(shadowed.root.starts_with(dunce::canonicalize(&ws).unwrap()));
        assert_eq!(found[0].source, PluginSource::Global);
    }

    #[test]
    fn the_same_canonical_directory_is_only_counted_once() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let root = ws.join(".agents");
        make_plugin(&root.join("plugins").join("both"), "both");
        write(&root.join("plugins.json"), r#"["plugins/both", "plugins/./both"]"#);
        assert_eq!(discover(Some(&ws), None, &BTreeMap::new()).len(), 1);
    }

    #[test]
    fn plugin_choice_from_the_map_reaches_the_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let dir = ws.join(".agents").join("plugins").join("off");
        // The directory name is the key, not the manifest's display name.
        write(&dir.join("plugin.json"), r#"{"name":"display-name"}"#);
        let found = discover(Some(&ws), None, &choices(&[("off", PluginChoice::Disabled)]));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "display-name");
        assert!(!found[0].enabled);
        let default = discover(Some(&ws), None, &BTreeMap::new());
        assert!(default[0].enabled);
    }

    #[test]
    fn namespaced_sanitizes_and_does_not_stack_separators() {
        assert_eq!(namespaced("plug", "build"), "plug__build");
        assert_eq!(namespaced("my-plug", "my-tool"), "my-plug__my-tool");
        // A name that already carries a separator sanitizes into one run of underscores, so
        // the result never contains more than the single inserted separator.
        assert_eq!(namespaced("plug", "plug__build"), "plug__plug_build");
        assert!(!namespaced("plug__", "build").contains("____"));
        assert_eq!(namespaced("plug", "__build__"), "plug__build");
        // Characters outside the model-visible alphabet become one underscore.
        assert_eq!(namespaced("pl ug", "a.b/c"), "pl_ug__a_b_c");
        assert_eq!(namespaced("p:lug", "x y"), "p_lug__x_y");
        // Nothing usable on either side still yields a valid, distinct name.
        assert!(is_command_name(&namespaced("---", "---")));
        assert_ne!(namespaced("!!!", "x"), namespaced("???", "x"));
    }

    #[test]
    fn namespaced_respects_the_length_cap() {
        let long = "a".repeat(200);
        let s = namespaced(&long, &long);
        assert!(s.len() <= MAX_NAMESPACED_BYTES, "{s}");
        assert!(is_command_name(&s));
        // A short id with an oversized inner name is trimmed, not rejected.
        let s = namespaced("plug", &long);
        assert!(s.len() <= MAX_NAMESPACED_BYTES);
        assert!(s.starts_with("plug__"));
    }

    #[test]
    fn results_are_sorted_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        make_plugin(&ws.join(".agents").join("plugins").join("zeta"), "zeta");
        make_plugin(&ws.join(".agents").join("plugins").join("alpha"), "alpha");
        make_plugin(&ws.join(".agents").join("plugins").join("mid"), "mid");
        let ids: Vec<String> = discover(Some(&ws), None, &BTreeMap::new()).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn the_plugin_cap_is_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let plugins = ws.join(".agents").join("plugins");
        for i in 0..70 {
            let name = format!("d{i:02}");
            std::fs::create_dir_all(plugins.join(&name)).unwrap();
            write(&plugins.join(&name).join("plugin.json"), &format!(r#"{{"name":"{name}"}}"#));
        }
        let found = discover(Some(&ws), None, &BTreeMap::new());
        assert_eq!(found.len(), MAX_PLUGINS);
        assert_eq!(found.len(), 64);
        // Sorted by id, so the cap keeps the alphabetically first sixty-four.
        assert_eq!(found.first().map(|p| p.id.as_str()), Some("d00"));
        assert_eq!(found.last().map(|p| p.id.as_str()), Some("d63"));
    }

    #[test]
    fn plugins_json_names_relative_directories_and_rejects_escapes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let root = ws.join(".agents");
        make_plugin(&root.join("from-config"), "from-config");
        write(
            &root.join("plugins.json"),
            r#"["from-config", "C:/Windows/System32", "/etc", "~/evil", "../outside", "CON", "nul.txt"]"#,
        );
        let found = discover(Some(&ws), None, &BTreeMap::new());
        let ids: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["from-config"]);
    }

    #[test]
    fn plugins_json_accepts_the_object_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let root = ws.join(".agents");
        make_plugin(&root.join("a"), "a");
        make_plugin(&root.join("b"), "b");
        write(&root.join("plugins.json"), r#"{"plugins":[{"path":"a"},{"path":"b"}]}"#);
        assert_eq!(discover(Some(&ws), None, &BTreeMap::new()).len(), 2);

        write(&root.join("plugins.json"), r#"{"a":{},"b":{"enabled":true}}"#);
        assert_eq!(discover(Some(&ws), None, &BTreeMap::new()).len(), 2);
    }

    #[test]
    fn plugins_json_dedups_against_the_directory_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let root = ws.join(".agents");
        make_plugin(&root.join("plugins").join("both"), "both");
        write(&root.join("plugins.json"), r#"["plugins/both"]"#);
        assert_eq!(discover(Some(&ws), None, &BTreeMap::new()).len(), 1);
    }

    #[test]
    fn bundled_content_is_collected_with_depth_and_containment() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let dir = ws.join(".agents").join("plugins").join("p");
        write(&dir.join("plugin.json"), r#"{"name":"p","version":"0.1.0","description":"d"}"#);
        write(&dir.join("skills").join("s1").join("SKILL.md"), "---\nname: s1\ndescription: d\n---\n");
        write(&dir.join("skills").join("not-a-skill").join("readme.txt"), "x");
        write(&dir.join("rules").join("one.md"), "# one\n");
        write(&dir.join("rules").join("nested").join("two.md"), "# two\n");
        write(&dir.join("rules").join("a").join("b").join("three.md"), "# three\n");
        write(&dir.join("rules").join("a").join("b").join("c").join("too-deep.md"), "# deep\n");
        write(&dir.join("rules").join("notes.txt"), "ignore me");
        write(&dir.join("hooks.json"), r#"{"sessionStart":[]}"#);
        write(&dir.join("mcp_config.json"), r#"{"mcpServers":{"s":{"command":"nope"}}}"#);

        let found = discover(Some(&ws), None, &BTreeMap::new());
        assert_eq!(found.len(), 1);
        let p = &found[0];
        assert_eq!(p.version.as_deref(), Some("0.1.0"));
        assert_eq!(p.description.as_deref(), Some("d"));
        assert_eq!(p.skill_dirs.len(), 1);
        assert!(p.skill_dirs[0].ends_with("s1"));
        let mut rules: Vec<String> = p.rule_files.iter().map(|f| entry_name(f)).collect();
        rules.sort();
        assert_eq!(rules, vec!["one.md", "three.md", "two.md"]);
        assert!(!rules.iter().any(|r| r == "notes.txt"));
        assert_eq!(p.hook_files.len(), 1);
        assert_eq!(p.mcp_configs.len(), 1);
        assert_eq!(p.mcp_configs[0]["mcpServers"]["s"]["command"], "nope");
    }

    #[test]
    fn a_file_pointing_out_of_the_plugin_root_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let dir = ws.join(".agents").join("plugins").join("p");
        write(&dir.join("plugin.json"), r#"{"name":"p"}"#);
        write(&dir.join("rules").join("ok.md"), "# ok\n");
        let outside = tmp.path().join("secret.md");
        write(&outside, "host secret\n");
        if let Err(e) = make_symlink_file(&outside, &dir.join("rules").join("evil.md")) {
            eprintln!("skipping: cannot create a file symlink here: {e}");
            return;
        }
        let found = discover(Some(&ws), None, &BTreeMap::new());
        let names: Vec<String> = found[0].rule_files.iter().map(|f| entry_name(f)).collect();
        assert_eq!(names, vec!["ok.md"]);

        // The same link as a hooks.json: existence is not enough, containment decides.
        make_symlink_file(&outside, &dir.join("hooks.json")).unwrap();
        let found = discover(Some(&ws), None, &BTreeMap::new());
        assert!(found[0].hook_files.is_empty());
    }

    #[test]
    fn a_directory_link_inside_rules_does_not_contribute_outside_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let dir = ws.join(".agents").join("plugins").join("p");
        write(&dir.join("plugin.json"), r#"{"name":"p"}"#);
        write(&dir.join("rules").join("ok.md"), "# ok\n");
        let outside = tmp.path().join("outside");
        write(&outside.join("evil.md"), "# host secret\n");
        if let Err(e) = make_symlink_dir(&outside, &dir.join("rules").join("linked")) {
            eprintln!("skipping: cannot create a directory link here: {e}");
            return;
        }
        let found = discover(Some(&ws), None, &BTreeMap::new());
        let names: Vec<String> = found[0].rule_files.iter().map(|f| entry_name(f)).collect();
        assert_eq!(names, vec!["ok.md"]);
    }

    #[test]
    fn a_symlinked_plugin_directory_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let real = ws.join(".agents").join("plugins").join("real");
        make_plugin(&real, "real");
        let linked = ws.join(".agents").join("plugins").join("linked");
        if let Err(e) = make_symlink_dir(&real, &linked) {
            eprintln!("skipping: cannot create a directory symlink here: {e}");
            return;
        }
        // The link exists and resolves, so discovering zero of them is the rejection working.
        assert!(std::fs::symlink_metadata(&linked).is_ok_and(|m| m.file_type().is_symlink()));
        assert!(linked.join("plugin.json").is_file());
        let ids: Vec<String> = discover(Some(&ws), None, &BTreeMap::new()).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["real"]);
    }

    #[test]
    fn a_plain_directory_without_plugin_content_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".agents").join("plugins").join("empty")).unwrap();
        assert!(discover(Some(&ws), None, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn device_names_and_escapes_are_rejected_at_the_path_level() {
        assert!(trusted_relative(Path::new("plugins/good")).is_some());
        assert!(trusted_relative(Path::new("./plugins/good")).is_some());
        assert!(trusted_relative(Path::new("")).is_none());
        assert!(trusted_relative(Path::new("C:/x")).is_none());
        assert!(trusted_relative(Path::new("C:x")).is_none());
        assert!(trusted_relative(Path::new("\\\\server\\share")).is_none());
        assert!(trusted_relative(Path::new("/etc")).is_none());
        assert!(trusted_relative(Path::new("~/p")).is_none());
        assert!(trusted_relative(Path::new("../p")).is_none());
        assert!(trusted_relative(Path::new("a/../../p")).is_none());
        assert!(trusted_relative(Path::new("a/CON")).is_none());
        assert!(trusted_relative(Path::new("a/nul.txt")).is_none());
        assert!(trusted_relative(Path::new("a/lpt9")).is_none());
        // Only the exact reserved name is a device; the letters around it do not matter.
        assert!(trusted_relative(Path::new("a/clock$")).is_none());
        assert!(trusted_relative(Path::new("a/CLOCK$.md")).is_none());
        assert!(trusted_relative(Path::new("a/clocks")).is_some());
        assert!(trusted_relative(Path::new("a/PRN.md")).is_none());
        // Only the reserved names are devices; lookalikes are ordinary files.
        assert!(trusted_relative(Path::new("a/console.md")).is_some());
        assert!(trusted_relative(Path::new("a/com0")).is_some());
    }

    #[test]
    fn discovery_without_a_workspace_or_data_dir_is_empty() {
        assert!(discover(None, None, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_junction_in_a_parent_component_cannot_move_a_plugin_outside_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let root = ws.join(".agents");
        // Host content that a cloned repo would like to load as a plugin.
        let outside = tmp.path().join("host");
        make_plugin(&outside.join("evil"), "stolen");
        std::fs::create_dir_all(root.join("plugins")).unwrap();
        if let Err(e) = make_symlink_dir(&outside, &root.join("plugins").join("link")) {
            eprintln!("skipping: cannot create a directory link here: {e}");
            return;
        }
        // The plugin directory itself is an ordinary directory; only its parent is a link, so
        // `symlink_metadata` on the entry cannot see the escape. Containment must catch it.
        let found = discover(Some(&ws), None, &BTreeMap::new());
        assert!(found.iter().all(|p| p.id != "evil"), "escaped via a parent junction");
        // The same escape through plugins.json, which names the path itself.
        write(&root.join("plugins.json"), r#"["plugins/link/evil"]"#);
        let found = discover(Some(&ws), None, &BTreeMap::new());
        assert!(found.iter().all(|p| p.id != "evil"), "escaped via plugins.json");
    }

    #[test]
    fn enable_state_keeps_whether_the_user_chose_it() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let dir = ws.join(".agents").join("plugins").join("p");
        write(&dir.join("plugin.json"), r#"{"name":"p"}"#);

        // Default choice, manifest enables: enabled but not user-enabled, so hooks stay off.
        let found = discover(Some(&ws), None, &BTreeMap::new());
        assert!(found[0].enabled);
        assert_eq!(found[0].choice, PluginChoice::Default);
        assert!(!found[0].is_user_enabled());

        // The user switched it on explicitly.
        let found = discover(Some(&ws), None, &choices(&[("p", PluginChoice::Enabled)]));
        assert!(found[0].is_user_enabled());

        // The user switched it off: neither flag claims it runs.
        let found = discover(Some(&ws), None, &choices(&[("p", PluginChoice::Disabled)]));
        assert!(!found[0].enabled);
        assert_eq!(found[0].choice, PluginChoice::Disabled);
        assert!(!found[0].is_user_enabled());
    }

    #[test]
    fn a_manifest_disabled_plugin_stays_off_until_the_user_says_otherwise() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let dir = ws.join(".agents").join("plugins").join("p");
        write(&dir.join("plugin.json"), r#"{"name":"p","disabled":true}"#);

        let found = discover(Some(&ws), None, &BTreeMap::new());
        assert!(!found[0].enabled);
        assert_eq!(found[0].choice, PluginChoice::Default);
        assert!(!found[0].is_user_enabled());

        let found = discover(Some(&ws), None, &choices(&[("p", PluginChoice::Enabled)]));
        assert!(found[0].enabled);
        assert!(found[0].is_user_enabled());
    }

    #[test]
    fn plugin_mcp_servers_use_the_shared_parser_and_skip_broken_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let dir = ws.join(".agents").join("plugins").join("p");
        write(&dir.join("plugin.json"), r#"{"name":"p"}"#);
        write(
            &dir.join("mcp_config.json"),
            r#"{"mcpServers":{
                "fs":{"command":"node","args":["s.js"]},
                "remote":{"url":"https://example.test/mcp"},
                "both":{"command":"node","url":"https://example.test"},
                "off":{"command":"node","enabled":false}
            }}"#,
        );
        let found = discover(Some(&ws), None, &BTreeMap::new());
        let servers = found[0].mcp_servers();
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        // Document order (serde_json keeps keys sorted), with the unusable entries dropped.
        assert_eq!(names, vec!["fs", "remote"]);
        // A plugin with no config contributes nothing rather than failing.
        let bare = ws.join(".agents").join("plugins").join("bare");
        write(&bare.join("plugin.json"), r#"{"name":"bare"}"#);
        let found = discover(Some(&ws), None, &BTreeMap::new());
        let bare = found.iter().find(|p| p.id == "bare").unwrap();
        assert!(bare.mcp_servers().is_empty());
    }

    #[test]
    fn a_plugin_with_a_malformed_mcp_config_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let dir = ws.join(".agents").join("plugins").join("p");
        write(&dir.join("plugin.json"), r#"{"name":"p"}"#);
        write(&dir.join("mcp_config.json"), "{not json");
        let found = discover(Some(&ws), None, &BTreeMap::new());
        assert_eq!(found.len(), 1);
        assert!(found[0].mcp_configs.is_empty());
        assert!(found[0].mcp_servers().is_empty());
    }

    #[test]
    fn alternate_data_streams_and_trailing_dots_are_rejected() {
        // `notes.md:hidden` opens a stream of a file rather than a directory, and Windows
        // strips a trailing dot on creation, so neither names the directory it appears to.
        assert!(trusted_relative(Path::new("a:stream")).is_none());
        assert!(trusted_relative(Path::new("plugins/p:stream")).is_none());
        assert!(trusted_relative(Path::new("plugins/p.")).is_none());
        assert!(trusted_relative(Path::new("plugins/p ")).is_none());
        assert!(trusted_relative(Path::new("plugins/p.md")).is_some());
        assert!(trusted_relative(Path::new("plugins/.hidden")).is_some());
    }

    #[test]
    fn one_source_cannot_starve_another_of_the_plugin_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let data = tmp.path().join("data");
        // A clone that ships far more than the cap in the workspace.
        let wdir = ws.join(".agents").join("plugins");
        for i in 0..(MAX_PLUGINS + 8) {
            let name = format!("w{i:03}");
            std::fs::create_dir_all(wdir.join(&name)).unwrap();
            write(&wdir.join(&name).join("plugin.json"), &format!(r#"{{"name":"{name}"}}"#));
        }
        // One plugin the user installed for themselves. It sorts first, so it survives the
        // final id-ordered truncation whenever the global source is scanned at all; the
        // failure this guards against is the global scan never running.
        make_plugin(&data.join("plugins").join("aa-user"), "aa-user");

        let found = discover(Some(&ws), Some(&data), &BTreeMap::new());
        assert!(found.iter().any(|p| p.id == "aa-user"), "the global plugin was never scanned");
        assert_eq!(found.len(), MAX_PLUGINS);
        // The workspace source filled its own budget, so exactly one slot is left for global.
        assert_eq!(found.iter().filter(|p| p.source == PluginSource::Workspace).count(), MAX_PLUGINS - 1);
        assert_eq!(found.iter().filter(|p| p.source == PluginSource::Global).count(), 1);
    }

    #[test]
    fn a_symlinked_manifest_is_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let dir = ws.join(".agents").join("plugins").join("p");
        // Skill content, so the plugin is still recognised without a readable manifest.
        write(&dir.join("skills").join("s").join("SKILL.md"), "---\nname: s\ndescription: d\n---\n");
        let host = tmp.path().join("host.json");
        write(&host, r#"{"name":"borrowed"}"#);
        // A junction at `plugin.json` covers the same containment code on a machine without
        // the privilege a file symlink needs, so at least one form runs everywhere.
        let created = make_symlink_file(&host, &dir.join("plugin.json"))
            .or_else(|_| make_symlink_dir(tmp.path(), &dir.join("plugin.json")));
        if let Err(e) = created {
            eprintln!("skipping: cannot create a link here: {e}");
            return;
        }
        let found = discover(Some(&ws), None, &BTreeMap::new());
        assert_eq!(found.len(), 1);
        // The display name falls back to the directory, not to the host content.
        assert_eq!(found[0].name, "p");
    }

    // --- local symlink helpers --------------------------------------------
    //
    // Windows needs Developer Mode or elevation for a file symlink but allows directory
    // junctions for ordinary users, so the directory case is tried both ways. Tests that
    // cannot create the link return early rather than failing: the containment code they
    // exercise is shared with the cases that do run everywhere.

    #[cfg(windows)]
    fn make_symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
        if link.exists() || std::fs::symlink_metadata(link).is_ok() {
            std::fs::remove_file(link)?;
        }
        std::os::windows::fs::symlink_file(target, link)
    }

    #[cfg(unix)]
    fn make_symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
        if std::fs::symlink_metadata(link).is_ok() {
            std::fs::remove_file(link)?;
        }
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn make_symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
        if std::fs::symlink_metadata(link).is_ok() {
            std::fs::remove_dir_all(link)?;
        }
        // A directory symlink needs Developer Mode or elevation; a junction does not, and a
        // junction is the sneaky form a clone actually produces, so fall back to one.
        std::os::windows::fs::symlink_dir(target, link).or_else(|e| {
            tracing::debug!("plugins test: directory symlink failed ({e}), trying a junction");
            make_junction(target, link)
        })
    }

    /// Create an NTFS junction with `cmd`'s built-in `mklink /J`. Junction creation needs no
    /// privilege, which is exactly why a clone can carry one.
    #[cfg(windows)]
    fn make_junction(target: &Path, link: &Path) -> std::io::Result<()> {
        let status = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other("mklink /J failed"))
        }
    }

    #[cfg(unix)]
    fn make_symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
        if std::fs::symlink_metadata(link).is_ok() {
            std::fs::remove_dir_all(link)?;
        }
        std::os::unix::fs::symlink(target, link)
    }
}
