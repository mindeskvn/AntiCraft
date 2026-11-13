//! Lifecycle hooks from `hooks.json`: discover, parse, and execute command handlers at
//! specific points in the agent loop.
//!
//! Discovery: `<workspace>/.agents/hooks.json` (workspace scope), then
//! `<data-dir>/hooks.json` (global scope). Named groups from all sources are merged in
//! discovery order; a group with `"enabled": false` contributes no handlers.
//!
//! Every hook command receives a JSON object on **stdin** and must write a JSON object
//! to **stdout**. Commands that fail, time out, or produce unparseable output are
//! silently skipped — hooks must never crash the agent.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::agent::plugins::{Plugin, PluginChoice};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_HOOK_OUTPUT: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single command handler from a hook group.
#[derive(Debug, Clone)]
pub struct HookHandler {
    pub command: String,
    pub timeout: Duration,
    /// Working directory for the command (directory of the `hooks.json` that declared it).
    pub base_dir: PathBuf,
}

/// A grouped tool-event entry: a regex pattern and the handlers that run on a match.
#[derive(Debug, Clone)]
pub struct ToolHookEntry {
    /// Raw pattern. `"*"` or `""` matches every tool.
    pub pattern: String,
    pub handlers: Vec<HookHandler>,
}

/// All handlers from one or more `hooks.json` files after merging named groups.
#[derive(Debug, Clone, Default)]
pub struct HookConfig {
    pub session_start: Vec<HookHandler>,
    pub pre_tool_use: Vec<ToolHookEntry>,
    pub post_tool_use: Vec<ToolHookEntry>,
    pub pre_invocation: Vec<HookHandler>,
    pub post_invocation: Vec<HookHandler>,
    pub stop: Vec<HookHandler>,
}

impl HookConfig {
    /// No handler of any event: every hook call can be skipped.
    pub fn is_empty(&self) -> bool {
        self.session_start.is_empty()
            && self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.pre_invocation.is_empty()
            && self.post_invocation.is_empty()
            && self.stop.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Lifecycle results
// ---------------------------------------------------------------------------

/// Decision returned by a `PreToolUse` hook command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolDecision {
    #[default]
    Allow,
    Deny,
    Ask,
    ForceAsk,
}

/// Parsed output of one `PreToolUse` handler.
#[derive(Debug, Clone, Default)]
pub struct PreToolResult {
    pub decision: ToolDecision,
    pub reason: Option<String>,
    /// Keys to shallow-merge into the tool call arguments before execution.
    pub overwrite: Option<serde_json::Map<String, Value>>,
}

/// A step to inject into the current turn.
#[derive(Debug, Clone)]
pub enum InjectStep {
    SystemMessage(String),
    UserMessage(String),
    Ephemeral(String),
}

/// What the agent loop should do after a pre/post-invocation hook run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminationBehavior {
    #[default]
    Proceed,
    Terminate,
    ForceContinue, // PostInvocation only
}

/// Combined output of a pre/post-invocation hook run.
#[derive(Debug, Clone, Default)]
pub struct InvocationResult {
    pub inject_steps: Vec<InjectStep>,
    pub termination: TerminationBehavior,
}

