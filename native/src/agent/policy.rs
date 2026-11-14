//! Approval policy (ORCHESTRATION.md section C).
//!
//! Combines the conversation [`AgentMode`], the [`ArtifactReviewPolicy`], the
//! [`TerminalPolicy`] and the user's allow/deny lists into one [`Decision`] per tool call,
//! plus a [`ReviewGate`] for `request_review` and artifact writes.
//!
//! Terminal precedence, highest first:
//! 1. Denylist: a rule whose tokens appear anywhere in the whole line or in any fragment of it
//!    (including text inside `$(..)`, script blocks, after `|`, `;`, `&&`) gives
//!    [`Decision::Deny`]. Nothing overrides it, not even a session "always allow": callers
//!    check `decide` before session approvals.
//! 2. Compound lines (pipes, chaining, redirection, subexpressions, parentheses) ask, except
//!    under [`TerminalPolicy::Turbo`] when the line splits cleanly and every part is allowed.
//! 3. Allowlist (simple commands, or each part of a compound line under Turbo). A path or
//!    script invocation (`.\git.ps1`, `C:\tmp\rg.exe`) never matches: the rule names a command
//!    on `PATH`, not any file with that name.
//! 4. The terminal policy: `Off` asks, `Auto` allows [`SAFE_COMMANDS`], `Turbo` allows.
//!    `Auto` also refuses arguments that run another program or write a file
//!    ([`UNSAFE_ARG_PREFIXES`]).
//!
//! The deny-over-allow ordering and "split compound commands and check each part" follow the
//! design of gemini-cli's policy engine (Apache-2.0, see NOTICE). Code and wording are ours.

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use super::artifacts::ArtifactKind;
use super::tools::{command_fragments, command_parts, is_simple_command, ConfirmationDetails, ToolKind};

/// Conversation mode (public docs: "Planning" / "Fast").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Research, write the task list and implementation plan artifacts, pause for review per
    /// [`ArtifactReviewPolicy`], then execute in task groups and finish with a walkthrough.
    #[default]
    #[serde(alias = "plan", alias = "Planning")]
    Planning,
    /// Act directly; no plan artifacts, review policy does not apply.
    #[serde(alias = "Fast")]
    Fast,
}

/// When the agent stops on a plan or diff (docs: "Request review" / "Always proceed"; the
/// middle value is an AntiCraft addition, see ORCHESTRATION.md naming notes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactReviewPolicy {
    /// Never pause for review.
    #[serde(alias = "always_proceed", alias = "AlwaysProceed")]
    AlwaysProceed,
    /// The agent calls `request_review` when it judges the change needs a look.
    #[serde(alias = "agent_decides", alias = "AgentDecides")]
    AgentDecides,
    /// Always pause after the implementation plan (and before large diffs).
    #[default]
    #[serde(alias = "request_review", alias = "RequestReview")]
    RequestReview,
}

/// Terminal command execution (docs legacy names: "Require Review" / "Always Proceed";
/// AntiCraft has no sandbox on Windows 10, so "Proceed in Sandbox" maps to `Auto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalPolicy {
    /// Always ask, except allowlisted commands.
    #[default]
    #[serde(alias = "Off", alias = "manual", alias = "Manual", alias = "require-review", alias = "request-review", alias = "strict")]
    Off,
    /// Run known read-only / safe simple commands without asking; everything else asks.
    #[serde(alias = "Auto", alias = "proceed-in-sandbox")]
    Auto,
    /// Run everything except denylisted commands (compound lines only when every part is
    /// allowed).
    #[serde(alias = "Turbo", alias = "always-proceed")]
    Turbo,
}

/// The one permission mode the message box offers (DESIGN.md 2.6).
///
/// A view over four settings that stay the source of truth — [`AgentMode`],
/// `Settings::auto_approve_edits`, [`TerminalPolicy`] and `Settings::skip_permissions` — so no
/// new stored field exists and a settings file written before the chip still loads. Each value
/// is one of the combinations a person picks, in the order the chip lists them:
/// Manual, Edit Auto, Plan, Auto, Bypass.
///
/// `Bypass` is the chip's name for `--dangerously-skip-permissions`: deletions, denylisted
/// commands and MCP tools still ask or are refused (those checks sit above the flag in
/// [`ApprovalPolicy::decide`]). It is never written to disk, because `skip_permissions` carries
/// `#[serde(skip)]`; a save stores Auto's fields, so a restart comes back on Auto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Every edit, command and other tool call waits for approval. Reads, searches and allowlisted
    /// commands still run when their settings allow it.
    #[default]
    #[serde(alias = "Manual")]
    Manual,
    /// File edits apply on their own; commands still ask.
    #[serde(alias = "EditAuto")]
    EditAuto,
    /// Planning mode: research, write a plan for review, then build (edits and commands ask).
    #[serde(alias = "Plan")]
    Plan,
    /// Read-only and build commands run on their own and edits apply; everything else asks.
    #[serde(alias = "Auto")]
    Auto,
    /// `--dangerously-skip-permissions`: every tool call runs without asking, except file
    /// deletions, denylisted commands and MCP tools.
    #[serde(alias = "Bypass")]
    Bypass,
}

impl PermissionMode {
    /// Conversation mode this permission mode runs in ([`PermissionMode::Plan`] is the only
    /// Planning one).
    pub fn agent_mode(self) -> AgentMode {
        match self {
            PermissionMode::Plan => AgentMode::Planning,
            PermissionMode::Manual | PermissionMode::EditAuto | PermissionMode::Auto | PermissionMode::Bypass => AgentMode::Fast,
        }
    }

    /// Whether `write_file` / `replace` apply without asking; the diff and hash pinning stay.
    pub fn auto_approve_edits(self) -> bool {
        match self {
            PermissionMode::Manual | PermissionMode::Plan => false,
            PermissionMode::EditAuto | PermissionMode::Auto | PermissionMode::Bypass => true,
        }
    }

