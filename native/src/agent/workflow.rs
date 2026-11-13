//! `run_workflow`: a script that orchestrates child agents.
//!
//! AntiCraft runs these scripts in **Rhai**, not Python: the engine makes the workflow a
//! pure-Rust feature with no interpreter on `PATH`, and a script can only do what the
//! registered primitives allow.
//!
//! Primitives (the names Antigravity's `run-workflow` skill documents):
//!
//! | primitive | behavior |
//! |---|---|
//! | `log(text)` | append a line to the result |
//! | `phase(name)` | set the phase later logs and agents group under |
//! | `agent(prompt, schema)` | run one child agent, or defer it inside a fan-out |
//! | `parallel(items, f)` | run `f` for every item concurrently; results in item order |
//! | `pipeline(items, f)` | run `f` for every item, one at a time; results in item order |
//! | `finish(value)` | the script's final value |
//!
//! Rhai has no script-level `await`, so every primitive blocks. `parallel` is where real
//! concurrency lives: while a fan-out runs, `agent()` records the child instead of starting
//! it, so the fan-out can start every child before waiting for any. `pipeline` runs them
//! strictly one after another. Both return results in item order.
//!
//! **Threading.** `rhai::Engine` is not `Send` without the crate's `sync` feature, so the
//! engine is built, used and dropped on one dedicated thread. That thread also owns a
//! `tokio::runtime::Handle`, which it may block on because it is not a runtime worker. The
//! async side awaits a oneshot, so cancellation still works.
//!
//! **Bounds.** A workflow is model-written code: it gets [`MAX_CHILDREN`] child runs,
//! [`MAX_OPERATIONS`] VM operations, bounded recursion and [`MAX_LOG_LINES`] of output.
//! Children never run the parent's hooks and answer every confirmation with `Cancel`,
//! because a workflow cannot ask the user anything.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{json, Value};

use super::orchestrate::RUN_WORKFLOW;
use super::{Agent, AgentConfig, AgentEvent, ConfirmOutcome, FinishReason, History, ToolRegistry};
use crate::agent::mcp::schema::sanitize_schema;
use crate::agent::policy::ApprovalPolicy;
use crate::api::{Content, FunctionDeclaration, ModelBackend};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Child agent runs allowed per workflow call.
pub const MAX_CHILDREN: usize = 64;
/// Rhai VM operations before the script is stopped (an infinite loop ends here).
pub const MAX_OPERATIONS: u64 = 5_000_000;
/// Expression nesting allowed (bounded recursion).
pub const MAX_EXPR_DEPTH: usize = 64;
/// Script size cap.
pub const MAX_SCRIPT_BYTES: usize = 256 * 1024;
/// `parallel()`/`pipeline()` nesting allowed.
const MAX_FANOUT_DEPTH: usize = 8;
/// Collected `log()` lines; further lines are dropped and reported.
pub const MAX_LOG_LINES: usize = 500;
/// Model turns a child agent may take.
pub const CHILD_MAX_TURNS: u32 = 20;
/// Wall-clock budget for one child run.
const CHILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The tool a workflow child calls to return its structured result.
pub const FINISH: &str = "finish";

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// Arguments of `run_workflow`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkflowArgs {
    /// Inline script. Exactly one of `Script` / `ScriptPath`.
    #[serde(rename = "Script", default)]
    pub script: Option<String>,
    #[serde(rename = "ScriptPath", default)]
    pub script_path: Option<String>,
    /// JSON object the script reads as `args`.
    #[serde(rename = "Args", default)]
    pub args: Option<Value>,
    #[serde(rename = "Description", default)]
    pub description: Option<String>,
}

impl WorkflowArgs {
    /// Parse and validate. Exclusivity is enforced here because the Code Assist schema has
    /// no `oneOf`, so the declaration cannot express it.
    pub fn parse(v: &Value) -> Result<Self, String> {
        let a: Self = serde_json::from_value(v.clone()).map_err(|e| format!("invalid run_workflow args: {e}"))?;
        let has_script = a.script.as_deref().is_some_and(|s| !s.trim().is_empty());
        let has_path = a.script_path.as_deref().is_some_and(|s| !s.trim().is_empty());
        match (has_script, has_path) {
            (true, true) => return Err("set exactly one of Script or ScriptPath, not both".into()),
            (false, false) => return Err("one of Script or ScriptPath is required".into()),
            _ => {}
        }
        if let Some(s) = &a.script {
            if s.len() > MAX_SCRIPT_BYTES {
                return Err(format!("Script is {} bytes (limit {MAX_SCRIPT_BYTES})", s.len()));
            }
        }
        if let Some(args) = &a.args {
            if !args.is_null() && !args.is_object() {
                return Err("Args must be a JSON object".into());
            }
        }
        Ok(a)
    }

    /// Script text: the inline one, else the file `ScriptPath` names. `read` bounds the path
    /// to the workspace and returns `None` when the file cannot be used.
    pub fn resolve(&self, read: impl FnOnce(&str) -> Option<String>) -> Result<String, String> {
        let script = match (&self.script, &self.script_path) {
            (Some(s), _) if !s.trim().is_empty() => s.clone(),
            (_, Some(p)) if !p.trim().is_empty() => read(p)
                .ok_or_else(|| format!("cannot read workflow script {p:?}: it must be a file inside the workspace"))?,
            _ => return Err("one of Script or ScriptPath is required".into()),
        };
        if script.trim().is_empty() {
            return Err("the workflow script is empty".into());
        }
        if script.len() > MAX_SCRIPT_BYTES {
            return Err(format!("the workflow script is larger than {MAX_SCRIPT_BYTES} bytes"));
        }
        Ok(script)
    }
}