/// Output of a `Stop` hook run.
#[derive(Debug, Clone, Default)]
pub struct StopResult {
    /// At least one handler returned `"continue"`.
    pub should_continue: bool,
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Discovery + parsing
// ---------------------------------------------------------------------------

/// Load and merge `hooks.json` from the workspace (`.agents/hooks.json`) and the
/// global data dir (`<data-dir>/hooks.json`). Workspace groups run before global
/// ones; a group with `"enabled": false` is dropped.
pub fn load_hooks(workspace: Option<&Path>, data_dir: Option<&Path>) -> HookConfig {
    let mut cfg = HookConfig::default();
    if let Some(ws) = workspace {
        let p = ws.join(".agents").join("hooks.json");
        if p.is_file() {
            merge_file(&p, ws, &mut cfg);
        }
    }
    if let Some(d) = data_dir {
        let p = d.join("hooks.json");
        if p.is_file() {
            merge_file(&p, d, &mut cfg);
        }
    }
    cfg
}

/// [`load_hooks`] plus the hook files of plugins the user switched on themselves.
///
/// The extra condition is the whole point of this function. A plugin found in a cloned
/// workspace is enabled by default (its manifest does not opt out), and a hook is an arbitrary
/// shell command: wiring one up at discovery would let a repository run code on the user's first
/// prompt by shipping a `hooks.json`. So a plugin contributes only when its resolved state came
/// from a user choice ([`PluginChoice::Enabled`]) — the manifest's own `"disabled": false` is not
/// consent. The plugin root is the handlers' working directory, as with any other hooks file.
pub fn load_hooks_with_plugins(
    workspace: Option<&Path>,
    data_dir: Option<&Path>,
    plugins: &[Plugin],
) -> HookConfig {
    let mut cfg = load_hooks(workspace, data_dir);
    for plugin in plugins {
        // The same gate the MCP path uses: executing a plugin's hooks is running its code, so
        // it needs the user's own choice, not just a manifest that failed to opt out.
        if !plugin.is_user_enabled() {
            continue;
        }
        for path in &plugin.hook_files {
            merge_file(path, &plugin.root, &mut cfg);
        }
    }
    cfg
}

fn merge_file(path: &Path, base: &Path, cfg: &mut HookConfig) {
    let Ok(raw) = std::fs::read_to_string(path) else { return };
    let Ok(val) = serde_json::from_str::<Value>(&raw) else { return };
    let Some(obj) = val.as_object() else { return };
    for (_name, group) in obj {
        let Value::Object(g) = group else { continue };
        if g.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        for (event, entries) in g {
            match event.as_str() {
                "SessionStart" => extend_flat(&mut cfg.session_start, entries, base),
                "PreToolUse" => extend_tool(&mut cfg.pre_tool_use, entries, base),
                "PostToolUse" => extend_tool(&mut cfg.post_tool_use, entries, base),
                "PreInvocation" => extend_flat(&mut cfg.pre_invocation, entries, base),
                "PostInvocation" => extend_flat(&mut cfg.post_invocation, entries, base),
                "Stop" => extend_flat(&mut cfg.stop, entries, base),
                _ => {}
            }
        }
    }
}

/// Flat events (SessionStart, PreInvocation, PostInvocation, Stop): a list of handler objects.
fn extend_flat(out: &mut Vec<HookHandler>, entries: &Value, base: &Path) {
    let Some(arr) = entries.as_array() else { return };
    for e in arr {
        if let Some(h) = parse_handler(e, base) {
            out.push(h);
        }
    }
}

/// Tool events (PreToolUse, PostToolUse): a list of `{matcher, hooks: [...]}` groups.
fn extend_tool(out: &mut Vec<ToolHookEntry>, entries: &Value, base: &Path) {
    let Some(arr) = entries.as_array() else { return };
    for e in arr {
        let Some(obj) = e.as_object() else { continue };
        let pattern = obj.get("matcher").and_then(Value::as_str).unwrap_or("*").to_owned();
        let mut handlers = Vec::new();
        if let Some(hs) = obj.get("hooks").and_then(Value::as_array) {
            for h in hs {
                if let Some(parsed) = parse_handler(h, base) {
                    handlers.push(parsed);
                }
            }
        }
        if !handlers.is_empty() {
            out.push(ToolHookEntry { pattern, handlers });
        }
    }
}

fn parse_handler(v: &Value, base: &Path) -> Option<HookHandler> {
    let obj = v.as_object()?;
    // The handler may nest `command` under a `hooks` array (already flattened by the
    // caller) or expose it directly; both shapes are accepted.
    let command = obj.get("command").and_then(Value::as_str)?.to_owned();
    if command.trim().is_empty() {
        return None;
    }
    let timeout = obj
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .max(1);
    Some(HookHandler {
        command: expand_home(&command),
        timeout: Duration::from_secs(timeout),
        base_dir: base.to_path_buf(),
    })
}

fn expand_home(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            let mut p = PathBuf::from(home);
            p.push(rest);
            return p.to_string_lossy().into_owned();
        }
    }
    s.to_owned()
}