    /// How shell commands run: `Manual` and `EditAuto` and `Plan` ask, `Auto` runs the safe set,
    /// `Bypass` runs everything not denylisted.
    pub fn terminal(self) -> TerminalPolicy {
        match self {
            PermissionMode::Manual | PermissionMode::EditAuto | PermissionMode::Plan => TerminalPolicy::Off,
            PermissionMode::Auto => TerminalPolicy::Auto,
            PermissionMode::Bypass => TerminalPolicy::Turbo,
        }
    }

    /// Whether `read_file`, `list_directory`, `glob` and `grep_search` run without asking.
    ///
    /// `Manual` and `Plan` promise that calls wait, so they turn reads off too; the other three
    /// leave the Settings toggle ("Auto-approve read and search tools") to decide. A conversation
    /// that picked no mode is not overridden at all and follows the toggle.
    pub fn auto_approve_read_tools(self) -> bool {
        match self {
            PermissionMode::Manual | PermissionMode::Plan => false,
            PermissionMode::EditAuto | PermissionMode::Auto | PermissionMode::Bypass => true,
        }
    }

    /// Whether every call is auto-approved. Only `Bypass`.
    pub fn skip_permissions(self) -> bool {
        matches!(self, PermissionMode::Bypass)
    }
}

/// Outcome of a policy check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny(String),
}

/// What a review request or artifact write does to the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewGate {
    /// Show the artifact or message and keep going.
    Inform,
    /// Pause until the user approves or requests changes.
    Wait,
}

/// Snapshot of the policy settings, built by `Settings::approval_policy`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalPolicy {
    pub mode: AgentMode,
    pub review: ArtifactReviewPolicy,
    pub terminal: TerminalPolicy,
    pub auto_approve_read_tools: bool,
    /// File edits (`write_file`, `replace`) apply without asking. The diff is still built and
    /// the file hash still pinned, so a file changed meanwhile is not overwritten. Off by default.
    pub auto_approve_edits: bool,
    /// Command prefixes (token-wise) that never ask. `regex:` prefix allowed.
    pub allowlist: Vec<String>,
    /// Command prefixes that are always rejected. Wins over everything.
    pub denylist: Vec<String>,
    /// `--dangerously-skip-permissions`: every tool call is auto-approved, even mutators, except
    /// that the denylist still wins (it is a safety floor, not a convenience). Off by default.
    pub skip_permissions: bool,
}

/// Live policy shared by the backend and a running agent. Read at every tool call, so a
/// settings change applies to the next call without rebuilding the agent (session approvals
/// survive).
pub type SharedPolicy = Arc<RwLock<ApprovalPolicy>>;

/// Command prefixes [`TerminalPolicy::Auto`] runs without asking (rule syntax as
/// [`rule_matches`]): read-only inspection plus build/test verbs that stay in the workspace.
/// Anything that installs, publishes, pushes or deletes is deliberately absent, and so are
/// entry points that run a file the workspace controls (`git -c`, `npm run` of a script named
/// on the line, `go generate`, `cargo +toolchain`), which can execute arbitrary code.
///
/// [`UNSAFE_ARG_PREFIXES`] removes the remaining holes: arguments that run another program
/// (`rg --pre`, `findstr /g`, `git grep --open-files-in-pager`), read files outside the
/// workspace (`type C:\...`), or write to the workspace (`sort x /O:y`).
pub const SAFE_COMMANDS: &[&str] = &[
    "ls", "dir", "pwd", "cat", "type", "echo", "rg", "grep", "findstr", "where", "which",
    "get-childitem", "get-content", "get-location", "get-item", "select-string", "test-path",
    "resolve-path", "measure-object", "gci", "gc", "sls",
    "git status", "git log", "git diff", "git show", "git branch", "git rev-parse", "git ls-files",
    "git blame", "git grep", "git remote -v", "git --version",
    "cargo check", "cargo tree", "cargo metadata", "cargo --version", "rustc --version",
    "npm test", "npm ls", "npm --version",
    "node --version", "python --version", "go version", "go vet",
];

/// `(command, argument prefix)` pairs that make an otherwise safe command unsafe under
/// [`TerminalPolicy::Auto`]: the argument starts another program, writes a file, or reaches
/// outside the workspace. Matched case-insensitively against every argument token of the
/// command (`--pre=cmd` is checked as `--pre`).
pub const UNSAFE_ARG_PREFIXES: &[(&str, &str)] = &[
    // ripgrep: --pre/--hostname-bin run an arbitrary program.
    ("rg", "--pre"), ("rg", "--hostname-bin"),
    // git: these options run a pager, shell, filter or editor, or write a file.
    ("git log", "--format"), ("git log", "--pretty"), ("git log", "--exec-path"),
    ("git log", "--ext-diff"), ("git log", "--textconv"), ("git log", "--open-files-in-pager"),
    ("git diff", "--output"), ("git diff", "--ext-diff"), ("git diff", "--textconv"),
    ("git diff", "--open-files-in-pager"), ("git diff", "--no-index"),
    ("git diff", "--pathspec-from-file"),
    ("git show", "--output"), ("git show", "--ext-diff"), ("git show", "--textconv"),
    ("git show", "--open-files-in-pager"),
    ("git grep", "--open-files-in-pager"),
    ("git grep", "--pathspec-from-file"),
    ("git blame", "--contents"), ("git blame", "--textconv"), ("git blame", "--ignore-revs-file"),
    // findstr /g and /f read one more file (possibly outside the workspace) as input.
    ("findstr", "/g"), ("findstr", "/f"),
    // tree /O and -OutputFile write a file.
    ("tree", "/o"), ("tree", "-o"),
];

/// The unsafe-argument rule that blocks this command (see [`UNSAFE_ARG_PREFIXES`]).
/// Applied per command prefix; options are compared against every argument after the
/// matched command words.
fn unsafe_argument(command: &str) -> Option<&'static str> {
    let toks = tokens(command);
    let unquoted: Vec<String> =
        command.split_whitespace().map(|t| t.trim_matches(['"', '\'']).to_lowercase()).collect();
    for (cmd, arg) in UNSAFE_ARG_PREFIXES {
        let want = tokens(cmd);
        if !command_prefix_matches(&want, &toks) {
            continue;
        }
        for t in unquoted.get(want.len()..).unwrap_or(&[]) {
            let t = t.as_str();
            if t == *arg || t.starts_with(&format!("{arg}=")) || (arg.starts_with('/') && t.starts_with(arg)) {
                return Some(arg);
            }
        }
    }
    // A safe command that passes an absolute path argument reaches outside the workspace.
    if SAFE_COMMANDS.iter().any(|r| rule_matches(r, command)) && has_absolute_path_arg(command) {
        return Some("absolute-path argument");
    }
    None
}