/// The tool declaration.
pub fn declaration() -> FunctionDeclaration {
    FunctionDeclaration {
        name: RUN_WORKFLOW.into(),
        description: "Run a workflow script that orchestrates many child agents. Primitives: \
                      agent(prompt, schema), parallel(items, f), pipeline(items, f), phase(name), \
                      log(value), finish(value). Returns the logs and the final value."
            .into(),
        parameters_json_schema: json!({"type":"object","properties":{
            "Script":{"type":"string","description":"Inline workflow script."},
            "ScriptPath":{"type":"string","description":"Path (inside the workspace) of a saved .rhai script. Use instead of Script."},
            "Args":{"type":"object","description":"Inputs the script reads as `args`."},
            "Description":{"type":"string","description":"One-line goal shown in the UI."}
        }}),
    }
}

/// Declaration of the `finish` tool a workflow child uses to return its result.
pub fn finish_declaration(schema: &Value) -> FunctionDeclaration {
    // Sanitize the caller-supplied schema before embedding it: a workflow script is model-written
    // code, so the schema it passes in is untrusted in the same way a server-supplied MCP schema
    // is. An unsanitized schema with `$ref`, deeply-nested objects, or a missing `required` array
    // would produce a declaration the API rejects for the whole request.
    let safe = sanitize_schema(schema);
    FunctionDeclaration {
        name: FINISH.into(),
        description: format!(
            "Return your final result. The `value` must match this JSON schema: {}",
            safe
        ),
        parameters_json_schema: json!({"type":"object","properties":{
            "value": safe
        },"required":["value"]}),
    }
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// How one child run ended.
#[derive(Debug, Clone)]
pub struct ChildOutcome {
    /// The value the child passed to `finish`, else its last text.
    pub value: Value,
    /// The child stopped without calling `finish` (its answer is prose).
    pub no_finish: bool,
}

/// What a workflow run produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkflowOutcome {
    pub logs: Vec<String>,
    /// The script's `finish` value, rendered as text.
    pub output: String,
    /// Child agents started.
    pub agents: usize,
    /// Log lines were dropped past [`MAX_LOG_LINES`].
    pub logs_truncated: bool,
}

/// A child agent runner: production uses [`AgentChildRunner`], tests use a fake.
pub trait ChildRunner: Send + Sync {
    fn run(&self, prompt: String, schema: Value) -> BoxFuture<'static, Result<ChildOutcome, String>>;
}

/// Runs a workspace tool from a script (`call_tool`). Production uses
/// [`WorkspaceToolCaller`]; tests use a fake.
pub trait ToolCaller: Send + Sync {
    /// Execute `name` with `args` and return its text output. Errors are strings so the script
    /// can catch them.
    fn call(&self, name: String, args: Value) -> BoxFuture<'static, Result<String, String>>;
}

/// [`ToolCaller`] over the real [`ToolRegistry`].
///
/// A workflow cannot ask the user anything, so a call the policy would only allow after a
/// confirmation is denied: that is the documented contract ("a call that needs user approval
/// that has not already been granted is denied"). Read and search tools run because the
/// policy allows them; mutators are denied unless the policy itself allows them.
pub struct WorkspaceToolCaller {
    tools: ToolRegistry,
    policy: ApprovalPolicy,
}

impl WorkspaceToolCaller {
    pub fn new(tools: ToolRegistry, policy: ApprovalPolicy) -> Self {
        Self { tools, policy }
    }
}

impl ToolCaller for WorkspaceToolCaller {
    fn call(&self, name: String, args: Value) -> BoxFuture<'static, Result<String, String>> {
        let tools = self.tools.clone();
        let policy = self.policy.clone();
        Box::pin(async move {
            let kind = tools.kind(&name).ok_or_else(|| format!("unknown tool {name:?}"))?;
            let details = if kind.is_mutator() {
                Some(tools.confirmation(&name, &args).map_err(|e| e.to_string())?)
            } else {
                None
            };
            match policy.decide(kind, details.as_ref()) {
                crate::agent::policy::Decision::Allow => {}
                crate::agent::policy::Decision::Ask => {
                    return Err(format!("{name} needs the user's approval, which a workflow cannot request"))
                }
                crate::agent::policy::Decision::Deny(r) => return Err(format!("{name} was denied: {r}")),
            }
            tools
                .execute_with(&name, &args, details.as_ref(), &CancellationToken::new())
                .await
                .map(|o| o.llm_content)
                .map_err(|e| e.to_string())
        })
    }
}

/// Why a workflow stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowError {
    /// The script failed (syntax, runtime, operation limit, child budget).
    Script(String),
    /// The parent turn was cancelled.
    Cancelled,
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowError::Script(m) => write!(f, "workflow script error: {m}"),
            WorkflowError::Cancelled => write!(f, "workflow cancelled"),
        }
    }
}

/// The tool result: `Logs:` and `Output:` sections, per the documented contract.
pub fn format_result(out: &WorkflowOutcome) -> String {
    let mut s = String::from("Logs:\n");
    if out.logs.is_empty() {
        s.push_str("(none)\n");
    }
    for line in &out.logs {
        s.push_str(line.trim_end());
        s.push('\n');
    }
    if out.logs_truncated {
        s.push_str(&format!("(log output truncated at {MAX_LOG_LINES} lines)\n"));
    }
    s.push_str("\nOutput:\n");
    let o = out.output.trim_end();
    s.push_str(if o.is_empty() { "(no output)" } else { o });
    s.push('\n');
    s
}

/// One-line summary for a notice: the goal, the agent count, then the result.
pub fn summary(description: &str, out: &WorkflowOutcome) -> String {
    let mut s = String::new();
    if !description.trim().is_empty() {
        s.push_str(description.trim());
        s.push('\n');
    }
    s.push_str(&format!("{} agent(s), {} log line(s)\n", out.agents, out.logs.len()));
    s.push_str(&format_result(out));
    s
}

