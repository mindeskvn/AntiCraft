//! Orchestration tools and state (ORCHESTRATION.md section B).
//!
//! These tools change agent/UI state, not the workspace, so they are intercepted in
//! `Agent::run_one` before the [`super::ToolRegistry`] and never ask for confirmation (except
//! `request_review`, which pauses the loop by design). Names and schemas are AntiCraft's own.
//!
//! | tool               | purpose                                                      |
//! |--------------------|--------------------------------------------------------------|
//! | `task_boundary`    | start/update a task group (name, status, summary, phase)     |
//! | `write_artifact`   | create/replace a Markdown artifact of this conversation      |
//! | `request_review`   | pause until the user approves or requests changes            |
//! | `notify_user`      | non-blocking message to the user (shown as a notice)         |
//! | `task_complete`    | final completion signal with a short result summary          |
//!
//! [`OrchestrationState`] holds the per-agent plan/artifact state and the per-prompt task
//! groups; the agent loop in `mod.rs` drives it. The pause on `request_review` uses the same
//! "send a request with a oneshot, await it cancel-aware" pattern as tool confirmations, an
//! idea taken from google-gemini/gemini-cli (Apache-2.0, see NOTICE) and reimplemented here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::artifacts::{
    validate_name, ArtifactComment, ArtifactError, ArtifactKind, ArtifactMeta, ArtifactStore, ReviewState, MAX_ARTIFACT_BYTES,
};
use super::policy::{AgentMode, ApprovalPolicy};
use crate::api::{Content, FunctionDeclaration, Role};

pub const TASK_BOUNDARY: &str = "task_boundary";
pub const WRITE_ARTIFACT: &str = "write_artifact";
pub const REQUEST_REVIEW: &str = "request_review";
pub const NOTIFY_USER: &str = "notify_user";
pub const TASK_COMPLETE: &str = "task_complete";
pub const RUN_WORKFLOW: &str = "run_workflow";

/// All orchestration tool names.
pub const ORCHESTRATION_TOOLS: &[&str] =
    &[TASK_BOUNDARY, WRITE_ARTIFACT, REQUEST_REVIEW, NOTIFY_USER, TASK_COMPLETE, RUN_WORKFLOW];

pub fn is_orchestration_tool(name: &str) -> bool {
    ORCHESTRATION_TOOLS.contains(&name)
}

/// Phase reported by `task_boundary` (the docs give no enum; AntiCraft's own).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    #[default]
    Planning,
    Executing,
    Verifying,
}

/// Status of a task group card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    InProgress,
    /// Paused on a `request_review` or a confirmation.
    WaitingForUser,
    Completed,
    Failed,
    /// Set by the UI/agent on Stop; never sent by the model.
    Cancelled,
}

/// One `task_boundary` call, as parsed from the model's args.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBoundary {
    /// Stable task name; a new name opens a new task group.
    pub task_name: String,
    #[serde(default)]
    pub status: TaskStatus,
    /// One-line progress summary shown under the title.
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub phase: TaskPhase,
}

/// Arguments of `write_artifact`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WriteArtifactArgs {
    /// `task`, `implementation_plan`, `walkthrough` or another `[a-z0-9_-]` name.
    pub name: String,
    pub content: String,
    #[serde(default)]
    pub summary: String,
}

impl WriteArtifactArgs {
    pub fn kind(&self) -> ArtifactKind {
        ArtifactKind::from_stem(&self.name)
    }
}

/// Arguments of `request_review`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RequestReviewArgs {
    /// Artifacts to review (names). Empty: a free-form question in `message`.
    #[serde(default)]
    pub artifacts: Vec<String>,
    pub message: String,
    /// `false`: FYI only, the loop does not wait (allowed under `AgentDecides`).
    #[serde(default)]
    pub blocking: Option<bool>,
}

/// Answer to a review request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// "Proceed": continue with the plan.
    Approved,
    /// "Proceed" with comments: continue, and take the notes into account while executing.
    ApprovedWithComments(Vec<ArtifactComment>),
    /// "Request changes": comments go back as the function response; the agent revises.
    ChangesRequested(Vec<ArtifactComment>),
    /// Dismissed or turn cancelled: the loop stops like a rejected tool.
    Cancelled,
}