/// True when an argument of `command` looks like an absolute path (`C:\\...`,
/// `\\\\server\\share`, `/usr/...`) or a home reference (`~/...`), i.e. a path that is not
/// inside the workspace.
fn has_absolute_path_arg(command: &str) -> bool {
    let mut words = command.split_whitespace();
    words.next(); // the command itself (checked by the caller)
    for tok in words {
        let t = tok.trim_matches(['"', '\'']);
        if t.starts_with("~/") || t.starts_with("~\\") {
            return true;
        }
        if t.starts_with("\\\\") {
            return true;
        }
        let b = t.as_bytes();
        if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            return true;
        }
        if t.len() > 1 && t.starts_with('/') && !t.starts_with("//") {
            return true;
        }
    }
    false
}

/// Longest accepted rule, in bytes.
pub const MAX_RULE_LEN: usize = 512;
/// Most rules kept per list.
pub const MAX_RULES: usize = 200;
/// Compiled-regex size cap for `regex:` rules.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

impl ApprovalPolicy {
    /// Wrap in a [`SharedPolicy`].
    pub fn shared(self) -> SharedPolicy {
        Arc::new(RwLock::new(self))
    }

    /// A copy of this policy adjusted for a workflow child.
    ///
    /// Children inherit the parent's allow/deny lists and skip-permissions setting, but their
    /// mode is forced to [`AgentMode::Fast`] so they never receive the planning tools — that
    /// prevents a child from recursively calling `run_workflow`, and stops the Planning-mode
    /// prompt sections from being appended (the child prompt already tells it to answer and
    /// call `finish`). Read tools are always auto-approved because a child cannot prompt; edits
    /// inherit the parent's `auto_approve_edits`, so a parent that opted into auto-editing lets
    /// its children edit, and one that did not keeps edits cancelled. Anything that would still
    /// ask is cancelled by the workflow drain and fails the child, which is the contract.
    pub fn for_child(&self) -> ApprovalPolicy {
        ApprovalPolicy {
            mode: AgentMode::Fast,
            review: self.review,
            terminal: self.terminal,
            auto_approve_read_tools: true,
            auto_approve_edits: self.auto_approve_edits,
            allowlist: self.allowlist.clone(),
            denylist: self.denylist.clone(),
            skip_permissions: self.skip_permissions,
        }
    }

    /// Decide for one call. `details` is `None` when no confirmation preview was built.
    pub fn decide(&self, kind: ToolKind, details: Option<&ConfirmationDetails>) -> Decision {
        // An MCP server is third-party code in another process, outside the workspace and
        // outside everything the tool layer guards. The blanket skip-permissions flag is a
        // convenience for tools we bound ourselves, so it must not extend to MCP: those calls
        // always ask. This check comes first because the block below returns Allow outright.
        if matches!(details, Some(ConfirmationDetails::Mcp { .. })) {
            return Decision::Ask;
        }
        // Erasing a file is irreversible, so it asks under every policy — including
        // --dangerously-skip-permissions and auto-apply edits. This check comes before the
        // blanket flag for the same reason as the MCP one above.
        if matches!(details, Some(ConfirmationDetails::Delete { .. })) {
            return Decision::Ask;
        }
        // --dangerously-skip-permissions: auto-approve everything except denylist commands
        // (denied) and shell lines that delete files (asked, like the Delete preview above:
        // the user approves every deletion, whatever the policy).
        if self.skip_permissions {
            if let Some(ConfirmationDetails::Shell { command, .. }) = details {
                if let Some(rule) = self.deny_hit(command) {
                    return Decision::Deny(format!("blocked by denylist rule \"{rule}\""));
                }
                if crate::agent::tools::deletes_files(command) {
                    return Decision::Ask;
                }
            }
            return Decision::Allow;
        }
        match (kind, details) {
            (_, Some(ConfirmationDetails::Shell { command, .. })) => self.decide_command(command),
            // A shell call without a preview never runs unseen.
            (ToolKind::Execute, _) => Decision::Ask,
            // Edits keep the diff preview (hash pinning); they skip the prompt only when the
            // user turned on auto-apply *and* the target is not a file that steers the agent
            // itself (rules, settings). See [`is_protected_path`].
            (ToolKind::Edit, Some(ConfirmationDetails::Edit { resolved, .. }))
                if self.auto_approve_edits && !is_protected_path(resolved) =>
            {
                Decision::Allow
            }
            (ToolKind::Edit, _) => Decision::Ask,
            (ToolKind::Read | ToolKind::Search, _) if self.auto_approve_read_tools => Decision::Allow,
            (ToolKind::Read | ToolKind::Search, _) => Decision::Ask,
        }
    }

    /// Terminal rule, see module docs.
    pub fn decide_command(&self, command: &str) -> Decision {
        if let Some(rule) = self.deny_hit(command) {
            return Decision::Deny(format!("blocked by denylist rule \"{rule}\""));
        }
        // Deleting a file is irreversible: it asks under every terminal policy, including
        // Turbo and any allowlist rule that would otherwise match the verb (`rm` on the list
        // does not make `rm notes.txt` run by itself).
        if crate::agent::tools::deletes_files(command) {
            return Decision::Ask;
        }
        if !is_simple_command(command) {
            return match (self.terminal, command_parts(command)) {
                (TerminalPolicy::Turbo, Some(parts))
                    if !parts.is_empty() && parts.iter().all(|p| self.decide_simple(p) == Decision::Allow) =>
                {
                    Decision::Allow
                }
                _ => Decision::Ask,
            };
        }
        self.decide_simple(command)
    }