// ---------------------------------------------------------------------------
// Production child runner
// ---------------------------------------------------------------------------

/// Runs a child [`Agent`] on the parent's backend and workspace.
pub struct AgentChildRunner {
    backend: Arc<dyn ModelBackend>,
    tools: ToolRegistry,
    base: AgentConfig,
    policy: Option<ApprovalPolicy>,
    schema: Value,
}

impl AgentChildRunner {
    pub fn new(backend: Arc<dyn ModelBackend>, tools: ToolRegistry, base: AgentConfig) -> Self {
        Self { backend, tools, base, policy: None, schema: Value::Null }
    }

    pub fn with_policy(mut self, policy: ApprovalPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Per-child prompt and schema are set by the workflow's `agent()` primitive; this
    /// runner keeps the shared configuration.
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.schema = schema;
        self
    }

    /// The configuration one child runs with, from the parent's `base`.
    ///
    /// A child is independent: its own identity, its own turn budget, no re-entrant hooks, and
    /// no Bypass Mode. The route is the user's own message, and since each child is its own
    /// `Agent` its request count would start at 1 again: one message running ten agents would
    /// send ten times the budget the mode promises. Nobody can answer a confirmation from inside
    /// a workflow either, so saying so up front lets the MCP path refuse an unanswerable `Ask`.
    fn child_config(base: &AgentConfig, schema: Value) -> AgentConfig {
        let mut cfg = base.clone();
        cfg.session_id = format!("{}-wf-{}", base.session_id, &uuid::Uuid::new_v4().simple().to_string()[..8]);
        cfg.max_turns = cfg.max_turns.min(CHILD_MAX_TURNS);
        cfg.hooks = super::hooks::HookConfig::default();
        cfg.finish_schema = Some(schema.clone());
        cfg.bypass = None;
        cfg.can_confirm = false;
        cfg.system_prompt = format!(
            "{}\n\n# Workflow child\n\nYou are one agent of a larger workflow. Answer only the task in the \
             user message; you do not see the other agents or the script.\n\nReturn your result by calling \
             the `{FINISH}` tool with a value matching this JSON schema:\n{}",
            cfg.system_prompt,
            serde_json::to_string(&schema).unwrap_or_else(|_| "{}".into())
        );
        cfg
    }
}

impl ChildRunner for AgentChildRunner {
    fn run(&self, prompt: String, schema: Value) -> BoxFuture<'static, Result<ChildOutcome, String>> {
        let backend = self.backend.clone();
        let tools = self.tools.clone();
        let policy = self.policy.clone();
        let schema = if schema.is_null() { self.schema.clone() } else { schema };
        let cfg = Self::child_config(&self.base, schema.clone());
        Box::pin(async move {
            let mut agent = Agent::new(backend, tools, cfg, History::new());
            if let Some(p) = policy {
                agent = agent.with_policy(p.shared());
            }
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            // The child's events MUST be drained: an unanswered Confirmation leaves the child
            // loop parked on a oneshot forever. Nobody can approve from a workflow, so every
            // confirmation and review is denied.
            let drain = tokio::spawn(async move {
                let mut finished = None;
                while let Some(ev) = rx.recv().await {
                    match ev {
                        AgentEvent::Confirmation(c) => {
                            let _ = c.respond.send(ConfirmOutcome::Cancel);
                        }
                        AgentEvent::ReviewRequested(r) => {
                            let _ = r.respond.send(super::orchestrate::ReviewOutcome::Cancelled);
                        }
                        AgentEvent::Finished { reason, history } => {
                            finished = Some((reason, history));
                            break;
                        }
                        _ => {}
                    }
                }
                let _ = done_tx.send(finished);
            });
            agent.run_prompt(prompt, tx, CancellationToken::new()).await;
            let _ = drain.await;
            let Some((reason, history)) = done_rx.await.ok().flatten() else {
                return Err("child agent ended without reporting a result".into());
            };
            if reason == FinishReason::Cancelled {
                return Err("child agent was cancelled".into());
            }
            Ok(extract_child_result(history))
        })
    }
}