impl ReviewOutcome {
    /// Function-response payload for the model.
    pub fn to_response(&self) -> Value {
        match self {
            ReviewOutcome::Approved => json!({"output": "The user approved. Proceed with execution."}),
            ReviewOutcome::ApprovedWithComments(c) => json!({
                "output": format!("The user approved. Proceed with execution and take these notes into account:
{}", super::artifacts::comments_to_feedback(c)),
                "approved": true,
            }),
            ReviewOutcome::ChangesRequested(c) => {
                json!({"output": super::artifacts::comments_to_feedback(c), "approved": false})
            }
            ReviewOutcome::Cancelled => json!({"error": "The user dismissed the review."}),
        }
    }
}

/// Pause request sent to the UI; answer through `respond`. Dropping the sender means
/// [`ReviewOutcome::Cancelled`].
#[derive(Debug)]
pub struct ReviewRequest {
    pub call_id: String,
    pub artifacts: Vec<String>,
    pub message: String,
    pub respond: oneshot::Sender<ReviewOutcome>,
}

/// Declarations of the orchestration tools. Planning mode gets all six; Fast mode only
/// `notify_user` and `task_complete`.
pub fn declarations(planning: bool) -> Vec<FunctionDeclaration> {
    let mut v = vec![
        FunctionDeclaration {
            name: NOTIFY_USER.into(),
            description: "Show a short message to the user without stopping work.".into(),
            parameters_json_schema: json!({"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}),
        },
        FunctionDeclaration {
            name: TASK_COMPLETE.into(),
            description: "Call once when the whole request is done. Give a one-paragraph result.".into(),
            parameters_json_schema: json!({"type":"object","properties":{"result":{"type":"string"}},"required":["result"]}),
        },
    ];
    if planning {
        v.extend([
            FunctionDeclaration {
                name: TASK_BOUNDARY.into(),
                description: "Open or update the current task group. Use a new task_name for each major unit of work.".into(),
                parameters_json_schema: json!({"type":"object","properties":{
                    "task_name":{"type":"string"},
                    "status":{"type":"string","enum":["in_progress","waiting_for_user","completed","failed"]},
                    "summary":{"type":"string","description":"One line on what is happening now."},
                    "phase":{"type":"string","enum":["planning","executing","verifying"]}
                },"required":["task_name","summary"]}),
            },
            FunctionDeclaration {
                name: WRITE_ARTIFACT.into(),
                description: "Create or replace a Markdown artifact of this conversation (task, implementation_plan, walkthrough).".into(),
                parameters_json_schema: json!({"type":"object","properties":{
                    "name":{"type":"string","pattern":"^[a-z0-9_-]{1,64}$"},
                    "content":{"type":"string"},
                    "summary":{"type":"string"}
                },"required":["name","content"]}),
            },
            FunctionDeclaration {
                name: REQUEST_REVIEW.into(),
                description: "Ask the user to review artifacts or answer a question, and wait for the reply.".into(),
                parameters_json_schema: json!({"type":"object","properties":{
                    "artifacts":{"type":"array","items":{"type":"string"}},
                    "message":{"type":"string"},
                    "blocking":{"type":"boolean"}
                },"required":["message"]}),
            },
            FunctionDeclaration {
                name: RUN_WORKFLOW.into(),
                description: "Run a workflow script that orchestrates many child agents: fan out over \
                              items, run stages in order, and aggregate their results. Use it when the \
                              work splits into many independent or staged pieces; use one normal tool \
                              call for a single piece."
                    .into(),
                // `Script`/`ScriptPath` exclusivity is checked in Rust, not here: the Code Assist
                // schema has no `oneOf`, and a rejected declaration would drop the tool entirely.
                parameters_json_schema: json!({"type":"object","properties":{
                    "Script":{"type":"string","description":"Inline workflow script."},
                    "ScriptPath":{"type":"string","description":"Path (inside the workspace) of a saved .rhai script. Use instead of Script."},
                    "Args":{"type":"object","description":"Inputs the script reads as `args`."},
                    "Description":{"type":"string","description":"One-line goal shown in the UI."}
                },"required":[]}),
            },
        ]);
    }
    v
}

/// Validate `task_boundary` args (name of 1..120 chars).
pub fn parse_task_boundary(args: &Value) -> Result<TaskBoundary, String> {
    let t: TaskBoundary = serde_json::from_value(args.clone()).map_err(|e| format!("invalid task_boundary args: {e}"))?;
    if t.task_name.trim().is_empty() || t.task_name.len() > 120 {
        return Err("task_name must be 1..120 characters".into());
    }
    Ok(t)
}

/// `true` when `name` is declared in the given mode (see [`declarations`]).
pub fn is_available(name: &str, planning: bool) -> bool {
    match name {
        NOTIFY_USER | TASK_COMPLETE => true,
        TASK_BOUNDARY | WRITE_ARTIFACT | REQUEST_REVIEW | RUN_WORKFLOW => planning,
        _ => false,
    }
}

/// Prompt sent for a bare `/plan` with no goal.
pub const BARE_PLAN_PROMPT: &str = "Look at the workspace and the conversation so far, ask me what is unclear, then propose an implementation plan for the next sensible piece of work.";

/// Split a leading `/plan` or `/fast` off a prompt. Returns the forced mode (if any) and the
/// remaining text. `/plan` alone becomes [`BARE_PLAN_PROMPT`]. Other text is returned as is.
pub fn split_mode_command(prompt: &str) -> (Option<AgentMode>, String) {
    let trimmed = prompt.trim_start();
    for (cmd, mode) in [("/plan", AgentMode::Planning), ("/fast", AgentMode::Fast)] {
        if let Some(rest) = trimmed.strip_prefix(cmd) {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                let rest = rest.trim();
                let text = if rest.is_empty() && mode == AgentMode::Planning { BARE_PLAN_PROMPT.to_owned() } else { rest.to_owned() };
                return (Some(mode), text);
            }
        }
    }
    (None, prompt.to_owned())
}

/// Response when the policy approves on the user's behalf.
pub fn auto_approved_response() -> Value {
    json!({"output": "Review is turned off in the user's settings, so this counts as approved. Continue."})
}

/// Response for a non-blocking review.
pub fn notified_response() -> Value {
    json!({"output": "Shown to the user. Continue without waiting for a reply."})
}

/// Terminal status for task groups still open when a prompt ends.
pub fn closing_status(reason: &super::FinishReason) -> TaskStatus {
    use super::FinishReason as F;
    match reason {
        F::Completed => TaskStatus::Completed,
        F::Cancelled | F::ToolRejected => TaskStatus::Cancelled,
        F::MaxTurns | F::LoopDetected(_) | F::Error => TaskStatus::Failed,
    }
}

/// Validate and store an artifact on a blocking thread: the file path and the store's
/// metadata (version, summary). Without a store only the checks run and `Ok(None)` is
/// returned; the content still reaches the UI through `ArtifactChanged`.
pub async fn store_artifact(store: Option<ArtifactStore>, args: &WriteArtifactArgs) -> Result<Option<(PathBuf, ArtifactMeta)>, ArtifactError> {
    validate_name(&args.name)?;
    if args.content.len() > MAX_ARTIFACT_BYTES {
        return Err(ArtifactError::TooLarge(args.content.len()));
    }
    let Some(store) = store else { return Ok(None) };
    let (name, content) = (args.name.clone(), args.content.clone());
    let summary = (!args.summary.trim().is_empty()).then(|| args.summary.clone());
    tokio::task::spawn_blocking(move || {
        let meta = store.write_versioned(&name, &content, summary.as_deref())?;
        let path = store.path(&name)?;
        Ok((path, meta))
    })
    .await
    .map_err(|e| ArtifactError::Io(std::io::Error::other(e.to_string())))?
    .map(Some)
}

/// Plan, artifact and task-group state of one agent.
///
/// Artifacts and the plan approval live as long as the agent (they span prompts); task groups,
/// the one-shot mode override and the completion flag are reset by [`Self::begin_prompt`].
#[derive(Debug, Clone, Default)]
pub struct OrchestrationState {
    artifacts: BTreeMap<String, Tracked>,
    plan_written: bool,
    plan_approved: bool,
    tasks: Vec<TaskBoundary>,
    mode_override: Option<AgentMode>,
    /// Result of `task_complete`, with the history length it was set at.
    completed: Option<(usize, String)>,
    /// Artifact metadata to hand back after an edit/retry rollback (see [`Self::roll_back`]).
    pending_restore: Vec<ArtifactMeta>,
}

/// One artifact plus the history position of the turn that wrote it.
#[derive(Debug, Clone)]
struct Tracked {
    meta: ArtifactMeta,
    /// History length when the writing turn started: everything before it is older.
    at: usize,
}

impl OrchestrationState {
    /// Start a prompt. `Some(Planning)` is the `/plan` one-shot: read-only until approval.
    pub fn begin_prompt(&mut self, mode_override: Option<AgentMode>) {
        self.tasks.clear();
        self.completed = None;
        self.mode_override = mode_override;
    }

    pub fn effective_mode(&self, configured: AgentMode) -> AgentMode {
        self.mode_override.unwrap_or(configured)
    }

    /// The current prompt came from `/plan`.
    pub fn strict_plan(&self) -> bool {
        self.mode_override == Some(AgentMode::Planning)
    }

    /// An implementation plan was written and is not approved yet.
    pub fn plan_pending(&self) -> bool {
        self.plan_written && !self.plan_approved
    }

    pub fn plan_approved(&self) -> bool {
        self.plan_approved
    }

    /// Reason to refuse a workspace-changing tool call right now, if any. `policy.mode` must
    /// already be the effective mode of this prompt.
    pub fn mutation_block(&self, policy: &ApprovalPolicy) -> Option<String> {
        if policy.mode == AgentMode::Fast {
            // Fast mode acts directly; the exception is a plan that is waiting for review
            // (switching the mode chip mid-run must not bypass it).
            return self.plan_pending().then(|| {
                "Refused: the implementation plan is waiting for review. Call request_review with artifacts [\"implementation_plan\"] and wait for approval before changing files or running commands.".to_owned()
            });
        }
        if self.strict_plan() && !self.plan_approved {
            return Some("Refused: the user asked for a plan first. Use read and search tools only, write the implementation_plan artifact and call request_review. Changes are allowed after approval.".into());
        }
        if policy.edits_blocked_by_review(self.plan_pending()) {
            return Some("Refused: the implementation plan is waiting for review. Call request_review with artifacts [\"implementation_plan\"] and wait for approval before changing files or running commands.".into());
        }
        None
    }

    /// `/plan` prompts always wait for the user on the plan, whatever the review policy.
    pub fn forces_plan_review(&self, names: &[String]) -> bool {
        self.strict_plan() && self.covers_plan(names)
    }

    /// Insert or replace a task group by name; returns the stored state.
    pub fn update_task(&mut self, tb: TaskBoundary) -> TaskBoundary {
        match self.tasks.iter_mut().find(|t| t.task_name == tb.task_name) {
            Some(t) => *t = tb.clone(),
            None => self.tasks.push(tb.clone()),
        }
        tb
    }

    pub fn tasks(&self) -> &[TaskBoundary] {
        &self.tasks
    }

    /// Change the status of the latest open task group (used around a review pause).
    pub fn set_current_status(&mut self, status: TaskStatus) -> Option<TaskBoundary> {
        let t = self.tasks.iter_mut().rev().find(|t| is_open(t.status))?;
        if t.status == status {
            return None;
        }
        t.status = status;
        Some(t.clone())
    }

    /// Close a review that is still pending (Stop while the loop waited, or a turn that ended
    /// before its answer): the artifacts stop showing "Needs review".
    pub fn cancel_reviews(&mut self) {
        for t in self.artifacts.values_mut() {
            if t.meta.review == ReviewState::Pending {
                t.meta.review = ReviewState::None;
            }
        }
    }

    /// Reopen the groups that `task_complete` closed in this prompt, because the turn goes on
    /// (the user steered more work in). Groups that already ended for good stay as they are.
    pub fn reopen_tasks(&mut self) -> Vec<TaskBoundary> {
        let mut out = Vec::new();
        for t in self.tasks.iter_mut().rev() {
            if t.status != TaskStatus::Completed {
                break;
            }
            t.status = TaskStatus::InProgress;
            out.push(t.clone());
        }
        out
    }

    /// Give every open group a terminal status; returns the changed groups.
    pub fn close_open_tasks(&mut self, status: TaskStatus) -> Vec<TaskBoundary> {
        self.tasks
            .iter_mut()
            .filter(|t| is_open(t.status))
            .map(|t| {
                t.status = status;
                t.clone()
            })
            .collect()
    }

    pub fn has_artifact(&self, name: &str) -> bool {
        self.artifacts.contains_key(name)
    }

    /// Take the store's metadata for an artifact written now (the store is the single version
    /// counter: an identical write keeps its version). A new plan version resets the approval.
    /// `history_len` is where the writing turn starts in the history, for [`Self::roll_back`]
    /// (an artifact rewritten in that turn is dropped with it).
    pub fn apply_meta(&mut self, meta: ArtifactMeta, history_len: usize) -> ArtifactMeta {
        let mut meta = meta;
        if let Some(prev) = self.artifacts.get(&meta.name) {
            meta.review = prev.meta.review;
        }
        if meta.kind.is_reviewable() {
            self.plan_written = true;
            self.plan_approved = false;
        }
        // Keep the position of the turn that first created the artifact only if it was
        // written before this turn; an artifact rewritten in this turn belongs to this turn.
        let at = history_len;
        self.artifacts.insert(meta.name.clone(), Tracked { meta: meta.clone(), at });
        meta
    }

    /// Undo the orchestration effects of the prompts that were dropped from the history: a
    /// retried or edited prompt must not inherit artifacts, versions, review states, a plan
    /// approval or task groups from the turn it replaces.
    ///
    /// `kept` is the length of the remaining history. Completion markers remember the history
    /// length they were set at, so a completion of a kept turn survives and a completion of a
    /// dropped turn does not. Artifacts written after the kept history are removed from memory
    /// and reported by [`Self::take_pending_artifacts`] so the store can delete them from disk.
    pub fn roll_back(&mut self, kept: usize) {
        self.pending_restore.clear();
        let (dropped, kept_artifacts): (Vec<_>, Vec<_>) = std::mem::take(&mut self.artifacts).into_iter().partition(|(_, t)| t.at >= kept);
        self.artifacts = kept_artifacts.into_iter().collect();
        self.pending_restore = dropped.into_iter().map(|(_, t)| t.meta).collect();
        let plan = self.plan_artifacts();
        self.plan_written = plan.iter().any(|(at, _)| *at < kept);
        self.plan_approved = plan.iter().any(|(at, r)| *at < kept && *r == ReviewState::Approved) && self.plan_written;
        self.tasks.clear();
        self.mode_override = None;
        if self.completed.as_ref().is_some_and(|(at, _)| *at >= kept) {
            self.completed = None;
        }
    }

    /// `(written_at, review)` of every reviewable artifact.
    fn plan_artifacts(&self) -> Vec<(usize, ReviewState)> {
        self.artifacts.values().filter(|t| t.meta.kind.is_reviewable()).map(|t| (t.at, t.meta.review)).collect()
    }

    /// Metadata a rolled-back prompt left behind, to be undone in the artifact store (by the
    /// backend on `TurnDone`, since the store is blocking file I/O).
    pub fn take_pending_artifacts(&mut self) -> Vec<ArtifactMeta> {
        std::mem::take(&mut self.pending_restore)
    }

    pub fn set_review(&mut self, names: &[String], state: ReviewState) {
        for n in names {
            if let Some(t) = self.artifacts.get_mut(n) {
                t.meta.review = state;
            }
        }
    }

    /// `true` when a review of `names` covers the implementation plan. Only naming a reviewable
    /// artifact counts: a free-form question (empty `artifacts`) never approves or reviews the
    /// plan, even while it is pending.
    pub fn covers_plan(&self, names: &[String]) -> bool {
        names.iter().any(|n| ArtifactKind::from_stem(n).is_reviewable())
    }

    /// The user (or the policy) approved a review of `names`.
    pub fn approve(&mut self, names: &[String]) {
        let plan = self.covers_plan(names);
        self.set_review(names, ReviewState::Approved);
        if plan && self.plan_written {
            self.plan_approved = true;
            for t in self.artifacts.values_mut().filter(|t| t.meta.kind.is_reviewable()) {
                t.meta.review = ReviewState::Approved;
            }
        }
    }

    pub fn artifacts(&self) -> Vec<ArtifactMeta> {
        self.artifacts.values().map(|t| t.meta.clone()).collect()
    }

    /// Restore metadata saved with a conversation.
    pub fn restore_artifacts(&mut self, metas: Vec<ArtifactMeta>) {
        self.artifacts = metas.into_iter().map(|m| (m.name.clone(), Tracked { meta: m, at: 0 })).collect();
        let plan = self.plan_artifacts();
        self.plan_written = !plan.is_empty();
        self.plan_approved = plan.iter().any(|(_, r)| *r == ReviewState::Approved);
    }

    /// `task_complete` result, tagged with the history length it applies to.
    pub fn complete(&mut self, result: String, history_len: usize) {
        self.completed = Some((history_len, result));
    }

    pub fn completed(&self) -> Option<&str> {
        self.completed.as_ref().map(|(_, r)| r.as_str())
    }

    /// Undo a completion because the user steered more work into the turn.
    pub fn reopen(&mut self) {
        self.completed = None;
    }
}

fn is_open(s: TaskStatus) -> bool {
    matches!(s, TaskStatus::InProgress | TaskStatus::WaitingForUser)
}

/// Index of the last real user prompt: a user content with text and no function responses.
pub fn last_prompt_index(history: &[Content]) -> Option<usize> {
    history.iter().rposition(|c| {
        c.role == Some(Role::User)
            && c.parts.iter().all(|p| p.function_response.is_none())
            && c.parts.iter().any(|p| p.text.as_deref().is_some_and(|t| !t.is_empty()))
    })
}

/// Text of the last user prompt, for resend. Multiple text parts are joined with a blank
/// line: `Content::text` concatenates them, which glued an injected steer onto the prompt in
/// histories written before steering moved into the function-response contents.
pub fn resend_text(c: &Content) -> String {
    let parts: Vec<&str> = c.parts.iter().filter(|p| !p.is_thought()).filter_map(|p| p.text.as_deref()).filter(|t| !t.is_empty()).collect();
    parts.join("

")
}

/// Where an edit/resend of the last prompt cuts the transcript: the index of the last
/// `ChatItem::User` (the bubble that is replaced). Item-based, not content-based, so the UI
/// cuts the same place the backend does, whatever steering was merged into later contents.
pub fn edit_cut(items: &[crate::ui::ChatItem]) -> Option<usize> {
    items.iter().rposition(|i| matches!(i, crate::ui::ChatItem::User(_)))
}

/// Split `history` for edit/resend: the contents before the last user prompt, and its text.
pub fn truncate_for_resend(history: &[Content]) -> Option<(Vec<Content>, String)> {
    let i = last_prompt_index(history)?;
    Some((history[..i].to_vec(), resend_text(&history[i])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_mode_has_no_plan_tools() {
        let names: Vec<_> = declarations(false).into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec![NOTIFY_USER, TASK_COMPLETE]);
        assert_eq!(declarations(true).len(), 6);
        assert!(is_available(RUN_WORKFLOW, true));
        assert!(!is_available(RUN_WORKFLOW, false), "run_workflow is planning only");
    }

    #[test]
    fn task_boundary_parses_defaults() {
        let t = parse_task_boundary(&json!({"task_name":"Read code","summary":"x"})).unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);
        assert!(parse_task_boundary(&json!({"task_name":"  "})).is_err());
    }

    fn meta(name: &str, version: u32, review: ReviewState) -> ArtifactMeta {
        ArtifactMeta {
            name: name.into(),
            kind: ArtifactKind::from_stem(name),
            summary: String::new(),
            version,
            review,
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn a_free_form_question_never_approves_the_plan() {
        let mut st = OrchestrationState::default();
        st.apply_meta(meta("implementation_plan", 1, ReviewState::None), 0);
        assert!(st.plan_pending());
        // `request_review` without artifacts is a question; proceeding to it must not lift the gate.
        st.approve(&[]);
        assert!(st.plan_pending(), "an empty artifact list does not cover the plan");
        assert!(!st.covers_plan(&[]));
        assert!(st.covers_plan(&["implementation_plan".into()]));
        st.approve(&["implementation_plan".into()]);
        assert!(st.plan_approved() && !st.plan_pending());
    }

    #[test]
    fn a_pending_plan_blocks_edits_even_in_fast_mode() {
        let mut st = OrchestrationState::default();
        st.begin_prompt(None);
        st.apply_meta(meta("implementation_plan", 1, ReviewState::None), 0);
        let mut policy = ApprovalPolicy::default();
        assert!(st.mutation_block(&policy).is_some(), "planning mode blocks");
        // The user switches the mode chip to Fast while the review is still pending.
        policy.mode = AgentMode::Fast;
        assert!(st.mutation_block(&policy).is_some(), "switching modes must not bypass the gate");
        st.approve(&["implementation_plan".into()]);
        assert!(st.mutation_block(&policy).is_none(), "Fast mode acts once the plan is approved");
        st.apply_meta(meta("implementation_plan", 2, ReviewState::None), 5);
        assert!(st.mutation_block(&policy).is_some(), "a new plan version re-arms the gate");
    }

    #[test]
    fn the_store_version_is_the_single_counter() {
        let mut st = OrchestrationState::default();
        // An identical write keeps version 1 (what the store reports).
        assert_eq!(st.apply_meta(meta("task", 1, ReviewState::None), 0).version, 1);
        let second = st.apply_meta(meta("task", 1, ReviewState::None), 0);
        assert_eq!(second.version, 1, "no in-memory bump on an identical write");
        assert_eq!(st.apply_meta(meta("task", 2, ReviewState::None), 0).version, 2);
    }

    #[test]
    fn roll_back_drops_what_the_dropped_turn_wrote() {
        let mut st = OrchestrationState::default();
        // Simulate turn 1 history at positions 0..2.
        st.apply_meta(meta("task", 1, ReviewState::None), 0);
        st.apply_meta(meta("implementation_plan", 1, ReviewState::Approved), 0);
        st.update_task(TaskBoundary { task_name: "A".into(), status: TaskStatus::Completed, summary: String::new(), phase: TaskPhase::Executing });
        // Turn 2 starts at history position 2.
        st.apply_meta(meta("walkthrough", 1, ReviewState::None), 2);
        st.apply_meta(meta("implementation_plan", 2, ReviewState::None), 2);
        assert!(st.plan_pending(), "the newer unapproved plan is pending");
        // Retry of turn 2: history is cut back to the user turn at position 2. The plan was
        // rewritten in that turn, so the revision is dropped too and the plan goes back to
        // unapproved; the task list from turn 1 stays.
        st.roll_back(2);
        let mut names: Vec<_> = st.artifacts().into_iter().map(|m| m.name).collect();
        names.sort();
        assert_eq!(names, vec!["task".to_string()]);
        assert!(!st.plan_pending() && !st.plan_approved(), "no plan is left, so no stale approval");
        assert!(st.tasks().is_empty(), "task groups are cleared by roll_back");
        let mut restored: Vec<_> = st.take_pending_artifacts().into_iter().map(|m| m.name).collect();
        restored.sort();
        assert_eq!(restored, vec!["implementation_plan".to_string(), "walkthrough".to_string()]);
        assert!(st.take_pending_artifacts().is_empty(), "taken once");
        // Retrying turn 1 as well drops everything.
        st.roll_back(0);
        assert!(st.artifacts().is_empty() && !st.plan_approved());
        assert_eq!(st.take_pending_artifacts().len(), 1);
    }

    #[test]
    fn reopen_tasks_only_undoes_the_completion() {
        let mut st = OrchestrationState::default();
        let b = |name: &str, status: TaskStatus| TaskBoundary { task_name: name.into(), status, summary: String::new(), phase: TaskPhase::Executing };
        st.update_task(b("A", TaskStatus::Failed));
        st.update_task(b("B", TaskStatus::InProgress));
        st.update_task(b("C", TaskStatus::InProgress));
        st.close_open_tasks(TaskStatus::Completed);
        let reopened = st.reopen_tasks();
        assert_eq!(reopened.iter().map(|t| t.task_name.as_str()).collect::<Vec<_>>(), vec!["C", "B"]);
        assert_eq!(st.tasks()[0].status, TaskStatus::Failed, "a failed group is not reopened");
    }

    #[test]
    fn resend_text_does_not_glue_parts_together() {
        let mut c = Content::user_text("rename a");
        c.parts.push(crate::api::Part::text("and add tests"));
        assert_eq!(resend_text(&c), "rename a

and add tests");
        assert_eq!(c.text(), "rename aand add tests", "Content::text concatenates; resend must not");
    }

    #[test]
    fn cancel_reviews_clears_pending_only() {
        let mut st = OrchestrationState::default();
        st.apply_meta(meta("implementation_plan", 1, ReviewState::Pending), 0);
        st.apply_meta(meta("walkthrough", 1, ReviewState::Approved), 0);
        st.cancel_reviews();
        let states: Vec<_> = st.artifacts().into_iter().map(|m| (m.name, m.review)).collect();
        assert_eq!(
            states,
            vec![("implementation_plan".to_string(), ReviewState::None), ("walkthrough".to_string(), ReviewState::Approved)]
        );
        assert!(st.plan_pending(), "the plan is still unapproved");
    }
}