    /// Allowlist, then the terminal policy, for one command (denylist already checked).
    /// A command invoked via a path (`./foo.ps1`, `C:\x\rg.exe`) is never auto-run: the
    /// allowlist rule names a PATH command, not an arbitrary file.
    fn decide_simple(&self, command: &str) -> Decision {
        if is_path_invocation(command) {
            return Decision::Ask;
        }
        if self.allowlist.iter().any(|r| rule_matches(r, command)) {
            return Decision::Allow;
        }
        match self.terminal {
            TerminalPolicy::Off => Decision::Ask,
            TerminalPolicy::Auto
                if is_simple_command(command)
                    && SAFE_COMMANDS.iter().any(|r| rule_matches(r, command))
                    && unsafe_argument(command).is_none() =>
            {
                Decision::Allow
            }
            TerminalPolicy::Auto => Decision::Ask,
            TerminalPolicy::Turbo => Decision::Allow,
        }
    }

    /// First denylist rule whose tokens appear in the whole line or any fragment.  Also
    /// checks whether the rule tokens appear as a contiguous sub-sequence inside a
    /// fragment, so a denied verb cannot be smuggled past by inserting a path, option, or
    /// extra separator between its tokens.
    fn deny_hit(&self, command: &str) -> Option<&str> {
        let frags = command_fragments(command);
        self.denylist
            .iter()
            .find(|r| {
                rule_matches(r, command)
                    || frags.iter().any(|f| rule_matches(r, f) || contains_tokens(r, f))
            })
            .map(|r| r.trim())
    }

    /// The artifact review policy only applies in Planning mode (per the docs).
    pub fn review_applies(&self) -> bool {
        self.mode == AgentMode::Planning
    }

    /// Gate for `request_review`. A plain question (no artifacts) waits for the answer unless
    /// the agent marked it non-blocking. Artifact reviews follow the policy: `RequestReview`
    /// always waits, `AgentDecides` waits unless `blocking == Some(false)`, `AlwaysProceed` and
    /// Fast mode never wait.
    pub fn review_gate(&self, has_artifacts: bool, blocking: Option<bool>) -> ReviewGate {
        if !has_artifacts {
            return if blocking == Some(false) { ReviewGate::Inform } else { ReviewGate::Wait };
        }
        if !self.review_applies() {
            return ReviewGate::Inform;
        }
        match self.review {
            ArtifactReviewPolicy::RequestReview => ReviewGate::Wait,
            ArtifactReviewPolicy::AgentDecides if blocking == Some(false) => ReviewGate::Inform,
            ArtifactReviewPolicy::AgentDecides => ReviewGate::Wait,
            ArtifactReviewPolicy::AlwaysProceed => ReviewGate::Inform,
        }
    }

    /// Writing an artifact of `kind` pauses for review even if the agent does not call
    /// `request_review` (Planning + `RequestReview` + a reviewable kind).
    pub fn pause_after_write(&self, kind: ArtifactKind) -> bool {
        self.review_applies() && self.review == ArtifactReviewPolicy::RequestReview && kind.is_reviewable()
    }

    /// Edits and shell commands wait while a written plan is still unapproved (Planning +
    /// `RequestReview`). The caller answers such calls with an error telling the model to
    /// request review first.
    pub fn edits_blocked_by_review(&self, plan_pending: bool) -> bool {
        plan_pending && self.review_applies() && self.review == ArtifactReviewPolicy::RequestReview
    }
}

/// Check a user-entered rule; `Err` carries a short reason for the list editor.
pub fn validate_rule(rule: &str) -> Result<(), String> {
    let r = rule.trim();
    if r.is_empty() {
        return Err("rule is empty".into());
    }
    if r.len() > MAX_RULE_LEN {
        return Err(format!("rule is longer than {MAX_RULE_LEN} bytes"));
    }
    if let Some(p) = r.strip_prefix("regex:") {
        if p.trim().is_empty() {
            return Err("regex is empty".into());
        }
        compile(p).map_err(|e| format!("invalid regex: {e}"))?;
    }
    Ok(())
}

fn compile(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(&format!("^(?:{pattern})$"))
        .case_insensitive(true)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
}

/// Normalize one token: strip quotes and lowercase. The first token additionally drops a
/// call operator (`&`) and an executable extension (`git.exe` -> `git`). It does NOT strip
/// directory components: a path-qualified command (`.\\git.ps1`, `C:\tools\rg.exe`) must never
/// compare equal to the bare command name, or an allowlist rule for `rg` would also approve
/// any file called `rg.exe` on disk. Such invocations are detected by [`is_path_invocation`]
/// and always ask.
fn norm_token(tok: &str, first: bool) -> String {
    let t = tok.trim_matches(['"', '\'']).to_lowercase();
    if !first {
        return t;
    }
    let t = t.trim_start_matches('&');
    strip_exe_ext(t).to_owned()
}

fn tokens(s: &str) -> Vec<String> {
    s.split_whitespace().enumerate().map(|(i, t)| norm_token(t, i == 0)).filter(|t| !t.is_empty()).collect()
}

/// Raw (quote-trimmed, lowercased) tokens; used for contiguous-subsequence deny checks.
fn raw_tokens(s: &str) -> Vec<String> {
    s.split_whitespace().map(|t| t.trim_matches(['"', '\'']).to_lowercase()).filter(|t| !t.is_empty()).collect()
}

/// True when the normalized tokens `want` (a command prefix) lead `have`. The last word of
/// `want` may be the start of the following token (`git diff` matches `git diff-tree` only
/// through an explicit rule, never here: prefixes stop at whole normalized tokens, except
/// that an option written as `--opt=value` matches `--opt`).
fn command_prefix_matches(want: &[String], have: &[String]) -> bool {
    if want.is_empty() || have.len() < want.len() {
        return false;
    }
    let n = want.len();
    for (i, (w, h)) in want.iter().zip(have).enumerate() {
        if i + 1 == n {
            if h != w && !h.starts_with(&format!("{w}=")) {
                return false;
            }
        } else if h != w {
            return false;
        }
    }
    true
}

/// Strip an executable extension from a token (`git.exe` -> `git`).
fn strip_exe_ext(t: &str) -> &str {
    [".exe", ".cmd", ".bat", ".com", ".ps1"].iter().find_map(|e| t.strip_suffix(e)).unwrap_or(t)
}