/// Pull the child's result out of its history: the last `finish` call's `value`, else the
/// last model text.
pub fn extract_child_result(history: Vec<Content>) -> ChildOutcome {
    for c in history.iter().rev() {
        for p in c.parts.iter().rev() {
            if let Some(call) = &p.function_call {
                if call.name == FINISH {
                    let value = match call.args.get("value") {
                        Some(v) => v.clone(),
                        None if call.args.as_object().is_some_and(|o| o.len() == 1) => {
                            call.args.as_object().and_then(|o| o.values().next()).cloned().unwrap_or(Value::Null)
                        }
                        None => call.args.clone(),
                    };
                    return ChildOutcome { value, no_finish: false };
                }
            }
        }
    }
    let text = history.iter().rev().map(|c| c.text()).find(|t| !t.trim().is_empty()).unwrap_or_default();
    ChildOutcome { value: Value::String(text), no_finish: true }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// State the primitives share. Single-threaded by construction: the engine and all its
/// closures live on one thread.
struct RunState {
    logs: Vec<String>,
    truncated: bool,
    phase: String,
    output: Option<String>,
    agents: usize,
    /// Fan-out frames, innermost last. While a frame exists, `agent()` records the child in it
    /// instead of running it, so the fan-out decides the concurrency. A stack (not one slot)
    /// is what lets `parallel` nest inside `parallel`.
    frames: Vec<Vec<ChildSpec>>,
}

/// One child call recorded during a fan-out.
#[derive(Clone)]
struct ChildSpec {
    prompt: String,
    schema: Value,
}

/// Run `script` with `runner` and no workspace tools (`call_tool` is unavailable).
pub async fn run_workflow(
    script: String,
    args: Value,
    runner: Arc<dyn ChildRunner>,
    handle: tokio::runtime::Handle,
    cancel: CancellationToken,
) -> Result<WorkflowOutcome, WorkflowError> {
    run_workflow_with_tools(script, args, runner, None, handle, cancel).await
}

/// [`run_workflow`] with `call_tool` bound to `tools`. Resolves when the script ends,
/// fails, or is cancelled.
pub async fn run_workflow_with_tools(
    script: String,
    args: Value,
    runner: Arc<dyn ChildRunner>,
    tools: Option<Arc<dyn ToolCaller>>,
    handle: tokio::runtime::Handle,
    cancel: CancellationToken,
) -> Result<WorkflowOutcome, WorkflowError> {
    if cancel.is_cancelled() {
        return Err(WorkflowError::Cancelled);
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let thread_cancel = cancel.clone();
    std::thread::Builder::new()
        .name("workflow-engine".into())
        .spawn(move || {
            let _ = tx.send(run_on_thread(script, args, runner, tools, handle, thread_cancel));
        })
        .map_err(|e| WorkflowError::Script(format!("cannot start the workflow thread: {e}")))?;
    tokio::select! {
        r = rx => r.unwrap_or_else(|_| Err(WorkflowError::Script("the workflow thread stopped unexpectedly".into()))),
        _ = cancel.cancelled() => Err(WorkflowError::Cancelled),
    }
}

fn run_on_thread(
    script: String,
    args: Value,
    runner: Arc<dyn ChildRunner>,
    tools: Option<Arc<dyn ToolCaller>>,
    handle: tokio::runtime::Handle,
    cancel: CancellationToken,
) -> Result<WorkflowOutcome, WorkflowError> {
    use rhai::{Dynamic, Engine, Map, Scope};
    use std::cell::RefCell;
    use std::rc::Rc;

    let mut engine = Engine::new();
    // Bound the VM: this is the only thing that stops a runaway script, because a blocking
    // thread cannot be interrupted from outside.
    engine.set_max_operations(MAX_OPERATIONS);
    engine.set_max_expr_depths(MAX_EXPR_DEPTH, MAX_EXPR_DEPTH);
    engine.set_max_string_size(MAX_LOG_LINES * 4096);
    engine.set_max_array_size(10_000);
    engine.set_max_map_size(1_000);

    let ast = engine.compile(script).map_err(|e| WorkflowError::Script(e.to_string()))?;
    let st = Rc::new(RefCell::new(RunState {
        logs: Vec::new(),
        truncated: false,
        phase: String::new(),
        output: None,
        agents: 0,
        frames: Vec::new(),
    }));

    {
        let st = st.clone();
        engine.register_fn("log", move |v: Dynamic| {
            let mut s = st.borrow_mut();
            if s.logs.len() >= MAX_LOG_LINES {
                s.truncated = true;
                return;
            }
            let line = if v.is_string() { v.into_string().unwrap_or_default() } else { render_dynamic(&v) };
            let phase = s.phase.clone();
            if phase.is_empty() {
                s.logs.push(line);
            } else {
                s.logs.push(format!("[{phase}] {line}"));
            }
        });
    }
    {
        let st = st.clone();
        engine.register_fn("phase", move |name: &str| {
            let mut s = st.borrow_mut();
            s.phase = name.to_owned();
        });
    }
    {
        let st = st.clone();
        engine.register_fn("finish", move |v: Dynamic| {
            st.borrow_mut().output = Some(render_dynamic(&v));
        });
    }
    // `agent(prompt, schema)`: runs the child, unless a fan-out frame is collecting it.
    {
        let st = st.clone();
        let runner = runner.clone();
        let handle = handle.clone();
        let cancel = cancel.clone();
        engine.register_fn("agent", move |prompt: &str, schema: Dynamic| -> Result<Dynamic, Box<rhai::EvalAltResult>> {
            let mut s = st.borrow_mut();
            if s.agents >= MAX_CHILDREN {
                return Err(script_err(format!("a workflow may start at most {MAX_CHILDREN} agents")));
            }
            s.agents += 1;
            let spec = ChildSpec { prompt: prompt.to_owned(), schema: dynamic_to_json(schema) };
            match s.frames.last_mut() {
                // Deferred: the fan-out decides when (and how concurrently) this child runs.
                Some(frame) => {
                    frame.push(spec);
                    Ok(Dynamic::UNIT)
                }
                None => {
                    drop(s);
                    run_one_child(runner.as_ref(), &handle, &cancel, spec).map(|v| json_to_dynamic(&v))
                }
            }
        });
    }
    // `parallel(items, f)` / `pipeline(items, f)`: run `f` per item, concurrently or in order.
    // Each item's result is one entry: the lambda's own value when it ran no agent, the child's
    // value for one agent, or an array for several. That keeps results aligned with `items`
    // even when the lambda branches.
    for (name, concurrent) in [("parallel", true), ("pipeline", false)] {
        let st = st.clone();
        let runner = runner.clone();
        let handle = handle.clone();
        let cancel = cancel.clone();
        engine.register_fn(name, move |ctx: rhai::NativeCallContext, items: rhai::Array, f: rhai::FnPtr| {
            if cancel.is_cancelled() {
                return Err(script_err("workflow cancelled".into()));
            }
            if concurrent {
                // Evaluate every lambda first (each only records specs), then run all the
                // recorded children together and fail fast on the first error.
                let mut plans = Vec::with_capacity(items.len());
                for item in items.iter().cloned() {
                    plans.push(eval_item(&st, &ctx, item, &f)?);
                }
                let runner = runner.clone();
                let cancel = cancel.clone();
                let values = handle.block_on(async move {
                    use futures::StreamExt as _;
                    let mut futs = futures::stream::FuturesUnordered::new();
                    let mut slots: Vec<Vec<Option<Value>>> = Vec::with_capacity(plans.len());
                    for (i, (specs, ret)) in plans.into_iter().enumerate() {
                        if specs.is_empty() {
                            slots.push(vec![Some(dynamic_to_json(ret))]);
                            continue;
                        }
                        slots.push(vec![None; specs.len()]);
                        for (j, spec) in specs.into_iter().enumerate() {
                            let r = runner.clone();
                            let c = cancel.clone();
                            futs.push(async move { (i, j, run_child_future(r.as_ref(), &c, spec).await) });
                        }
                    }
                    // Dropping `futs` on the first error cancels the siblings still running.
                    while let Some((i, j, r)) = futs.next().await {
                        match r {
                            Ok(v) => slots[i][j] = Some(v),
                            Err(e) => return Err(e),
                        }
                    }
                    Ok::<_, String>(slots)
                })?;
                let mut arr = rhai::Array::new();
                for slot in values {
                    arr.push(item_result(slot));
                }
                Ok(Dynamic::from(arr))
            } else {
                // Pipeline: each item's lambda runs, and its children finish, before the next.
                let mut arr = rhai::Array::new();
                for item in items.iter().cloned() {
                    if cancel.is_cancelled() {
                        return Err(script_err("workflow cancelled".into()));
                    }
                    let (specs, ret) = eval_item(&st, &ctx, item, &f)?;
                    if specs.is_empty() {
                        arr.push(ret);
                        continue;
                    }
                    let mut values = Vec::with_capacity(specs.len());
                    for spec in specs {
                        if cancel.is_cancelled() {
                            return Err(script_err("workflow cancelled".into()));
                        }
                        values.push(run_one_child(runner.as_ref(), &handle, &cancel, spec)?);
                    }
                    arr.push(item_result(values.into_iter().map(Some).collect()));
                }
                Ok(Dynamic::from(arr))
            }
        });
    }

    // `call_tool(name, args)`: run a workspace tool. A workflow cannot ask the user anything,
    // so a tool the policy would only allow after a confirmation fails the script.
    if let Some(tools) = tools.clone() {
        let handle = handle.clone();
        let cancel = cancel.clone();
        engine.register_fn("call_tool", move |name: &str, args: Dynamic| -> Result<Dynamic, Box<rhai::EvalAltResult>> {
            if cancel.is_cancelled() {
                return Err(script_err("workflow cancelled".into()));
            }
            let args = dynamic_to_json(args);
            let fut = tools.call(name.to_owned(), args);
            handle.block_on(fut).map(Dynamic::from).map_err(|e| script_err(format!("{name}: {e}")))
        });
    }

    let mut scope = Scope::new();
    if let Some(obj) = args.as_object() {
        let mut m = Map::new();
        for (k, v) in obj {
            m.insert(k.as_str().into(), json_to_dynamic(v));
        }
        scope.push_constant("args", m);
    }

    let result = engine.eval_ast_with_scope::<Dynamic>(&mut scope, &ast);
    let mut s = st.borrow_mut();
    let output = match s.output.take() {
        Some(o) => o,
        None => result.as_ref().map(render_dynamic).unwrap_or_default(),
    };
    let outcome = WorkflowOutcome {
        logs: std::mem::take(&mut s.logs),
        output,
        agents: s.agents,
        logs_truncated: s.truncated,
    };
    drop(s);
    match result {
        Ok(_) => Ok(outcome),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("workflow cancelled") {
                Err(WorkflowError::Cancelled)
            } else {
                Err(WorkflowError::Script(msg))
            }
        }
    }
}

/// Evaluate one fan-out item: push a frame, call `f(item)`, pop the frame. Returns the child
/// specs the lambda recorded (in call order) and the lambda's own value, which is the item's
/// result when it started no agent.
fn eval_item(
    st: &std::rc::Rc<std::cell::RefCell<RunState>>,
    ctx: &rhai::NativeCallContext,
    item: rhai::Dynamic,
    f: &rhai::FnPtr,
) -> Result<(Vec<ChildSpec>, rhai::Dynamic), Box<rhai::EvalAltResult>> {
    let depth = {
        let mut s = st.borrow_mut();
        s.frames.push(Vec::new());
        s.frames.len()
    };
    if depth > MAX_FANOUT_DEPTH {
        st.borrow_mut().frames.pop();
        return Err(script_err(format!("parallel()/pipeline() nested more than {MAX_FANOUT_DEPTH} deep")));
    }
    let ret = f.call_within_context::<rhai::Dynamic>(ctx, (item,));
    let specs = st.borrow_mut().frames.pop().unwrap_or_default();
    let ret = ret.map_err(|e| script_err(e.to_string()))?;
    Ok((specs, ret))
}

/// Collapse one item's child values into its result: none (the lambda's own value), one
/// (that child's value), or several (an array, in call order).
fn item_result(slot: Vec<Option<Value>>) -> rhai::Dynamic {
    let mut values: Vec<Value> = slot.into_iter().map(|v| v.unwrap_or(Value::Null)).collect();
    match values.len() {
        0 => json_to_dynamic(&Value::Null),
        1 => json_to_dynamic(&values.remove(0)),
        _ => {
            let mut arr = rhai::Array::new();
            for v in values {
                arr.push(json_to_dynamic(&v));
            }
            rhai::Dynamic::from(arr)
        }
    }
}

fn run_child_future(
    runner: &dyn ChildRunner,
    cancel: &CancellationToken,
    spec: ChildSpec,
) -> BoxFuture<'static, Result<Value, String>> {
    let fut = runner.run(spec.prompt, spec.schema);
    let cancel = cancel.clone();
    Box::pin(async move {
        tokio::select! {
            r = tokio::time::timeout(CHILD_TIMEOUT, fut) => match r {
                // A child that never called `finish` failed the workflow: its value is prose
                // (often an apology after a cancelled edit, or "[stopped: turn limit]"), not a
                // result. The contract is "an agent that ends without calling finish fails".
                Ok(Ok(o)) if o.no_finish => {
                    let snippet = match &o.value {
                        Value::String(s) => s.chars().take(120).collect::<String>(),
                        other => other.to_string().chars().take(120).collect::<String>(),
                    };
                    Err(format!("an agent ended without calling finish: {snippet}"))
                }
                Ok(Ok(o)) => Ok(o.value),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(format!("an agent timed out after {}s", CHILD_TIMEOUT.as_secs())),
            },
            _ = cancel.cancelled() => Err("workflow cancelled".to_string()),
        }
    })
}