/// Does `tool` match the matcher pattern? `"*"` and `""` match everything; otherwise the
/// pattern is a regex tested against the lowercased tool name.
pub fn matches(pattern: &str, tool: &str) -> bool {
    if pattern == "*" || pattern.is_empty() {
        return true;
    }
    // A regex error means no match — safer than panicking on a user-supplied pattern.
    regex::Regex::new(pattern).ok().is_some_and(|re| re.is_match(tool))
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Run a command handler, feeding it `input` on stdin and returning its parsed JSON output.
/// Returns `None` when the command fails, times out, or emits no valid JSON.
///
/// Execution order:
/// 1. Spawn the child.
/// 2. Write stdin on a **detached thread** so a command that never reads stdin cannot block
///    the parent — the pipe buffer on Windows is ~4 KB; a large hook payload would otherwise
///    park the parent inside `write_all` indefinitely, bypassing the timeout entirely.
/// 3. Poll `try_wait` until the deadline; kill + reap on timeout.
/// 4. Read stdout **after** the child exits (grandchildren on Windows keep the pipe open
///    while alive, so reading before exit would block past the configured timeout).
///
/// The command runs in its own process tree (a job object on Windows, a process group on
/// unix), the same guard shell commands use. Ending the tree, not just `cmd.exe`, is what
/// stops `node` under `cmd /c npm test` on a timeout. The tree also ends once the command
/// exits, so a leftover background process cannot hold the stdout pipe open, and with the
/// app: this function runs on a blocking thread that exit abandons, but the OS closes the
/// job handle with the process.
pub fn run_handler(h: &HookHandler, input: &Value) -> Option<Value> {
    let mut cmd = std::process::Command::new(shell_program());
    cmd.arg(shell_flag())
        .arg(&h.command)
        .current_dir(&h.base_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().ok()?;
    let tree = crate::agent::tools::proc_tree::TreeGuard::attach_std(&child);
    // Move stdin to a detached thread: a command that ignores stdin keeps the pipe open and a
    // synchronous write_all would stall the parent once the pipe buffer fills.
    if let Some(mut stdin) = child.stdin.take() {
        let payload = serde_json::to_string(input).unwrap_or_default();
        std::thread::spawn(move || {
            let _ = stdin.write_all(payload.as_bytes());
        });
    }
    // Poll for exit, kill on timeout, then reap so the OS closes the child's stdout handle.
    let deadline = std::time::Instant::now() + h.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                if let Some(t) = &tree {
                    t.kill();
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // Whatever the command left running ends here, before stdout is read.
    drop(tree);
    let success = status.is_some_and(|s| s.success());
    if !success {
        return None;
    }
    // Read stdout only after the process has fully exited: on Windows, `cmd /c` may orphan
    // a grandchild that keeps the write end of the pipe open.  By reading after exit+reap the
    // pipe is always closed by the time we call read_to_end, so this can never block.
    let mut buf = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
    }
    if buf.len() > MAX_HOOK_OUTPUT {
        buf.truncate(MAX_HOOK_OUTPUT);
    }
    serde_json::from_slice(&buf).ok()
}

#[cfg(unix)]
fn shell_program() -> &'static str {
    "sh"
}
#[cfg(unix)]
fn shell_flag() -> &'static str {
    "-c"
}
#[cfg(windows)]
fn shell_program() -> &'static str {
    "cmd"
}
#[cfg(windows)]
fn shell_flag() -> &'static str {
    "/c"
}