/// First token of a command, lowercased, quote- and `&`-trimmed (no extension removal).
fn head(s: &str) -> &str {
    s.split_whitespace().next().unwrap_or("").trim_matches(['"', '\'']).trim_start_matches('&')
}

/// True when the command runs a file by path rather than a command resolved from `PATH`:
/// `.\\git.ps1`, `..\x\rg.exe`, `C:\Windows\System32\curl.exe`, `/bin/sh`, `foo.ps1`, `a.bat`.
/// Such commands bypass allowlist and `SAFE_COMMANDS` matching; they always ask.
pub fn is_path_invocation(command: &str) -> bool {
    let first = head(command);
    if first.is_empty() {
        return false;
    }
    let b = first.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return true;
    }
    if first.contains('\\') || first.contains('/') {
        return true;
    }
    let lower = first.to_ascii_lowercase();
    [".ps1", ".bat", ".cmd", ".vbs", ".js"].iter().any(|e| lower.ends_with(e))
}

/// True when `rule`'s tokens appear as contiguous tokens anywhere inside `command`
/// (`C:\bin\rm -rf build` contains `rm -rf`). This keeps the denylist closed against inserted
/// arguments and separators. `regex:` rules are never segment-matched.
fn contains_tokens(rule: &str, command: &str) -> bool {
    if rule.trim().starts_with("regex:") {
        return false;
    }
    let want = raw_tokens(rule);
    if want.is_empty() {
        return false;
    }
    let have = raw_tokens(command);
    if have.windows(want.len()).any(|w| w == want.as_slice()) {
        return true;
    }
    // A path-qualified verb still counts: `C:\tools\git.exe push --force` and
    // `& 'Remove-Item' -Recurse` must be caught by the same rules.
    let base: Vec<String> = have
        .iter()
        .map(|t| {
            let b = t.rsplit(['\\', '/']).next().unwrap_or(t);
            strip_exe_ext(b).to_owned()
        })
        .collect();
    base.windows(want.len()).any(|w| w.iter().zip(&want).all(|(a, b)| a == b))
}

/// True when the file at `path` steers the agent itself: rules files, settings, and anything
/// under `.agents/`. Auto-applied edits never touch these; they always ask even when
/// `auto_approve_edits` is on. A model-planted rule otherwise becomes a standing instruction
/// that outlives the prompt that installed it.
pub fn is_protected_path(path: &std::path::Path) -> bool {
    let lower = path.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or("");
    if matches!(name, "agents.md" | "gemini.md" | "anticraft.md" | "settings.json") {
        return true;
    }
    lower.split('/').any(|c| c == ".agents" || c == ".gemini" || c == ".anticraft")
}