fn run_one_child(
    runner: &dyn ChildRunner,
    handle: &tokio::runtime::Handle,
    cancel: &CancellationToken,
    spec: ChildSpec,
) -> Result<Value, Box<rhai::EvalAltResult>> {
    if cancel.is_cancelled() {
        return Err(script_err("workflow cancelled".into()));
    }
    handle.block_on(run_child_future(runner, cancel, spec)).map_err(script_err)
}

fn script_err(m: String) -> Box<rhai::EvalAltResult> {
    Box::new(rhai::EvalAltResult::ErrorRuntime(m.into(), rhai::Position::NONE))
}

// ---------------------------------------------------------------------------
// JSON <-> Dynamic
// ---------------------------------------------------------------------------

fn json_to_dynamic(v: &Value) -> rhai::Dynamic {
    use rhai::{Array, Dynamic, Map};
    match v {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => Dynamic::from(i as rhai::INT),
            None => Dynamic::from(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => Dynamic::from(s.clone()),
        Value::Array(a) => {
            let mut arr = Array::new();
            for item in a {
                arr.push(json_to_dynamic(item));
            }
            Dynamic::from(arr)
        }
        Value::Object(o) => {
            let mut m = Map::new();
            for (k, val) in o {
                m.insert(k.as_str().into(), json_to_dynamic(val));
            }
            Dynamic::from(m)
        }
    }
}

fn dynamic_to_json(d: rhai::Dynamic) -> Value {
    if d.is_unit() {
        return Value::Null;
    }
    if let Some(b) = d.clone().try_cast::<bool>() {
        return Value::from(b);
    }
    if let Some(i) = d.clone().try_cast::<rhai::INT>() {
        return Value::from(i);
    }
    if let Some(f) = d.clone().try_cast::<rhai::FLOAT>() {
        return Value::from(f);
    }
    if d.is_array() {
        return Value::Array(d.cast::<rhai::Array>().into_iter().map(dynamic_to_json).collect());
    }
    if d.is_map() {
        let mut o = serde_json::Map::new();
        for (k, v) in d.cast::<rhai::Map>() {
            o.insert(k.to_string(), dynamic_to_json(v));
        }
        return Value::Object(o);
    }
    if let Ok(s) = d.clone().into_string() {
        return Value::String(s);
    }
    Value::String(d.to_string())
}

/// Render any Rhai value as text for `finish` and the result's `Output:` section.
fn render_dynamic(d: &rhai::Dynamic) -> String {
    if d.is_unit() {
        return String::new();
    }
    match dynamic_to_json(d.clone()) {
        Value::String(s) => s,
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Workspace script loading
// ---------------------------------------------------------------------------

/// Read a `ScriptPath` from the workspace. Only a regular `.rhai` file inside the workspace
/// is accepted; the size cap applies.
pub fn load_script(tools: &ToolRegistry, path: &str) -> Option<String> {
    let ws = tools.workspace();
    let p = ws.resolve(path).ok()?;
    let is_rhai = p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("rhai"));
    if !p.is_file() || !is_rhai {
        return None;
    }
    let text = std::fs::read_to_string(&p).ok()?;
    (text.len() <= MAX_SCRIPT_BYTES).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(script: Option<&str>, path: Option<&str>) -> WorkflowArgs {
        WorkflowArgs { script: script.map(str::to_owned), script_path: path.map(str::to_owned), ..Default::default() }
    }

    /// Answers each prompt with `echo:<prompt>` and counts the calls.
    struct Echo(std::sync::atomic::AtomicUsize);

    impl ChildRunner for Echo {
        fn run(&self, prompt: String, _schema: Value) -> BoxFuture<'static, Result<ChildOutcome, String>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Ok(ChildOutcome { value: Value::String(format!("echo:{prompt}")), no_finish: false }) })
        }
    }

    /// Never called: proves a script started no agent.
    struct NoAgents;

    impl ChildRunner for NoAgents {
        fn run(&self, _p: String, _s: Value) -> BoxFuture<'static, Result<ChildOutcome, String>> {
            Box::pin(async { Err("no agent expected".to_string()) })
        }
    }

    fn rt() -> tokio::runtime::Handle {
        tokio::runtime::Handle::current()
    }

    /// A workflow child never inherits Bypass Mode: the mode is part of the user's own message,
    /// and each child starts its request count over, so passing it on would multiply the budget.
    #[test]
    fn a_workflow_child_does_not_inherit_the_bypass_route() {
        use crate::agent::bypass::BypassRoute;
        use crate::api::{CaError, CaGenerateResponse, GenerateContentRequest, ModelBackend};

        struct Silent;
        impl ModelBackend for Silent {
            fn stream_turn(
                &self,
                _m: String,
                _p: String,
                _r: GenerateContentRequest,
                _c: CancellationToken,
            ) -> futures::stream::BoxStream<'static, Result<CaGenerateResponse, CaError>> {
                use futures::StreamExt;
                futures::stream::empty().boxed()
            }
        }

        let mut cfg = AgentConfig::new("gemini-2.5-pro", "sys", "s");
        cfg.bypass = Some(BypassRoute {
            backend: std::sync::Arc::new(Silent),
            model: "bypass-model".into(),
            label: "Gateway".into(),
            requests: 3,
            instructions: String::new(),
            context_window: 128_000,
        });
        let child = AgentChildRunner::child_config(&cfg, json!({"type": "object"}));
        assert!(child.bypass.is_none(), "the child must not route through the bypass provider");
        // Everything else a child needs is still its own.
        assert!(child.can_confirm == false && child.finish_schema.is_some());
        assert!(child.session_id.starts_with("s-wf-"));
    }

    #[test]
    fn parse_needs_exactly_one_source() {
        assert!(WorkflowArgs::parse(&json!({})).is_err());
        assert!(WorkflowArgs::parse(&json!({"Script":"log(1)","ScriptPath":"x.rhai"})).is_err());
        assert_eq!(WorkflowArgs::parse(&json!({"Script":"log(1)"})).unwrap().script.as_deref(), Some("log(1)"));
        assert!(WorkflowArgs::parse(&json!({"Script":"  "})).is_err(), "a blank script is no script");
        assert!(WorkflowArgs::parse(&json!({"Script":"x","Args":[1]})).is_err(), "Args must be an object");
        assert!(WorkflowArgs::parse(&json!({"Script":"x","Args":{"a":1}})).is_ok());
        let big = "x".repeat(MAX_SCRIPT_BYTES + 1);
        assert!(WorkflowArgs::parse(&json!({"Script": big})).is_err());
    }

    #[test]
    fn resolve_reads_inline_or_the_file() {
        assert_eq!(args(Some("a"), None).resolve(|_| None).unwrap(), "a");
        assert_eq!(args(None, Some("w.rhai")).resolve(|p| Some(format!("// {p}"))).unwrap(), "// w.rhai");
        assert!(args(None, Some("m.rhai")).resolve(|_| None).is_err());
    }

    #[test]
    fn result_sections() {
        let out = WorkflowOutcome { logs: vec!["a".into(), "b".into()], output: "42".into(), agents: 2, logs_truncated: false };
        assert_eq!(format_result(&out), "Logs:\na\nb\n\nOutput:\n42\n");
        assert_eq!(format_result(&WorkflowOutcome::default()), "Logs:\n(none)\n\nOutput:\n(no output)\n");
        let cut = WorkflowOutcome { logs: vec!["a".into()], logs_truncated: true, ..Default::default() };
        assert!(format_result(&cut).contains("truncated"));
    }

    #[test]
    fn declarations_carry_the_documented_fields() {
        let d = declaration();
        assert_eq!(d.name, RUN_WORKFLOW);
        for k in ["Script", "ScriptPath", "Args", "Description"] {
            assert!(d.parameters_json_schema["properties"].get(k).is_some(), "missing {k}");
        }
        let f = finish_declaration(&json!({"type":"string"}));
        assert_eq!(f.name, FINISH);
        assert_eq!(f.parameters_json_schema["required"], json!(["value"]));
    }

    #[test]
    fn child_result_prefers_finish_over_text() {
        use crate::api::{FunctionCall, Part};
        let h = vec![
            Content::model(vec![Part::text("thinking")]),
            Content::model(vec![Part {
                function_call: Some(FunctionCall { id: None, name: FINISH.into(), args: json!({"value": 42}) }),
                ..Default::default()
            }]),
            Content::model(vec![Part::text("done")]),
        ];
        let r = extract_child_result(h);
        assert_eq!(r.value, json!(42));
        assert!(!r.no_finish);

        let prose = vec![Content::model(vec![Part::text("the answer")])];
        let r = extract_child_result(prose);
        assert_eq!(r.value, json!("the answer"));
        assert!(r.no_finish, "a child that never called finish is flagged");
    }

    #[tokio::test]
    async fn script_primitives_run_without_agents() {
        let out = run_workflow(
            r#"log("start"); phase("Discover"); log("scanning"); finish("done")"#.into(),
            json!({}),
            Arc::new(NoAgents),
            rt(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.output, "done");
        assert_eq!(out.logs, vec!["start".to_string(), "[Discover] scanning".to_string()]);
        assert_eq!(out.agents, 0);
    }

    #[tokio::test]
    async fn args_are_visible_as_a_global_map() {
        let out = run_workflow(
            "finish(args.name)".into(),
            json!({"name": "anti"}),
            Arc::new(NoAgents),
            rt(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.output, "anti");
    }

    #[tokio::test]
    async fn a_child_is_started_and_its_value_returned() {
        let runner = Arc::new(Echo(Default::default()));
        let out = run_workflow(
            r#"let a = agent("hi", #{}); let b = agent("yo", #{}); finish(a + "|" + b)"#.into(),
            json!({}),
            runner.clone(),
            rt(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.output, "echo:hi|echo:yo");
        assert_eq!(runner.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(out.agents, 2);
    }

    #[tokio::test]
    async fn parallel_returns_results_in_item_order() {
        let runner = Arc::new(Echo(Default::default()));
        let out = run_workflow(
            r#"let r = parallel(["a", "b", "c"], |x| agent("job " + x, #{})); finish(r[0] + "," + r[1] + "," + r[2])"#.into(),
            json!({}),
            runner.clone(),
            rt(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.output, "echo:job a,echo:job b,echo:job c");
        assert_eq!(runner.0.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn pipeline_runs_every_item() {
        let runner = Arc::new(Echo(Default::default()));
        let out = run_workflow(
            r#"let r = pipeline(["x", "y"], |i| agent("p " + i, #{})); finish(r)"#.into(),
            json!({}),
            runner.clone(),
            rt(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(runner.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(out.output.contains("echo:p x"));
        assert!(out.output.contains("echo:p y"));
    }

    #[tokio::test]
    async fn runaway_script_is_stopped() {
        let start = std::time::Instant::now();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_workflow(
                "let i = 0; while true { i += 1; }".into(),
                json!({}),
                Arc::new(NoAgents),
                rt(),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("the operation limit must stop the script");
        assert!(matches!(r, Err(WorkflowError::Script(_))), "{r:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(30));
    }

    #[tokio::test]
    async fn the_child_budget_is_enforced() {
        let script = format!(
            "let i = 0; while i < {} {{ agent(\"x\", #{{}}); i += 1; }} finish(\"ok\")",
            MAX_CHILDREN + 1
        );
        let r = run_workflow(script, json!({}), Arc::new(Echo(Default::default())), rt(), CancellationToken::new()).await;
        assert!(matches!(r, Err(WorkflowError::Script(_))), "the cap must stop the script: {r:?}");
    }

    #[tokio::test]
    async fn cancellation_before_start_ends_the_workflow() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let r = run_workflow("finish(1)".into(), json!({}), Arc::new(NoAgents), rt(), cancel).await;
        assert_eq!(r.unwrap_err(), WorkflowError::Cancelled);
    }

    #[tokio::test]
    async fn a_script_error_is_reported_without_hanging() {
        let r = run_workflow("this is not rhai ((".into(), json!({}), Arc::new(NoAgents), rt(), CancellationToken::new()).await;
        assert!(matches!(r, Err(WorkflowError::Script(_))), "{r:?}");
    }

    #[test]
    fn json_dynamic_roundtrip() {
        for v in [json!(null), json!(true), json!(7), json!("s"), json!([1, "a", null]), json!({"a": 1, "b": [true]})] {
            assert_eq!(dynamic_to_json(json_to_dynamic(&v)), v, "roundtrip broke on {v}");
        }
        assert_eq!(render_dynamic(&json_to_dynamic(&json!("plain"))), "plain");
        assert_eq!(render_dynamic(&json_to_dynamic(&json!({"a": 1}))), "{\"a\":1}");
    }

    #[test]
    fn log_lines_are_capped() {
        let script = format!("let i = 0; while i < {} {{ log(\"x\"); i += 1; }} finish(\"done\")", MAX_LOG_LINES + 50);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(run_workflow(script, json!({}), Arc::new(NoAgents), rt.handle().clone(), CancellationToken::new())).unwrap();
        assert_eq!(out.logs.len(), MAX_LOG_LINES);
        assert!(out.logs_truncated);
    }

    /// A child that returns prose instead of calling `finish` must fail the workflow.
    #[tokio::test]
    async fn no_finish_child_fails_the_workflow() {
        struct ProseFaker;
        impl ChildRunner for ProseFaker {
            fn run(&self, _p: String, _s: Value) -> BoxFuture<'static, Result<ChildOutcome, String>> {
                Box::pin(async { Ok(ChildOutcome { value: Value::String("sorry, I cannot help".into()), no_finish: true }) })
            }
        }
        let r = run_workflow(
            r#"let v = agent("review", #{}); finish(v)"#.into(),
            json!({}),
            Arc::new(ProseFaker),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(r, Err(WorkflowError::Script(_))), "a no-finish child must fail: {r:?}");
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("without calling finish"), "error should name the contract: {msg}");
    }

    /// A child that calls `finish` succeeds even when its last text is prose.
    #[tokio::test]
    async fn finish_child_succeeds() {
        struct FinishFaker;
        impl ChildRunner for FinishFaker {
            fn run(&self, _p: String, _s: Value) -> BoxFuture<'static, Result<ChildOutcome, String>> {
                Box::pin(async { Ok(ChildOutcome { value: json!({"ok": true}), no_finish: false }) })
            }
        }
        let out = run_workflow(
            r#"let v = agent("work", #{}); finish(v)"#.into(),
            json!({}),
            Arc::new(FinishFaker),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.output.contains("ok"), "value should be in output: {}", out.output);
    }

    /// Items without a child call return the lambda's own value and stay aligned.
    #[tokio::test]
    async fn parallel_item_with_no_agent_returns_unit() {
        let runner = Arc::new(Echo(Default::default()));
        // Only item "b" calls agent; items "a" and "c" return unit (Dynamic::UNIT -> null).
        let out = run_workflow(
            r#"
                let r = parallel(["a", "b", "c"], |x| {
                    if x == "b" { agent("job " + x, #{}) } else { () }
                });
                finish(r.len())
            "#.into(),
            json!({}),
            runner.clone(),
            tokio::runtime::Handle::current(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        // Three items → result array length 3.
        assert_eq!(out.output.trim(), "3", "parallel must return one entry per item: {}", out.output);
        assert_eq!(runner.0.load(std::sync::atomic::Ordering::SeqCst), 1, "only one agent should start");
    }

    /// `for_child` forces Fast mode and inherits skip_permissions.
    #[test]
    fn for_child_inherits_policy_fields() {
        use crate::agent::policy::{AgentMode, ApprovalPolicy, TerminalPolicy};
        let parent = ApprovalPolicy {
            mode: AgentMode::Planning,
            terminal: TerminalPolicy::Turbo,
            auto_approve_edits: true,
            auto_approve_read_tools: false,
            allowlist: vec!["cargo".into()],
            denylist: vec!["rm -rf".into()],
            skip_permissions: true,
            ..Default::default()
        };
        let child = parent.for_child();
        assert_eq!(child.mode, AgentMode::Fast, "child must be Fast so planning tools are hidden");
        assert!(child.auto_approve_read_tools, "reads must be auto-approved in children");
        assert!(child.auto_approve_edits, "auto_approve_edits is inherited");
        assert!(child.skip_permissions, "skip_permissions is inherited");
        assert_eq!(child.terminal, TerminalPolicy::Turbo, "terminal policy inherited");
        assert_eq!(child.allowlist, vec!["cargo".to_string()]);
        assert_eq!(child.denylist, vec!["rm -rf".to_string()]);
    }
}