# Agent orchestration and chat control

Spec for how anticraft-native plans, runs and pauses agent work, and how the user steers
the chat. Behavior follows the publicly documented behavior of Google Antigravity
(antigravity.google/docs: artifacts, implementation-plan, walkthrough, artifact-review,
agent-settings, permissions, settings, slash-commands, rules, cli/*). Names match the public
docs where they exist. All wording, prompts, schemas and code here are original. Mechanics
also reuse ideas from google-gemini/gemini-cli (Apache-2.0: plan mode, `write_todos`,
subagents, confirmation state machine), reimplemented in Rust. See NOTICE.

Status: modules and types exist as compiling skeletons (section G). Wiring into the agent
loop, backend and UI is follow-up work. Behavior that is already live is marked (live).

## Naming notes

| Concept | Docs name | AntiCraft name | Notes |
|---|---|---|---|
| Conversation modes | Planning / Fast | `AgentMode::{Planning, Fast}` | exact |
| Plan pause setting | artifact review policy: "Request review", "Always proceed" | `ArtifactReviewPolicy::{RequestReview, AgentDecides, AlwaysProceed}` | `AgentDecides` is ours; the docs have only two values |
| Terminal setting | legacy "Terminal command auto execution": Require Review / Proceed in Sandbox / Always Proceed; preset "Turbo" | `TerminalPolicy::{Off, Auto, Turbo}` | no sandbox on Windows 10, so "Proceed in Sandbox" maps to `Auto` (safe simple commands only); labels "Always ask", "Auto", "Turbo" |
| Allow/deny lists | allow / ask / deny rules, `command(prefix)`, `regex:` | `terminal_allowlist`, `terminal_denylist` | deny > ask > allow; token-prefix or `regex:` |
| Send while busy | Queued Messages: "Queue" / "Send Immediately" | `QueueMode::{Queue, SendImmediately}` | label "Steer" in the chip menu |
| Plan approval | Proceed / Review (comments) | Proceed / Request changes | |
| Artifacts | Implementation Plan, Walkthrough, Task (implied) | `task.md`, `implementation_plan.md`, `walkthrough.md` | docs give no storage path or task-list file name |
| Task progress | task groups (overview, progress updates, pending steps) | task group card fed by `task_boundary` | docs give no status enum; ours below |

Unknowns (not in public docs, chosen originally): plan section schema, task status enum,
artifact storage path, the `task.md` name, the compaction threshold, editing past messages,
a Stop button in the App, folder mentions.

## A. Agent modes

- **Planning** (default). Flow per prompt:
  1. Research: read-only tools plus `task_boundary` (phase `planning`).
  2. Write `task.md` (checklist) and `implementation_plan.md` with `write_artifact`.
  3. Under `RequestReview`, call `request_review` on the plan and wait. Under
     `AgentDecides`, the agent asks only for non-trivial or risky plans. Under
     `AlwaysProceed`, it never waits.
  4. Execute in task groups (`task_boundary` per unit, phase `executing` / `verifying`),
     ticking `task.md` items (`[ ]`, `[/]`, `[x]`).
  5. Write `walkthrough.md` (what changed, how it was verified), then `task_complete`.
  Inquiries ("how does X work") get a direct answer with no artifacts.
- **Fast**: act directly. Only `notify_user` and `task_complete` are declared; review
  policy does not apply.
- Mode chip in the composer, next to the model chip, set per conversation
  (`AppCommand::SetAgentMode`), default from `Settings.agent_mode`.
- `/plan <prompt>` and `/fast <prompt>` force a mode for one prompt
  (`AppCommand::SendPromptWithMode`).
- The system prompt is rebuilt per request from mode, policy and rules, so switching modes
  does not drop the agent (and its session approvals).

### System prompt sections (to write in `agent/prompt.rs`)
- `PLANNING`: describe the five steps, the artifact names and the checklist syntax, plus
  "one task group per coherent unit; keep summaries to one line".
- `REVIEW_<policy>`: when to call `request_review`, and that a "changes requested" response
  means revise the plan and ask again.
- `FAST`: act directly, keep the user informed with short messages, no artifacts.
- `RULES`: injected rules files (section D.7) under a heading with their paths.
- `APPROVED_PLAN`: after approval, "follow `implementation_plan.md`; update `task.md`".

## B. Orchestration tools (`agent/orchestrate.rs`)

Intercepted in `Agent::run_one` before the `ToolRegistry`; they never touch the workspace
and need no confirmation.

| Tool | Args | Effect | Response to model |
|---|---|---|---|
| `task_boundary` | `task_name`, `summary`, `status?` (`in_progress`, `waiting_for_user`, `completed`, `failed`), `phase?` (`planning`, `executing`, `verifying`) | `AgentEvent::TaskUpdate` | `{"output":"ok"}` |
| `write_artifact` | `name` (`^[a-z0-9_-]{1,64}$`), `content` (<= 256 KiB), `summary?` | writes `<data-dir>/conversations/<id>/artifacts/<name>.md`, version + 1, `AgentEvent::ArtifactChanged` | path + version |
| `request_review` | `message`, `artifacts?: [name]`, `blocking?` | `AgentEvent::ReviewRequested` with a oneshot; the loop awaits it (cancel-aware) | approved / comments / dismissed (`ReviewOutcome::to_response`) |
| `notify_user` | `message` | `AgentEvent::Notify` | ok |
| `task_complete` | `result` | `AgentEvent::TaskComplete`; the loop ends after this turn | ok |

Rules:
- Status `cancelled` is never sent by the model. Stop sets it on open groups.
- `ReviewOutcome::Cancelled` ends the turn like a rejected tool (`FinishReason::ToolRejected`)
  through `seal_history`.
- `ChangesRequested` is returned as the function response, so the loop continues.
- Writes happen on the turn task (`spawn_blocking`). Metadata (`ArtifactMeta`) reaches the
  backend through `TurnDone` so `Backend::turn_finished` stays the single persistence
  writer. `ConversationStore::delete` also removes the artifact directory.
- Status enum (ours): `in_progress`, `waiting_for_user`, `completed`, `failed`, `cancelled`.
  Todo semantics follow gemini-cli `write_todos` (at most one item in progress).

## C. Policies (`agent/policy.rs`, settings, Customizations > Agent)

New `Settings` fields (serde default, clamped in `sanitized()`): `agent_mode`,
`artifact_review`, `terminal_policy`, `terminal_allowlist`, `terminal_denylist`,
`queued_messages`, `compact_threshold`. The existing `auto_approve_read_tools` stays.

`ApprovalPolicy::decide(kind, details) -> Decision {Allow, Ask, Deny(reason)}` replaces the
`needs_confirm` expression in `run_one`:
- Shell: denylist -> `Deny` (always wins). Compound command (`is_simple_command` false) ->
  `Ask`. Allowlist -> `Allow`. Then `Off` -> `Ask`, `Auto` -> `Allow` only for
  `SAFE_COMMANDS` roots, `Turbo` -> `Allow`.
- Edits: always `Ask` unless a session "Always allow" applies. Edit approvals keep passing
  `ConfirmationDetails` so hash pinning (`check_approved`) still works.
- Read/search: `Allow` when `auto_approve_read_tools`, else `Ask`.
- Rule syntax: token prefix (`git push` matches `git push origin`) or `regex:<pattern>`
  anchored `^(?:...)$`, case-insensitive.
- A policy change takes effect at the next request through a shared handle (like
  `SharedModel`), without dropping session approvals.

Customizations > Agent UI: segmented controls for Mode, Review policy and Terminal policy,
two list editors (add / edit / delete rows, validated on Enter), the existing read-tools
toggle, and Queued Messages.

## D. Chat control

1. **Send while busy.** Enter while busy follows `queued_messages`:
   - `Queue`: `AppCommand::QueuePrompt`. The backend keeps a `VecDeque`. On a normal finish
     (`Completed`) it pops the next one and runs it. After Stop or a rejection the queue is
     kept, not run. `AppEvent::Queue` mirrors it as chips above the composer
     (`ui/queue.rs`), with remove and "send now".
   - `SendImmediately` (steer): `AppCommand::SteerPrompt`. The text goes through a channel
     into the running turn. After `run_calls` and before the next `stream_one`, it is
     appended as an extra text part in the same functionResponse user content (alternation
     stays intact). `AgentEvent::SteerInjected` shows it as a user bubble. If the turn has
     no more tool calls, it becomes the next prompt instead.
   - Alt+Enter sends one message with the other behavior.
2. **Stop** (live: cancels the stream and tools). Adds: open task groups become `cancelled`,
   a pending review is answered `Cancelled`, queued messages stay.
3. **Edit and resend** the last user message (pencil on the last bubble, idle only).
   `AppCommand::EditLastPrompt` truncates history to before that user content and sends the
   new text.
4. **Retry** (`AppCommand::Retry`): the same truncation, then resend the same text. Replaces
   today's "Send again".
5. **@ mentions** (`ui/mentions.rs`, `agent/context.rs`). `@query` after whitespace opens a
   popup of workspace files and folders ranked by `fuzzy_score`. The index comes from
   `AppCommand::IndexWorkspace` -> walkdir in `spawn_blocking`, honoring `.gitignore`-like
   skips (`target`, `node_modules`, `.git`), capped at 20k entries -> `AppEvent::WorkspaceIndex`.
   Choosing inserts `@[label](rel/path)`. Before sending, the agent resolves tokens through
   `Workspace::resolve` (escapes rejected): files are attached as context (size-capped),
   folders as a depth-1 listing.
6. **/ commands** (`ui/slash.rs`). The popup opens on a leading `/`. Built-ins: `/plan`,
   `/fast`, `/new`, `/clear`, `/compact`. Then workflows from
   `<workspace>/.agents/workflows/*.md` and `<data-dir>/workflows/*.md` (workspace shadows
   global, built-ins win), showing the frontmatter `description`. A workflow expands into a
   prompt containing its body plus any extra text.
7. **Rules** (`context::load_rules`): `AGENTS.md`, `GEMINI.md`, `ANTICRAFT.md` at the
   workspace root plus `.agents/rules/*.md` (top level, sorted). 24 KiB per file and 64 KiB
   total; files past the budget are skipped. Injected under the `RULES` prompt section.
8. **Bypass Mode** (`settings.bypass`, `agent/bypass.rs`, `ui/bypass.rs`): while enabled and
   naming a provider profile, the first `requests` model requests of each user message go to
   that profile's backend and model instead of the conversation's own; the rest of the message
   goes back to the normal backend. The count restarts each message. The route appends its
   `.md` instruction files to those requests' system instruction only (never the history, so
   the normal model never receives them) and rebuilds the generation config for its own model.
   The route is read once when the agent is built (`bypass_route()` in `app.rs`), so switching
   the mode mid-turn affects the next message, not the one in flight; a settings change that
   touches the route or its profile rebuilds the agent (`agent_inputs_changed`). A route whose
   profile has no model fails the message before any request rather than silently using the
   normal backend. `compaction` continues to run on the normal backend. The composer carries a
   red band naming the provider, its model, the count and the model that takes over.