/// Token-prefix match (`git push` matches `git push origin` and `git.exe push`), or
/// `regex:<pattern>` anchored against the whole trimmed command. Case-insensitive. Invalid
/// regex rules never match (`validate_rule` rejects them, `Settings::sanitized` drops them).
///
/// The first tokens compare with an executable extension stripped from either side
/// (`git` matches `git.exe`), but a path-qualified first token never matches a bare command
/// name: `.\git.exe status` is a different executable from `git status`, and the caller
/// therefore never matches a rule written for the PATH command.
pub fn rule_matches(rule: &str, command: &str) -> bool {
    let rule = rule.trim();
    if let Some(p) = rule.strip_prefix("regex:") {
        return compile(p).is_ok_and(|r| r.is_match(command.trim()));
    }
    let want = tokens(rule);
    let have = tokens(command);
    if want.is_empty() || have.len() < want.len() {
        return false;
    }
    if is_path_invocation(command) {
        return false;
    }
    want.iter().zip(&have).enumerate().all(|(i, (a, b))| {
        if i == 0 {
            strip_exe_ext(a) == strip_exe_ext(b)
        } else {
            a == b
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(terminal: TerminalPolicy, allow: &[&str], deny: &[&str]) -> ApprovalPolicy {
        ApprovalPolicy {
            terminal,
            allowlist: allow.iter().map(|s| s.to_string()).collect(),
            denylist: deny.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn shell(cmd: &str) -> ConfirmationDetails {
        ConfirmationDetails::Shell {
            command: cmd.into(),
            dir: ".".into(),
            root_command: super::super::tools::root_command(cmd),
            description: None,
            simple: is_simple_command(cmd),
        }
    }

    #[test]
    fn deny_beats_allow() {
        let p = pol(TerminalPolicy::Turbo, &["rm"], &["rm -rf"]);
        assert!(matches!(p.decide_command("rm -rf /"), Decision::Deny(_)));
        // A deletion asks even when the allowlist names the verb: erasing a file always
        // reaches the user, so neither Turbo nor an allowlist rule can run it unseen.
        assert_eq!(p.decide_command("rm a.txt"), Decision::Ask);
        assert!(matches!(p.decide_command("ls | rm -rf x"), Decision::Deny(_)));
    }

    /// Every spelling of "delete" is gated, under every policy.
    /// --dangerously-skip-permissions approves everything else, but a deletion still asks.
    #[test]
    fn skip_permissions_still_asks_for_deletions() {
        let p = ApprovalPolicy { skip_permissions: true, terminal: TerminalPolicy::Turbo, ..Default::default() };
        for c in ["rm notes.txt", "Remove-Item -Recurse build", "cargo build && del a.txt", "git clean -fd"] {
            assert_eq!(p.decide(ToolKind::Execute, Some(&shell(c))), Decision::Ask, "{c}");
        }
        assert_eq!(p.decide(ToolKind::Execute, Some(&shell("cargo build"))), Decision::Allow);
        assert_eq!(p.decide(ToolKind::Execute, Some(&shell("echo \"rm -rf /\""))), Decision::Allow, "quoted text deletes nothing");
        let erase = ConfirmationDetails::Delete { path: "a.txt".into(), old_hash: None };
        assert_eq!(p.decide(ToolKind::Edit, Some(&erase)), Decision::Ask);
    }

    #[test]
    fn deletions_always_ask() {
        for terminal in [TerminalPolicy::Off, TerminalPolicy::Auto, TerminalPolicy::Turbo] {
            let p = pol(terminal, &["rm", "del", "Remove-Item"], &[]);
            for c in [
                "rm notes.txt",
                "del notes.txt",
                "erase notes.txt",
                "Remove-Item notes.txt",
                "Remove-Item -Recurse -Force build",
                "ri build",
                "unlink x",
                "git rm tracked.rs",
                "git clean -fd",
                "cargo build && rm -rf target",
                "ls; rd /s /q dir",
            ] {
                assert_eq!(p.decide_command(c), Decision::Ask, "{terminal:?}: {c}");
            }
            // Non-deleting lines still follow the policy.
            assert_eq!(p.decide_command("cargo build"), if terminal == TerminalPolicy::Turbo { Decision::Allow } else { Decision::Ask });
            // A deletion gets no session key, so "Always allow" cannot cover it later.
            assert_eq!(crate::agent::tools::root_command("rm notes.txt"), "");
            assert_eq!(crate::agent::tools::root_command("git rm x"), "");
            assert!(crate::agent::tools::deletes_files("python -c \"import os;os.remove('x')\"").eq(&false), "code in a quoted argument is not a delete verb");
        }
        // Deleting via a quoted argument is not a shell deletion (the model would have to call
        // a program that deletes, which the shell policy does not model).
        assert!(crate::agent::tools::deletes_files("rm -f a.txt"));
        assert!(!crate::agent::tools::deletes_files("echo rm"));
        assert!(!crate::agent::tools::deletes_files("rg 'rm -rf'"));
    }

    #[test]
    fn denylist_sees_every_fragment() {
        let p = pol(TerminalPolicy::Turbo, &["git"], &["rm -rf", "format", "git push --force"]);
        for c in [
            "git status; rm -rf ~",
            "git log && RM -RF build",
            "git $(rm -rf /)",
            "echo hi | & format c:",
            "git log `\nrm -rf x",
            "\"C:\\tools\\git.exe\" push --force origin",
            "{ rm -rf . }",
        ] {
            assert!(matches!(p.decide_command(c), Decision::Deny(_)), "{c:?}");
        }
        // Token prefix, not substring.
        assert_eq!(p.decide_command("Get-Process | Format-Table"), Decision::Allow);
        assert_eq!(p.decide_command("git push origin"), Decision::Allow);
    }

    #[test]
    fn compound_only_runs_in_turbo_when_every_part_allowed() {
        let off = pol(TerminalPolicy::Off, &["cargo", "git"], &[]);
        assert_eq!(off.decide_command("cargo fmt && cargo test"), Decision::Ask);
        assert_eq!(off.decide_command("cargo test"), Decision::Allow);

        let auto = pol(TerminalPolicy::Auto, &[], &[]);
        assert_eq!(auto.decide_command("git status | more"), Decision::Ask);

        let turbo = pol(TerminalPolicy::Turbo, &[], &["rm -rf"]);
        assert_eq!(turbo.decide_command("cargo fmt && cargo test | findstr ok"), Decision::Allow);
        // A separator inside quotes does not split the line, so the chain it really contains is
        // what gets judged — one part — and a quoted one-command line auto-runs.
        assert_eq!(turbo.decide_command("python -c \"print('x')\""), Decision::Allow);
        assert_eq!(
            turbo.decide_command("python -V; python -c \"import requests,sys;print('requests ok')\""),
            Decision::Allow
        );
        // A denied verb inside quotes is still denied: the denylist scans every fragment,
        // quoted or not.
        assert!(matches!(turbo.decide_command("echo \"rm -rf /\""), Decision::Deny(_)));
        // Opaque constructs never auto-run.
        assert_eq!(turbo.decide_command("git log $(whoami)"), Decision::Ask);
        assert_eq!(turbo.decide_command("git log \"$(whoami)\""), Decision::Ask);
        assert_eq!(turbo.decide_command("Invoke-Command { ls }"), Decision::Ask);
        assert_eq!(turbo.decide_command("a ;;; b"), Decision::Ask);
    }

    #[test]
    fn auto_runs_only_safe_simple_commands() {
        let p = pol(TerminalPolicy::Auto, &[], &[]);
        for c in ["git status --short", "Get-ChildItem src", "cargo check -q", "rg foo"] {
            assert_eq!(p.decide_command(c), Decision::Allow, "{c}");
        }
        for c in ["git push", "cargo install ripgrep", "npm install", "python -c \"import os\"", "Remove-Item x", "echo x > f.txt"] {
            assert_eq!(p.decide_command(c), Decision::Ask, "{c}");
        }
        assert_eq!(pol(TerminalPolicy::Off, &[], &[]).decide_command("git status"), Decision::Ask);
    }

    #[test]
    fn decide_by_tool_kind() {
        let mut p = pol(TerminalPolicy::Turbo, &[], &["format"]);
        p.auto_approve_read_tools = true;
        let read = ConfirmationDetails::Read { summary: "x".into() };
        assert_eq!(p.decide(ToolKind::Read, Some(&read)), Decision::Allow);
        assert_eq!(p.decide(ToolKind::Search, None), Decision::Allow);
        assert_eq!(p.decide(ToolKind::Edit, None), Decision::Ask);
        let auto = ApprovalPolicy { auto_approve_edits: true, ..p.clone() };
        assert_eq!(auto.decide(ToolKind::Edit, None), Decision::Ask, "no preview: never unseen");
        assert_eq!(p.decide(ToolKind::Execute, None), Decision::Ask);
        assert_eq!(p.decide(ToolKind::Execute, Some(&shell("ls"))), Decision::Allow);
        assert!(matches!(p.decide(ToolKind::Execute, Some(&shell("format c:"))), Decision::Deny(_)));
        p.auto_approve_read_tools = false;
        assert_eq!(p.decide(ToolKind::Read, Some(&read)), Decision::Ask);
    }

    #[test]
    fn mcp_tools_always_ask_even_with_skip_permissions() {
        // An MCP server runs third-party code outside the workspace; the blanket
        // skip-permissions flag deliberately does not reach it.
        let p = ApprovalPolicy { skip_permissions: true, terminal: TerminalPolicy::Turbo, ..Default::default() };
        let mcp = ConfirmationDetails::Mcp {
            server: "files".into(),
            tool: "read_secret".into(),
            summary: "read_secret(path)".into(),
            read_only: true,
        };
        assert_eq!(p.decide(ToolKind::Read, Some(&mcp)), Decision::Ask);
        assert_eq!(p.decide(ToolKind::Edit, Some(&mcp)), Decision::Ask);
        // The flag still covers everything else, including shell commands.
        assert_eq!(p.decide(ToolKind::Execute, Some(&shell("ls"))), Decision::Allow);
    }

    #[test]
    fn a_child_policy_still_asks_for_mcp() {
        // The child inherits skip_permissions, but the MCP arm sits above it, so a child that
        // somehow reached the prompt would still be asked. With no one to answer, the agent
        // refuses the call instead of prompting (see the agent tests).
        for skip in [false, true] {
            let parent = ApprovalPolicy { skip_permissions: skip, ..Default::default() };
            let child = parent.for_child();
            let mcp = ConfirmationDetails::Mcp {
                server: "files".into(),
                tool: "read_secret".into(),
                summary: "read_secret()".into(),
                read_only: true,
            };
            assert_eq!(child.decide(ToolKind::Execute, Some(&mcp)), Decision::Ask, "skip={skip}");
        }
    }

    #[test]
    fn review_gate_follows_mode_and_policy() {
        let mut p = ApprovalPolicy::default();
        assert_eq!(p.mode, AgentMode::Planning);
        assert_eq!(p.review_gate(true, Some(false)), ReviewGate::Wait);
        assert!(p.pause_after_write(ArtifactKind::ImplementationPlan));
        assert!(!p.pause_after_write(ArtifactKind::TaskList));
        assert!(p.edits_blocked_by_review(true));
        assert!(!p.edits_blocked_by_review(false));

        p.review = ArtifactReviewPolicy::AgentDecides;
        assert_eq!(p.review_gate(true, None), ReviewGate::Wait);
        assert_eq!(p.review_gate(true, Some(false)), ReviewGate::Inform);
        assert!(!p.pause_after_write(ArtifactKind::ImplementationPlan));
        assert!(!p.edits_blocked_by_review(true));

        p.review = ArtifactReviewPolicy::AlwaysProceed;
        assert_eq!(p.review_gate(true, Some(true)), ReviewGate::Inform);
        // Questions to the user still wait.
        assert_eq!(p.review_gate(false, None), ReviewGate::Wait);
        assert_eq!(p.review_gate(false, Some(false)), ReviewGate::Inform);

        p.review = ArtifactReviewPolicy::RequestReview;
        p.mode = AgentMode::Fast;
        assert_eq!(p.review_gate(true, Some(true)), ReviewGate::Inform);
        assert!(!p.pause_after_write(ArtifactKind::ImplementationPlan));
        assert!(!p.edits_blocked_by_review(true));
    }

    #[test]
    fn prefix_and_regex_rules() {
        assert!(rule_matches("git push", "git push origin main"));
        assert!(rule_matches("git push", "GIT.EXE  Push origin"));
        assert!(!rule_matches("git push", "git pull"));
        assert!(!rule_matches("git push", "git"));
        assert!(!rule_matches("", "git"));
        assert!(rule_matches("regex:npm run (build|test)", "npm run test"));
        assert!(rule_matches("regex:npm run (build|test)", "  NPM run build "));
        assert!(!rule_matches("regex:npm run (build|test)", "npm run test2"));
        assert!(!rule_matches("regex:(", "("));
    }

    #[test]
    fn validate_rules() {
        assert!(validate_rule("git push").is_ok());
        assert!(validate_rule("regex:cargo (test|check)").is_ok());
        assert!(validate_rule("  ").is_err());
        assert!(validate_rule("regex:").is_err());
        assert!(validate_rule("regex:(").is_err());
        assert!(validate_rule(&"a".repeat(MAX_RULE_LEN + 1)).is_err());
    }

    #[test]
    fn legacy_enum_names_deserialize() {
        let t: TerminalPolicy = serde_json::from_str("\"always-proceed\"").unwrap();
        assert_eq!(t, TerminalPolicy::Turbo);
        let t: TerminalPolicy = serde_json::from_str("\"proceed-in-sandbox\"").unwrap();
        assert_eq!(t, TerminalPolicy::Auto);
        let r: ArtifactReviewPolicy = serde_json::from_str("\"request_review\"").unwrap();
        assert_eq!(r, ArtifactReviewPolicy::RequestReview);
        assert_eq!(serde_json::to_string(&ArtifactReviewPolicy::AgentDecides).unwrap(), "\"agent-decides\"");
        let m: AgentMode = serde_json::from_str("\"plan\"").unwrap();
        assert_eq!(m, AgentMode::Planning);
    }

    #[test]
    fn shared_handle_updates_live() {
        let h = ApprovalPolicy::default().shared();
        assert_eq!(h.read().unwrap().decide_command("ls"), Decision::Ask);
        h.write().unwrap().terminal = TerminalPolicy::Auto;
        assert_eq!(h.read().unwrap().decide_command("ls"), Decision::Allow);
    }

    // --- security regressions ---------------------------------------------------------

    #[test]
    fn path_invocation_never_matches_a_bare_command_rule() {
        // Prior behaviour stripped the directory and the extension from the first token, so
        // any file named rg.exe / ls.exe matched the allowlist and SAFE_COMMANDS.
        let p = pol(TerminalPolicy::Turbo, &["git", "ls", "rg"], &[]);
        for c in [
            ".\\git.exe push --force",
            "C:\\Users\\me\\evil\\ls.exe -Recurse",
            "..\\..\\tools\\rg.exe --pre evil",
            "./git.sh status",
            "/usr/bin/git push",
            "payload.ps1 -Command x",
            "run.bat",
        ] {
            assert!(!rule_matches("git", c), "{c}: path invocation matched a bare rule");
            assert!(matches!(p.decide_command(c), Decision::Ask | Decision::Deny(_)), "{c}");
        }
        // Extension on the command itself (no path) still matches its rule.
        assert!(rule_matches("git", "git.exe push"));
        assert_eq!(p.decide_command("rg.exe foo"), Decision::Allow, "PATH command with extension still allowed");
        // A legitimately PATH-resolved command without directory separators is unaffected.
        assert!(rule_matches("rg", "rg --files"));
    }

    #[test]
    fn auto_refuses_dangerous_arguments_on_safe_commands() {
        let p = pol(TerminalPolicy::Auto, &[], &[]);
        for c in [
            "rg --pre 'rm' foo",
            "rg --pre=evil.exe pattern",
            "rg --hostname-bin cmd",
            "git log --format='%x09' --exec-path=evil",
            "git diff --output=C:\\Windows\\x",
            "git diff --no-index C:\\a C:\\b",
            "git grep --open-files-in-pager=cmd pattern",
            "findstr /g:C:\\Windows\\a.txt needle",
            "tree /F C:\\ /O:out.txt",
            "Get-Content C:\\Windows\\System32\\config\\SAM",
            "type C:\\boot.ini",
            "rg C:\\ secret",
        ] {
            assert_eq!(p.decide_command(c), Decision::Ask, "{c}");
        }
        // The same commands without the dangerous argument still auto-run.
        assert_eq!(p.decide_command("rg --files foo"), Decision::Allow);
        assert_eq!(p.decide_command("git log -n 5"), Decision::Allow);
        assert_eq!(p.decide_command("Get-Content src/main.rs"), Decision::Allow);
    }

    #[test]
    fn auto_rejects_entry_points_that_run_workspace_code() {
        let p = pol(TerminalPolicy::Auto, &[], &[]);
        for c in [
            "npm run deploy",         // script defined by package.json
            "npm run build:evil",
            "git -c core.pager=evil log",  // config injection before the subcommand
            "cargo +nightly build",        // alternate toolchain
            "cargo fmt -- --check --emit=foo",
            "go generate ./...",
        ] {
            assert_eq!(p.decide_command(c), Decision::Ask, "{c}");
        }
        // `git -c core.pager=cat log` is gone entirely: git alone is not a safe root, so
        // `git -c ...` must ask.
        assert_eq!(p.decide_command("git -c core.pager=cat log"), Decision::Ask);
        // `npm run test` (a fixed npm command) still auto-runs even if its args change.
        assert_eq!(p.decide_command("npm test -- --watch=false"), Decision::Allow);
        // A path-qualified git is a different binary and never auto-runs.
        assert_eq!(p.decide_command("C:\\\\Git\\\\bin\\\\git.exe diff"), Decision::Ask);
    }

    #[test]
    fn denylist_portmatches_through_path_and_extra_tokens() {
        let p = pol(TerminalPolicy::Turbo, &[], &["rm -rf", "remove-item", "format"]);
        for c in [
            "C:\\Windows\\System32\\rm -rf build",        // path-qualified rm
            "Remove-Item -Recurse -Force (Join-Path $home .config\\agents)", // separate: parens
            "& 'Remove-Item' -Recurse build",
            "C:\\bin\\Remove-Item -Recurse foo",
            "format c:",                                   // baseline
            "X:\\tools\\format.com c:",
        ] {
            assert!(matches!(p.decide_command(c), Decision::Deny(_)), "{c:?}");
        }
        // `format` as a verb inside a *different* word still does not trip the rule; but a
        // contiguous `format c:` inside `Get-Process | Format-Table c:` must.
        assert_eq!(p.decide_command("Get-Process"), Decision::Allow);
        assert!(matches!(p.decide_command("echo hi; Get-Item x | format c:"), Decision::Deny(_)));
        // But a single-token rule `format` catches the word anywhere in a segment.
        assert!(matches!(p.decide_command("Get-Item x | Format c:"), Decision::Deny(_)));
    }

    #[test]
    fn eacaped_denylist_word_in_middle_is_caught() {
        // Rule `remove-item`; user typed `echo hi | & remove-item ...` already covered.
        // Now a longer fragment that starts with a different token:
        let p = pol(TerminalPolicy::Turbo, &[], &["remove-item -recurse"]);
        for c in [
            "echo hi | get-childitem x | remove-item -recurse",  // compound
            "ls ; gc \\\\server\\share | remove-item -recurse",     // compound with unc path
            "gi . | % { remove-item -recurse }",                  // script block
        ] {
            assert!(matches!(p.decide_command(c), Decision::Deny(_)), "{c:?}");
        }
    }

    #[test]
    fn session_approvals_do_not_cover_parentheses_arguments() {
        // `ls (Remove-Item ...)` is not a simple command, so it never gets an
        // always-allow and always asks (or is denied by the inner fragment).
        let p = pol(TerminalPolicy::Auto, &[], &["remove-item"]);
        assert!(!is_simple_command("ls (Remove-Item -Recurse C:\\x)"));
        assert!(matches!(p.decide_command("ls (Remove-Item -Recurse C:\\x)"), Decision::Deny(_)));
        // A benign parenthesised line is not auto-approved either.
        assert_eq!(p.decide_command("ls (Get-ChildItem)"), Decision::Ask);
    }

    #[test]
    fn auto_approve_edits_protects_rules_and_settings() {
        let mut p = pol(TerminalPolicy::Turbo, &[], &[]);
        p.auto_approve_edits = true;
        let edit = |resolved: &str| ConfirmationDetails::Edit {
            path: "x".into(),
            diff: String::new(),
            is_new_file: false,
            resolved: std::path::PathBuf::from(resolved),
            old_hash: None,
        };
        for path in [
            "C:\\ws\\.agents\\rules\\new-rule.md",
            "C:\\ws\\AGENTS.md",
            "C:\\ws\\.anticraft\\settings.json",
            "C:\\ws\\GEMINI.md",
            "C:\\ws\\.agents\\workflows\\deploy.md",
            "C:\\ws\\sub\\ANTICRAFT.md",
        ] {
            assert_eq!(p.decide(ToolKind::Edit, Some(&edit(path))), Decision::Ask, "{path}");
        }
        // Ordinary source files still auto-apply when the user turned that on.
        assert_eq!(p.decide(ToolKind::Edit, Some(&edit("C:\\ws\\src\\main.rs"))), Decision::Allow);
    }
}
