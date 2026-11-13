//! `/` command popup (ORCHESTRATION.md section D.6).
//!
//! Opens when the input starts with `/` and has no whitespace yet. Lists the built-in
//! commands followed by workflows (workspace shadows global), filtered by prefix then fuzzy
//! match. Built-ins act locally or send a dedicated `AppCommand`; a workflow expands into a
//! prompt that carries the workflow body.

use crate::agent::context::{fuzzy_score, Workflow, WorkflowScope};

/// Built-in commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// Force Planning mode for this one prompt.
    Plan,
    /// Force Fast mode for this one prompt.
    Fast,
    /// New conversation.
    New,
    /// Clear the transcript of the current conversation (history reset, same workspace).
    Clear,
    /// Summarize older history now.
    Compact,
}

pub const BUILTINS: &[(Builtin, &str, &str)] = &[
    (Builtin::Plan, "plan", "Plan first: research, write a plan and wait for review"),
    (Builtin::Fast, "fast", "Act directly without plan artifacts"),
    (Builtin::New, "new", "Start a new conversation"),
    (Builtin::Clear, "clear", "Clear this conversation's history"),
    (Builtin::Compact, "compact", "Summarize older messages to free context"),
];

/// Prompt sent for a bare `/plan` (no goal given): open-ended discovery.
pub const BARE_PLAN_PROMPT: &str =
    "Look around the workspace first. Then ask me a few short questions about what we should plan, \
     offering concrete options where you can, before writing any plan.";

impl Builtin {
    /// Takes free text after the command (`/plan <goal>`); choosing it in the popup inserts
    /// `/name ` instead of running it.
    pub fn takes_args(self) -> bool {
        matches!(self, Builtin::Plan | Builtin::Fast)
    }

    /// Commands that make no sense while a turn is running.
    pub fn needs_idle(self) -> bool {
        true
    }
}

/// One popup row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashItem {
    Builtin(Builtin),
    Workflow { name: String, description: String, scope: WorkflowScope },
}

impl SlashItem {
    pub fn name(&self) -> &str {
        match self {
            SlashItem::Builtin(b) => BUILTINS.iter().find(|x| x.0 == *b).map(|x| x.1).unwrap_or(""),
            SlashItem::Workflow { name, .. } => name,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            SlashItem::Builtin(b) => BUILTINS.iter().find(|x| x.0 == *b).map(|x| x.2).unwrap_or(""),
            SlashItem::Workflow { description, .. } => description,
        }
    }

    /// Short right-aligned tag in the popup.
    pub fn tag(&self) -> &'static str {
        match self {
            SlashItem::Builtin(_) => "",
            SlashItem::Workflow { scope: WorkflowScope::Workspace, .. } => "workspace",
            SlashItem::Workflow { scope: WorkflowScope::Global, .. } => "global",
        }
    }
}

/// Parsed `/command rest` from a submitted prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashInvocation<'a> {
    pub name: &'a str,
    pub rest: &'a str,
}

/// Split `/name rest`; `None` when the text is not a command.
pub fn parse(input: &str) -> Option<SlashInvocation<'_>> {
    let s = input.trim_start().strip_prefix('/')?;
    let (name, rest) = s.split_once(char::is_whitespace).unwrap_or((s, ""));
    (!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then_some(SlashInvocation { name, rest: rest.trim() })
}

pub fn builtin(name: &str) -> Option<Builtin> {
    BUILTINS.iter().find(|x| x.1 == name).map(|x| x.0)
}

/// Active popup query: the input starts with `/` and has no whitespace before `caret`.
/// Returns the text between `/` and the caret.
pub fn active_query(input: &str, caret: usize) -> Option<&str> {
    let head = input.get(..caret)?;
    let q = head.strip_prefix('/')?;
    (!head.contains(char::is_whitespace)).then_some(q)
}

/// Popup rows for `query` (text after `/`).
pub fn items(query: &str, workflows: &[Workflow]) -> Vec<SlashItem> {
    let mut v: Vec<(i32, SlashItem)> = Vec::new();
    for (b, name, _) in BUILTINS {
        if let Some(s) = fuzzy_score(query, name) {
            v.push((s + if name.starts_with(query) { 100 } else { 0 }, SlashItem::Builtin(*b)));
        }
    }
    for w in workflows {
        if builtin(&w.name).is_some() {
            continue; // built-ins win over a workflow with the same name
        }
        if let Some(s) = fuzzy_score(query, &w.name) {
            let item = SlashItem::Workflow { name: w.name.clone(), description: w.description.clone(), scope: w.scope };
            v.push((s + if w.name.starts_with(query) { 100 } else { 0 }, item));
        }
    }
    // Stable sort: equal scores keep built-ins first, then workflow order.
    v.sort_by_key(|x| std::cmp::Reverse(x.0));
    v.into_iter().map(|(_, i)| i).collect()
}

/// Prompt text sent for a workflow invocation.
pub fn workflow_prompt(w: &Workflow, rest: &str) -> String {
    let mut s = format!("Follow the workflow \"/{}\" below.\n\n{}\n", w.name, w.body.trim());
    if !rest.is_empty() {
        s.push_str(&format!("\nAdditional input: {rest}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wf(name: &str) -> Workflow {
        Workflow { name: name.into(), description: "d".into(), body: "b".into(), scope: WorkflowScope::Global, path: "x".into() }
    }

    #[test]
    fn parse_and_filter() {
        assert_eq!(parse("/plan add tests"), Some(SlashInvocation { name: "plan", rest: "add tests" }));
        assert_eq!(parse("not a command"), None);
        assert_eq!(parse("/ x"), None);
        let w = vec![wf("deploy")];
        let it = items("p", &w);
        assert_eq!(it[0], SlashItem::Builtin(Builtin::Plan));
        assert!(items("dep", &w).iter().any(|i| i.name() == "deploy"));
        assert_eq!(items("", &w).len(), BUILTINS.len() + 1);
    }

    #[test]
    fn builtin_shadows_workflow() {
        let it = items("plan", &[wf("plan")]);
        assert_eq!(it.iter().filter(|i| i.name() == "plan").count(), 1);
        assert!(matches!(it[0], SlashItem::Builtin(Builtin::Plan)));
    }

    #[test]
    fn query_detection() {
        assert_eq!(active_query("/pl", 3), Some("pl"));
        assert_eq!(active_query("/", 1), Some(""));
        assert_eq!(active_query("/plan x", 7), None);
        assert_eq!(active_query(" /plan", 6), None);
        assert_eq!(active_query("a/b", 3), None);
    }

    #[test]
    fn workflow_expansion() {
        let p = workflow_prompt(&wf("deploy"), "staging");
        assert!(p.contains("/deploy") && p.contains("b") && p.ends_with("Additional input: staging\n"));
        assert!(Builtin::Plan.takes_args() && !Builtin::New.takes_args());
    }
}