9. **Compaction**: when the last `promptTokenCount` reaches `compact_threshold` (default 0.7)
   of the model limit, or on `/compact`, older contents are summarized by one extra model
   call into a single user/model pair. The last `COMPACT_KEEP_TURNS` user turns, the
   approved plan path and the artifact list are kept. `AgentEvent::Compacted` shows a notice.

## E. UI

1. **Task group cards** (`ui/taskgroups.rs`): title, status dot and label, step lines (the
   latest is bright), edited-file pills, and a collapsible "N tool calls" list. Tool cards
   that follow a boundary belong to it, and so do pending confirmations. New
   `ChatItem::TaskGroup` (to add).
2. **Artifacts side panel** (`ui/artifacts_panel.rs`): a right panel toggled from the
   conversation header. It lists artifacts with versions, renders the selected one as
   Markdown, and has a comment box per artifact. While a review is pending it shows a
   review bar: **Proceed** and **Request changes (N)**. Comments go back through
   `ReviewOutcome::ChangesRequested`. Outside a pending review, comments are sent as a user
   turn (`comments_to_feedback`).
3. **Pending review banner** above the composer: "Plan ready for review", with Open, Proceed
   and Request changes. Enter in an empty composer does not approve.
4. **File change list**: edits made in the conversation (`AppEvent::FileChanges`, +/- counts
   and a new-file tag) with per-file accept, or reject (`ResolveFileChange`, restoring the
   pre-conversation baseline that the backend snapshots on first edit).
