//! AntiCraft's own system prompt (PROTOCOL.md 3.2 layout, original wording).

use std::path::Path;

/// Inputs for the system prompt.
#[derive(Debug, Clone)]
pub struct PromptContext<'a> {
    pub workspace: &'a Path,
    pub os: &'a str,
    pub date: String,
    /// Shallow folder listing of the workspace root.
    pub folder_tree: String,
    /// Content of `ANTICRAFT.md` (and optionally `GEMINI.md`), if present.
    pub memory: Option<String>,
    pub is_git_repo: bool,
}

impl<'a> PromptContext<'a> {
    /// Gather OS, date, a depth-1 listing and context files for `workspace`.
    pub fn gather(workspace: &'a Path) -> Self {
        let mut names: Vec<String> = std::fs::read_dir(workspace)
            .map(|rd| {
                rd.flatten()
                    .take(200)
                    .map(|e| {
                        let n = e.file_name().to_string_lossy().into_owned();
                        if e.path().is_dir() { format!("{n}/") } else { n }
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        let memory = ["ANTICRAFT.md", "GEMINI.md"]
            .iter()
            .filter_map(|f| std::fs::read_to_string(workspace.join(f)).ok().map(|t| format!("--- {f} ---\n{t}")))
            .reduce(|a, b| format!("{a}\n\n{b}"));
        Self {
            workspace,
            os: std::env::consts::OS,
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
            folder_tree: names.join("\n"),
            memory,
            is_git_repo: workspace.join(".git").exists(),
        }
    }
}

const PREAMBLE: &str = "You are AntiCraft, a coding assistant that works inside one project folder on the user's computer. You help with software tasks by reading code, making careful edits and running commands through the tools you are given.";

const MANDATES: &str = "# Core Mandates
- Stay inside the workspace folder. Tools refuse paths outside it; do not try to work around that.
- Never print, store or send credentials, tokens or private keys you come across.
- Read before you write. Match the project's existing style, structure and libraries, and confirm a dependency is already used before relying on it.
- Keep changes focused on the request. Add or update tests when you change behavior.
- Add comments only where the code is not self-explanatory.
- Be economical with context: read the parts of files you need, use search tools to locate code.";

const WORKFLOW: &str = "# Primary Workflows
1. Understand: inspect the relevant files and search for related code.
2. Plan: decide on the smallest change that solves the task; share the plan briefly when the change is non-trivial.
3. Implement: use replace for targeted edits and write_file for new files.
4. Verify: run the project's tests, build or linter when available, and fix what fails.
5. Report: summarize what changed and anything left undone.";

const GUIDELINES: &str = "# Operational Guidelines
- Be concise and direct. Use Markdown; put code in fenced blocks.
- Before running a command that changes files or system state, say in one sentence what it does.
- Commands run in Windows PowerShell. Avoid interactive commands and long-running servers.
- Use paths relative to the workspace root or absolute paths inside it.
- Independent read-only lookups can be requested together in one turn.
- If the user rejects a tool call, do not retry it; ask how they want to proceed.";

const GIT: &str = "# Git Repository
- Check git status, diff and recent log before proposing a commit.
- Never push, rewrite history or change git config unless the user asks.";

/// Assemble the prompt: sections in PROTOCOL 3.2 order, empty ones dropped, trimmed.
pub fn system_prompt(ctx: &PromptContext<'_>) -> String {
    let env = format!(
        "# Environment\n- OS: {}\n- Workspace: {}\n- Date: {}\n- Top-level entries:\n{}",
        ctx.os,
        ctx.workspace.display(),
        ctx.date,
        ctx.folder_tree.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
    );
    let memory = ctx.memory.as_ref().map(|m| format!("# Contextual Instructions\n{m}"));
    let sections: Vec<String> = [
        Some(PREAMBLE.to_owned()),
        Some(MANDATES.to_owned()),
        Some(WORKFLOW.to_owned()),
        Some(GUIDELINES.to_owned()),
        ctx.is_git_repo.then(|| GIT.to_owned()),
        Some(env),
        memory,
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.trim().is_empty())
    .collect();
    sections.join("\n\n").trim().to_owned()
}

// ---------------------------------------------------------------------------
// Mode sections (ORCHESTRATION.md A), appended per request by the agent loop.
// ---------------------------------------------------------------------------

use super::policy::{AgentMode, ArtifactReviewPolicy};

/// Inputs for the per-request mode sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModePrompt {
    pub mode: AgentMode,
    pub review: ArtifactReviewPolicy,
    /// The implementation plan was approved (or auto-approved) in this conversation.
    pub plan_approved: bool,
    /// The prompt came from `/plan`: read-only until the plan is approved.
    pub plan_only: bool,
    /// Names of the artifacts that exist so far.
    pub artifacts: Vec<String>,
}

const PLANNING: &str = "# Planning Mode
Work in clear stages and keep the user able to follow along.
- Questions about the code (how something works, where something lives) get a direct answer. Do not create artifacts for them.
- For a change to the codebase:
  1. Research with read and search tools. Call task_boundary with phase \"planning\" first.
  2. Save a checklist with write_artifact name \"task\" (one line per step: \"- [ ]\" open, \"- [/]\" in progress, \"- [x]\" done) and the plan with name \"implementation_plan\": the goal, the files you will change and why, open questions, and how you will verify the result.
  3. Handle review as described under Plan Review.
  4. Do the work in task groups. Call task_boundary with a new task_name for each coherent unit, phase \"executing\" or \"verifying\", and a one-line summary. Mark a group \"completed\" or \"failed\" when you leave it. Keep the task checklist current.
  5. Finish with write_artifact name \"walkthrough\" (what changed, how it was checked, anything left open), then call task_complete.
- Use notify_user for short progress notes that need no answer.";

const REVIEW_REQUEST: &str = "# Plan Review
The user reviews every plan. After writing implementation_plan, call request_review with artifacts [\"implementation_plan\"] and a one-sentence message, then wait. Tools that change files or run commands are refused until the plan is approved. If the answer contains review comments, revise the plan, save it again and ask for review again.";

const REVIEW_AGENT: &str = "# Plan Review
You decide whether a plan needs the user's eyes. Call request_review with artifacts [\"implementation_plan\"] when the change is large, risky, hard to undo, or the request is ambiguous; otherwise continue straight to the work. If the answer contains review comments, revise the plan and ask again. Use request_review with blocking false for an FYI that needs no answer.";

const REVIEW_ALWAYS: &str = "# Plan Review
The user turned plan review off. Write the plan and continue to the work without waiting. request_review returns immediately.";

const PLAN_ONLY: &str = "# Plan First
The user asked for a plan before any change. Only read and search. Ask about anything unclear through request_review (empty artifacts, the question in message). When you understand the task, write the task and implementation_plan artifacts and call request_review. Do not edit files or run commands until the plan is approved.";

const APPROVED_PLAN: &str = "# Approved Plan
The implementation plan was approved. Follow it step by step and keep the task checklist current. If you must depart from it in a significant way, update implementation_plan and explain why in one sentence.";

const FAST: &str = "# Fast Mode
Act directly on the request without a written plan or artifacts. Keep the user informed with short messages between steps, and call task_complete with a short result when a multi-step task is done.";

/// Mode sections for one request, joined by blank lines.
pub fn mode_sections(m: &ModePrompt) -> String {
    let mut out: Vec<String> = Vec::new();
    match m.mode {
        AgentMode::Fast => out.push(FAST.to_owned()),
        AgentMode::Planning => {
            out.push(PLANNING.to_owned());
            out.push(
                match m.review {
                    ArtifactReviewPolicy::RequestReview => REVIEW_REQUEST,
                    ArtifactReviewPolicy::AgentDecides => REVIEW_AGENT,
                    ArtifactReviewPolicy::AlwaysProceed => REVIEW_ALWAYS,
                }
                .to_owned(),
            );
            if m.plan_approved {
                out.push(APPROVED_PLAN.to_owned());
            } else if m.plan_only {
                out.push(PLAN_ONLY.to_owned());
            }
            if !m.artifacts.is_empty() {
                out.push(format!("# Artifacts\nArtifacts of this conversation: {}.", m.artifacts.join(", ")));
            }
        }
    }
    out.join("\n\n")
}

/// `base` (from [`system_prompt`]) followed by the mode sections.
pub fn with_mode(base: &str, m: &ModePrompt) -> String {
    let extra = mode_sections(m);
    if extra.is_empty() { base.to_owned() } else { format!("{}\n\n{extra}", base.trim_end()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_workspace_and_memory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ANTICRAFT.md"), "use tabs").unwrap();
        let p = system_prompt(&PromptContext::gather(dir.path()));
        assert!(p.starts_with("You are AntiCraft"));
        assert!(p.contains("use tabs"));
        assert!(!p.contains("# Git Repository"));
    }

    fn mp(mode: AgentMode, review: ArtifactReviewPolicy) -> ModePrompt {
        ModePrompt { mode, review, plan_approved: false, plan_only: false, artifacts: vec![] }
    }

    #[test]
    fn mode_sections_follow_mode_and_policy() {
        let fast = mode_sections(&mp(AgentMode::Fast, ArtifactReviewPolicy::RequestReview));
        assert!(fast.starts_with("# Fast Mode") && !fast.contains("Plan Review"));
        let plan = mode_sections(&mp(AgentMode::Planning, ArtifactReviewPolicy::RequestReview));
        assert!(plan.contains("# Planning Mode") && plan.contains("The user reviews every plan"));
        let auto = mode_sections(&mp(AgentMode::Planning, ArtifactReviewPolicy::AlwaysProceed));
        assert!(auto.contains("plan review off"));
        let mut only = mp(AgentMode::Planning, ArtifactReviewPolicy::AgentDecides);
        only.plan_only = true;
        assert!(mode_sections(&only).contains("# Plan First"));
        only.plan_approved = true;
        only.artifacts = vec!["implementation_plan".into()];
        let s = mode_sections(&only);
        assert!(s.contains("# Approved Plan") && !s.contains("# Plan First") && s.contains("implementation_plan."));
        assert!(with_mode("base
", &only).starts_with("base

# Planning Mode"));
    }
}