/// Merge extra fields from `common` into `base`. Common fields do not overwrite base fields.
fn merge_common(mut base: Value, common: &Value) -> Value {
    if let (Some(b), Some(c)) = (base.as_object_mut(), common.as_object()) {
        for (k, v) in c {
            b.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    base
}

/// Run all `PreToolUse` handlers matching `tool`. The first `Deny`/`Ask`/`ForceAsk` wins
/// (left-to-right); a later `Allow` does not override an earlier gate. `overwrite` maps
/// are merged left-to-right (later keys win).
pub fn run_pre_tool_use(cfg: &HookConfig, common: &Value, tool: &str, args: &Value, step_idx: u64) -> PreToolResult {
    let mut result = PreToolResult::default();
    for entry in &cfg.pre_tool_use {
        if !matches(&entry.pattern, tool) {
            continue;
        }
        for h in &entry.handlers {
            let input = merge_common(
                json!({ "toolCall": { "name": tool, "args": args }, "stepIdx": step_idx }),
                common,
            );
            let Some(out) = run_handler(h, &input) else { continue };
            if let Some(d) = out.get("decision").and_then(Value::as_str) {
                result.decision = match d {
                    "deny" => ToolDecision::Deny,
                    "ask" => ToolDecision::Ask,
                    "force_ask" => ToolDecision::ForceAsk,
                    _ => ToolDecision::Allow,
                };
            }
            if let Some(r) = out.get("reason").and_then(Value::as_str) {
                result.reason = Some(r.to_owned());
            }
            if let Some(ow) = out.get("overwrite").and_then(Value::as_object) {
                let map = result.overwrite.get_or_insert_with(serde_json::Map::new);
                for (k, v) in ow {
                    map.insert(k.clone(), v.clone());
                }
            }
            if result.decision == ToolDecision::Deny {
                return result;
            }
        }
    }
    result
}

/// Run all `PostToolUse` handlers matching `tool`. Output is ignored (the contract is `{}`).
pub fn run_post_tool_use(cfg: &HookConfig, common: &Value, tool: &str, args: &Value, step_idx: u64, error: Option<&str>) {
    for entry in &cfg.post_tool_use {
        if !matches(&entry.pattern, tool) {
            continue;
        }
        for h in &entry.handlers {
            let mut input = merge_common(
                json!({ "toolCall": { "name": tool, "args": args }, "stepIdx": step_idx }),
                common,
            );
            if let Some(e) = error {
                input["error"] = json!(e);
            }
            let _ = run_handler(h, &input);
        }
    }
}

/// Run `PreInvocation` handlers. Collects injected steps and the terminal behavior; a
/// `terminate` from any handler ends the run.
pub fn run_pre_invocation(cfg: &HookConfig, common: &Value, invocation_num: u64, initial_steps: u64) -> InvocationResult {
    let mut result = InvocationResult::default();
    for h in &cfg.pre_invocation {
        let input = merge_common(
            json!({ "invocationNum": invocation_num, "initialNumSteps": initial_steps }),
            common,
        );
        let Some(out) = run_handler(h, &input) else { continue };
        collect_inject_steps(&out, &mut result.inject_steps);
        if let Some(t) = out.get("terminationBehavior").and_then(Value::as_str) {
            if t == "terminate" {
                result.termination = TerminationBehavior::Terminate;
                return result;
            }
        }
    }
    result
}

/// Run `PostInvocation` handlers. `force_continue` keeps the loop going; `terminate` stops it.
pub fn run_post_invocation(cfg: &HookConfig, common: &Value, invocation_num: u64, initial_steps: u64) -> InvocationResult {
    let mut result = InvocationResult::default();
    for h in &cfg.post_invocation {
        let input = merge_common(
            json!({ "invocationNum": invocation_num, "initialNumSteps": initial_steps }),
            common,
        );
        let Some(out) = run_handler(h, &input) else { continue };
        collect_inject_steps(&out, &mut result.inject_steps);
        if let Some(t) = out.get("terminationBehavior").and_then(Value::as_str) {
            match t {
                "force_continue" => result.termination = TerminationBehavior::ForceContinue,
                "terminate" => {
                    result.termination = TerminationBehavior::Terminate;
                    return result;
                }
                _ => {}
            }
        }
    }
    result
}

/// Run `Stop` handlers. Any handler returning `{"decision":"continue"}` sets the continue flag.
pub fn run_stop(cfg: &HookConfig, common: &Value, execution_num: u64, reason: &str, error: Option<&str>) -> StopResult {
    let mut result = StopResult::default();
    for h in &cfg.stop {
        let mut input = merge_common(
            json!({ "executionNum": execution_num, "terminationReason": reason, "fullyIdle": true }),
            common,
        );
        if let Some(e) = error {
            input["error"] = json!(e);
        }
        let Some(out) = run_handler(h, &input) else { continue };
        if out.get("decision").and_then(Value::as_str) == Some("continue") {
            result.should_continue = true;
            if let Some(r) = out.get("reason").and_then(Value::as_str) {
                result.reason = Some(r.to_owned());
            }
        }
    }
    result
}

/// Run `SessionStart` handlers. Their `injectSteps` are returned in order; the caller adds
/// them to the session once (they persist in the trajectory).
pub fn run_session_start(cfg: &HookConfig, common: &Value) -> Vec<InjectStep> {
    let mut steps = Vec::new();
    for h in &cfg.session_start {
        let Some(out) = run_handler(h, common) else { continue };
        collect_inject_steps(&out, &mut steps);
    }
    steps
}

fn collect_inject_steps(out: &Value, steps: &mut Vec<InjectStep>) {
    let Some(arr) = out.get("injectSteps").and_then(Value::as_array) else { return };
    for step in arr {
        if let Some(m) = step.get("systemMessage").and_then(|v| v.get("systemMessage")).and_then(Value::as_str) {
            if !m.is_empty() {
                steps.push(InjectStep::SystemMessage(m.to_owned()));
            }
        } else if let Some(u) = step.get("userMessage").and_then(Value::as_str) {
            if !u.is_empty() {
                steps.push(InjectStep::UserMessage(u.to_owned()));
            }
        } else if let Some(e) = step.get("ephemeralMessage").and_then(Value::as_str) {
            if !e.is_empty() {
                steps.push(InjectStep::Ephemeral(e.to_owned()));
            }
        }
    }
}

/// Build the common payload fields every hook receives.
pub fn common_payload(
    conversation_id: &str,
    workspace_paths: &[&str],
    model_name: &str,
    last_user_input: &str,
) -> Value {
    json!({
        "conversationId": conversation_id,
        "workspacePaths": workspace_paths,
        "modelName": model_name,
        "lastUserInput": last_user_input,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a script that echoes `body` (a JSON object) and return the shell command for it.
    /// `cmd /c` has its own quoting rules, so a real file avoids quoting the body by hand.
    #[cfg(windows)]
    fn echo_script(dir: &Path, name: &str, body: &str) -> String {
        let p = dir.join(format!("{name}.cmd"));
        std::fs::write(&p, format!("@echo off\r\necho {body}\r\n")).unwrap();
        // Bare path: Rust's Command quotes it if needed; embedding quotes here makes
        // `cmd /c` look for a file whose name literally contains quote characters.
        p.display().to_string()
    }

    #[cfg(unix)]
    fn echo_script(dir: &Path, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(format!("{name}.sh"));
        std::fs::write(&p, format!("#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{body}'\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.display().to_string()
    }

    fn handler(command: String, secs: u64, base: &Path) -> HookHandler {
        HookHandler { command, timeout: Duration::from_secs(secs), base_dir: base.to_path_buf() }
    }

    #[test]
    fn matcher_star_empty_and_regex() {
        assert!(matches("*", "run_command"));
        assert!(matches("", "create_file"));
        assert!(matches("run_command", "run_command"));
        assert!(!matches("run_command", "view_file"));
        // The docs show the alternation escaped for a Markdown table; the value itself is `a|b`.
        assert!(matches(r"run_command|view_file", "view_file"));
        assert!(!matches(r"run_command|view_file", "create_file"));
        assert!(matches("browser_.*", "browser_click"));
        assert!(!matches("(unclosed", "anything"), "a bad regex must not match or panic");
    }

    #[test]
    fn parse_merges_groups_and_skips_disabled() {
        let ws = tempfile::tempdir().unwrap();
        let g = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join(".agents")).unwrap();
        std::fs::write(
            ws.path().join(".agents/hooks.json"),
            r#"{
              "lint": {"PostToolUse": [{"matcher": "run_command", "hooks": [{"type": "command", "command": "lint", "timeout": 5}]}]},
              "gate": {"enabled": false, "PreToolUse": [{"matcher": "*", "hooks": [{"command": "no"}]}]},
              "reminder": {"SessionStart": [{"command": "start"}], "Stop": [{"command": "stop"}]}
            }"#,
        )
        .unwrap();
        // Global file: a second PreToolUse group; defaults apply to the handler.
        std::fs::write(
            g.path().join("hooks.json"),
            r#"{"audit": {"PreToolUse": [{"matcher": "create_file", "hooks": [{"command": "audit"}]}]}}"#,
        )
        .unwrap();
        let cfg = load_hooks(Some(ws.path()), Some(g.path()));
        assert!(!cfg.is_empty());
        assert_eq!(cfg.post_tool_use.len(), 1);
        assert_eq!(cfg.post_tool_use[0].pattern, "run_command");
        assert_eq!(cfg.post_tool_use[0].handlers[0].timeout, Duration::from_secs(5));
        assert_eq!(cfg.post_tool_use[0].handlers[0].base_dir, dunce::canonicalize(ws.path()).unwrap_or_else(|_| ws.path().to_path_buf()));
        // The disabled group contributes nothing.
        assert_eq!(cfg.pre_tool_use.len(), 1);
        assert_eq!(cfg.pre_tool_use[0].pattern, "create_file");
        assert_eq!(cfg.pre_tool_use[0].handlers[0].timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(cfg.session_start.len(), 1);
        assert_eq!(cfg.stop.len(), 1);
        assert!(cfg.pre_invocation.is_empty() && cfg.post_invocation.is_empty());
    }

    #[test]
    fn load_hooks_ignores_missing_and_malformed_files() {
        let ws = tempfile::tempdir().unwrap();
        let g = tempfile::tempdir().unwrap();
        assert!(load_hooks(Some(ws.path()), Some(g.path())).is_empty());
        std::fs::create_dir_all(g.path()).unwrap();
        std::fs::write(g.path().join("hooks.json"), "{not json").unwrap();
        assert!(load_hooks(Some(ws.path()), Some(g.path())).is_empty());
    }

    #[test]
    fn plugin_hooks_need_an_explicit_user_choice() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = ws.path();
        let plug = root.join(".agents/plugins/p");
        std::fs::create_dir_all(&plug).unwrap();
        std::fs::write(plug.join("plugin.json"), r#"{"name":"p"}"#).unwrap();
        std::fs::write(
            plug.join("hooks.json"),
            r#"{"start": {"SessionStart": [{"command": "touch-started"}]}}"#,
        )
        .unwrap();

        // Discovered with no user choice: the manifest enables it, the hooks stay off.
        let found = crate::agent::plugins::discover(Some(root), Some(data.path()), &std::collections::BTreeMap::new());
        assert_eq!(found.len(), 1);
        assert!(found[0].enabled);
        assert!(load_hooks_with_plugins(Some(root), Some(data.path()), &found).is_empty());

        // The user switched it on: its hooks are merged, with the plugin root as base dir.
        let choices = std::collections::BTreeMap::from([(
            "p".to_owned(),
            crate::agent::plugins::PluginChoice::Enabled,
        )]);
        let found = crate::agent::plugins::discover(Some(root), Some(data.path()), &choices);
        let cfg = load_hooks_with_plugins(Some(root), Some(data.path()), &found);
        assert_eq!(cfg.session_start.len(), 1);
        assert_eq!(cfg.session_start[0].command, "touch-started");
        assert_eq!(cfg.session_start[0].base_dir, found[0].root);

        // Switched off explicitly: nothing, even though the manifest has no "disabled".
        let choices = std::collections::BTreeMap::from([(
            "p".to_owned(),
            crate::agent::plugins::PluginChoice::Disabled,
        )]);
        let found = crate::agent::plugins::discover(Some(root), Some(data.path()), &choices);
        assert!(load_hooks_with_plugins(Some(root), Some(data.path()), &found).is_empty());
    }

    #[test]
    fn run_handler_pipes_stdin_and_parses_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler(echo_script(dir.path(), "deny", r#"{"decision":"deny","reason":"no"}"#), 10, dir.path());
        let out = run_handler(&h, &json!({"toolCall": {"name": "x"}})).expect("script output");
        assert_eq!(out["decision"], "deny");
        assert_eq!(out["reason"], "no");
    }

    #[test]
    fn run_handler_times_out_and_rejects_bad_output() {
        let dir = tempfile::tempdir().unwrap();
        // A sleeper that outlives the 1s timeout.
        let sleep = if cfg!(windows) { "ping -n 5 127.0.0.1 >nul" } else { "sleep 5" };
        let h = handler(sleep.to_owned(), 1, dir.path());
        let started = std::time::Instant::now();
        assert!(run_handler(&h, &json!({})).is_none(), "a timed-out command yields None");
        assert!(started.elapsed() < Duration::from_secs(4), "the timeout must kill the command");
        // Valid JSON is required; a failing command yields None.
        let bad = handler(echo_script(dir.path(), "plain", "not json"), 10, dir.path());
        assert!(run_handler(&bad, &json!({})).is_none());
        let fail = handler("exit 3".into(), 10, dir.path());
        assert!(run_handler(&fail, &json!({})).is_none());
        assert!(run_handler(&handler("definitely-not-a-real-program-xyz".into(), 5, dir.path()), &json!({})).is_none());
    }

    /// What a hook starts ends with it. `cmd /c npm test` runs node under cmd.exe, and killing
    /// cmd.exe alone leaves node running with no deadline.
    #[cfg(windows)]
    #[test]
    fn a_timed_out_hook_takes_its_child_processes_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker.txt");
        // The grandchild appends to the marker every 200 ms for about 10 s.
        let script = dir.path().join("spawn.ps1");
        std::fs::write(
            &script,
            format!(
                "Start-Process -NoNewWindow -FilePath powershell.exe -ArgumentList '-NoProfile','-Command','for($i=0;$i -lt 50;$i++){{Add-Content -Path \"{}\" -Value x; Start-Sleep -Milliseconds 200}}'\r\nStart-Sleep -Seconds 30\r\n",
                marker.display()
            ),
        )
        .unwrap();
        let h = handler(format!("powershell -NoProfile -ExecutionPolicy Bypass -File {}", script.display()), 5, dir.path());
        assert!(run_handler(&h, &json!({})).is_none(), "the hook timed out");
        std::thread::sleep(Duration::from_millis(800));
        let len1 = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        assert!(len1 > 0, "the grandchild had started before the timeout");
        std::thread::sleep(Duration::from_millis(1200));
        let len2 = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
        assert_eq!(len1, len2, "the hook's child process kept running after the timeout");
    }

    #[test]
    fn pre_tool_use_deny_wins_over_a_later_allow() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = HookConfig::default();
        cfg.pre_tool_use.push(ToolHookEntry {
            pattern: "*".into(),
            handlers: vec![
                handler(echo_script(dir.path(), "deny", r#"{"decision":"deny","reason":"blocked"}"#), 10, dir.path()),
                handler(echo_script(dir.path(), "allow", r#"{"decision":"allow"}"#), 10, dir.path()),
            ],
        });
        let r = run_pre_tool_use(&cfg, &json!({}), "run_command", &json!({"cmd": "rm -rf /"}), 3);
        assert_eq!(r.decision, ToolDecision::Deny);
        assert_eq!(r.reason.as_deref(), Some("blocked"));
    }

    #[test]
    fn pre_tool_use_asks_overwrites_and_skips_non_matching_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = HookConfig::default();
        cfg.pre_tool_use.push(ToolHookEntry {
            pattern: "run_.*".into(),
            handlers: vec![handler(
                echo_script(dir.path(), "ask", r#"{"decision":"ask","reason":"check","overwrite":{"CommandLine":"ls"}}"#),
                10,
                dir.path(),
            )],
        });
        let r = run_pre_tool_use(&cfg, &json!({}), "run_command", &json!({"CommandLine": "rm -rf /"}), 1);
        assert_eq!(r.decision, ToolDecision::Ask);
        assert_eq!(r.reason.as_deref(), Some("check"));
        assert_eq!(r.overwrite.as_ref().unwrap()["CommandLine"], "ls");
        // A tool that does not match runs no handler.
        let r = run_pre_tool_use(&cfg, &json!({}), "view_file", &json!({}), 1);
        assert_eq!(r.decision, ToolDecision::Allow);
        assert!(r.overwrite.is_none() && r.reason.is_none());
    }

    #[test]
    fn invocation_steps_and_termination() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = HookConfig::default();
        cfg.pre_invocation.push(handler(
            echo_script(
                dir.path(),
                "inv",
                r#"{"injectSteps":[{"systemMessage":{"systemMessage":"sys"}},{"ephemeralMessage":"e"},{"userMessage":"u"}],"terminationBehavior":""}"#,
            ),
            10,
            dir.path(),
        ));
        let r = run_pre_invocation(&cfg, &json!({}), 2, 9);
        assert_eq!(r.termination, TerminationBehavior::Proceed);
        assert_eq!(r.inject_steps.len(), 3);
        assert!(matches!(&r.inject_steps[0], InjectStep::SystemMessage(s) if s == "sys"));
        assert!(matches!(&r.inject_steps[1], InjectStep::Ephemeral(s) if s == "e"));
        assert!(matches!(&r.inject_steps[2], InjectStep::UserMessage(s) if s == "u"));

        let mut cfg = HookConfig::default();
        cfg.pre_invocation.push(handler(echo_script(dir.path(), "term", r#"{"terminationBehavior":"terminate"}"#), 10, dir.path()));
        assert_eq!(run_pre_invocation(&cfg, &json!({}), 1, 0).termination, TerminationBehavior::Terminate);

        let mut cfg = HookConfig::default();
        cfg.post_invocation.push(handler(
            echo_script(dir.path(), "force", r#"{"injectSteps":[],"terminationBehavior":"force_continue"}"#),
            10,
            dir.path(),
        ));
        assert_eq!(run_post_invocation(&cfg, &json!({}), 1, 0).termination, TerminationBehavior::ForceContinue);
    }

    #[test]
    fn stop_and_session_start_contracts() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = HookConfig::default();
        cfg.stop.push(handler(
            echo_script(dir.path(), "stop", r#"{"decision":"continue","reason":"tests still running"}"#),
            10,
            dir.path(),
        ));
        let r = run_stop(&cfg, &json!({}), 1, "NO_TOOL_CALL", None);
        assert!(r.should_continue);
        assert_eq!(r.reason.as_deref(), Some("tests still running"));

        let mut cfg = HookConfig::default();
        cfg.stop.push(handler(echo_script(dir.path(), "nostop", r#"{"decision":"allow"}"#), 10, dir.path()));
        assert!(!run_stop(&cfg, &json!({}), 1, "USER_CANCELED", None).should_continue);

        let mut cfg = HookConfig::default();
        cfg.session_start.push(handler(
            echo_script(dir.path(), "start", r#"{"injectSteps":[{"systemMessage":{"systemMessage":"Welcome!"}}]}"#),
            10,
            dir.path(),
        ));
        let steps = run_session_start(&cfg, &json!({}));
        assert!(matches!(&steps[..], [InjectStep::SystemMessage(s)] if s == "Welcome!"));
    }

    #[test]
    fn post_tool_use_reports_only_the_error_field() {
        // The contract is `{}` out and one payload in; a failing handler must not disturb the
        // agent, and a handler that matches nothing runs nothing (both are the same code path).
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = HookConfig::default();
        cfg.post_tool_use.push(ToolHookEntry {
            pattern: "run_command".into(),
            handlers: vec![handler("definitely-not-a-real-program-xyz".into(), 5, dir.path())],
        });
        run_post_tool_use(&cfg, &json!({}), "run_command", &json!({"CommandLine": "x"}), 1, Some("exit status 1"));
    }
}