5. Confirmation queue: `PendingConfirm` becomes a FIFO, so a second request no longer
   cancels the first.

## F. Fixes

1. **Centered column** (done): `Col::for_width` centers a column of at most 700 px with a
   16 px minimum gutter. User bubbles render inside the column, right-aligned to its edge.
   The header and composer dock follow `col`. The column makes room for an open side panel
   only, never for a turn's progress: that is in the Progress panel (`ui/progress_panel.rs`).
2. **Vietnamese input** (partly done). Code reading found no reordering in winit, egui-winit
   or egui. Done:
   - `ANTICRAFT_KEYTRACE=1` (debug builds only, since it records typed text) logs event
     order from `raw_input_hook`.
   - `sanitize_injected_keys` drops phantom `Key` presses caused by `VK_PACKET` scancodes
     (for example space -> `Key::D`) so they cannot trigger shortcuts.
   If the trace shows the space arriving after the `c`, the Unikey/EVKey mode is the cause
   and the fix is a tool setting. Otherwise add a Win32 `with_msg_hook` trace next.

## G. Skeleton modules

| File | Contents |
|---|---|
| `src/agent/orchestrate.rs` | tool names, `TaskBoundary`, `TaskStatus`, `TaskPhase`, `WriteArtifactArgs`, `RequestReviewArgs`, `ReviewOutcome`, `ReviewRequest`, `declarations(planning)`, `parse_task_boundary` |
| `src/agent/artifacts.rs` | `ArtifactKind`, `ReviewState`, `ArtifactMeta`, `ArtifactComment`, `comments_to_feedback`, `ArtifactStore`, `validate_name` |
| `src/agent/policy.rs` | `AgentMode`, `ArtifactReviewPolicy`, `TerminalPolicy`, `Decision`, `ApprovalPolicy::decide`, `rule_matches`, `SAFE_COMMANDS` |
| `src/agent/context.rs` | `load_rules`, `load_workflows`, `split_frontmatter`, `Mention`, `parse_mentions`, `fuzzy_score`, `should_compact`, `CompactionPlan` |
| `src/ui/taskgroups.rs` | `TaskGroup`, `status_label`, `show` |
| `src/ui/artifacts_panel.rs` | `ArtifactView`, `ArtifactsPanel`, `PanelAction`, `show` |
| `src/ui/mentions.rs` | `MentionPopup`, `active_query`, `matches`, `insert` |
| `src/ui/slash.rs` | `Builtin`, `BUILTINS`, `SlashItem`, `parse`, `items`, `workflow_prompt` |
| `src/ui/queue.rs` | `QueueMode`, `QueuedPrompt`, `ChipAction`, `show` |

## Implementation order

1. Policy (`decide` in `run_one`, shared handle) plus the Customizations > Agent tab.
2. Orchestration tools, artifact store and per-request system prompt sections.
3. Review pause (`ReviewRequested`), the review bar and the banner.
4. Task group cards and the side panel.
5. Queue/steer, edit/retry.
6. Mentions, slash commands, rules, workflows.
7. Compaction and the file change list.

Each step gets scripted fake-model tests (no real credentials): plan -> review -> proceed;
changes requested round-trip; denylist denies; queue runs after `Completed` only; steer keeps
user/model alternation; cancel during review resolves without blocking the UI.
