# AntiCraft UI design spec (egui)

Target: match the layout, spacing, colors and interaction model of the reference shots in `_ref/ui`.
All code and art are original. No Google logos or images, and no "Antigravity" string: the product name is "AntiCraft".
Icons come from an MIT icon font (`egui-phosphor`, Regular weight; pin the release that matches egui 0.37).

Sources: geometry and colors come from `_ref/ui/main.json` (computed DOM styles).
`main.json` was captured at a viewport about 1842px wide. Every x position below has been rebased to the 1400x900 reference PNGs: sidebar coordinates are fixed, and content coordinates are centered in the area right of the sidebar.
Screens with no DOM dump (history, customizations, model menu, conversation details) reuse the same tokens. Their geometry is an inferred approximation, so verify it by comparing screenshots side by side.

## 1. Design tokens

### Colors (dark only; the Light theme setting may stay but is not styled)
| Token | Value | Use |
|---|---|---|
| bg | `#101010` | window / central panel |
| fg | `#cccccc` | primary text, selected nav text |
| fg-secondary | `#9d9d9d` (cc 75% over bg) | inactive nav rows, toolbar icons, composer chips |
| fg-muted | `#6e6e6e` (cc 50% over bg) | menu bar items, project headers, relative time, "See all", hover icons, "Local" chip |
| placeholder | `#474747` (cc 30% over bg) | composer and search placeholder |
| primary | `#007acc` | focus ring, toggles on, selected radio, settings links |
| link | `#6a9fcb` | markdown links |
| code-fg | `#d7ba7d` | inline code text |
| success | `#7aae66` | tool done check, diff additions |
| warning | `#f3c949` | quota banner |
| error | `#de5555` | tool error, auth banner, diff removals |
| sidebar | `#141414` (white 2.5% over bg) | sidebar panel fill |
| card | `#1c1c1c` (white 5% over bg) | composer card, tool cards, popups, code blocks |
| card-border | `#262626` (cc 5% over card) | 1px card stroke |
| border | `rgba(255,255,255,0.05)` (about `#1b1b1b` on bg) | panel separators, selected-row stroke |
| secondary | `#2f2f2f` (white 12% over bg) | button hover fill, active chip, user bubble |
| row-selected | `#323232` | selected nav row / active conversation |
| row-hover | `rgba(255,255,255,0.04)` | hover on any sidebar or list row |
| group-header | `#161616` | project group header row fill |
| send-bg | `#2d2d2d` | send button when empty/disabled; `#cccccc` fill with `#101010` arrow when ready |
| tooltip | `#252526` | tooltip fill |

### Typography
Font: Segoe UI is loaded at runtime from `C:\Windows\Fonts\segoeui.ttf` (do not bundle it), falling back to the egui default. CSS px maps to egui points at 1:1 for pixels-per-point 1, so tune it until the result matches the PNGs at 1400x900.
| Role | Size / weight |
|---|---|
| nav row label | 14 / regular |
| section header ("Projects", date groups) | 12 / medium |
| menu bar items | 13 / medium, fg-muted |
| conversation row title | 13.5 / regular, fg |
| relative time, project subtitle | 12 / regular, fg-muted |
| "See all (N)" | 12.5 / regular, fg-muted |
| composer text | 14 / regular |
| chips (model, Review, Local) | 12 / regular |
| conversation title (header) | 14 / medium |
| body markdown | 14 / regular, line height about 1.5 |
| code | monospace 12.5 |
| page titles (History, Customizations) | 20 / semibold |

### Radii
nav row 8; icon buttons 6 (20-24px) or 8 (28px); menu buttons 6; composer card 16; tool/file cards 10; code blocks 8; popups 10; user bubble 12; chips and composer round buttons are pills (radius = h/2).

### Spacing and sizes
- Sidebar width 256 (fixed). Inner horizontal padding 8, so rows are 240 wide.
- Nav row 240x32, row pitch 38 (gap 6). Icon 16 at x+10, label at x+34.
- Conversation row 240x32, pitch 33 (1px gap).
- Icon buttons: 20x20 in the section header, 24x24 in rows, 28x28 in toolbars.
- Composer and conversation column max width 768 (48rem); artifacts 896.
- Card padding 12; message vertical gap 16; section gap 24.

## 2. Regions at 1400x900

### 2.1 Title / menu bar (y 0-34)
- Keep the native OS title bar. Under it, a 34px top panel with a bg fill.
- Left: text buttons "AntiCraft", "File", "View", "Window" at x 8 / 102 / 148 / 202, y 3, h 28, radius 6, 13/medium fg-muted. Hover gives a secondary fill and fg text.
- Menus:
  - AntiCraft: About, Sign out.
  - File: New Conversation (Ctrl+N), Open Workspace..., Customizations, Quit.
  - View: Toggle Sidebar (Ctrl+B).
  - Window: Minimize.

### 2.2 Toolbar row (y 37-73)
- x 8, y 43: sidebar toggle 38x24, icon `SidebarSimple`.
- 28x28 icon buttons at x 50 / 82 / 114 / 146 (radius 8, fg-secondary): back `ArrowLeft`, forward `ArrowRight`, search `MagnifyingGlass` (opens History with search focused), new `NotePencil`. Back and forward walk an in-memory stack of views; when the stack is at its end, the button is disabled.
- Right edge: `DotsThree` 28x28 at x 1280, then the "Open IDE" button 78x28 at x 1312 (13/medium, 1px border, radius 6). Visual only: disabled with tooltip "Coming soon".

### 2.3 Sidebar (x 0-256, y 73-900, fill `sidebar`, right stroke `border`)
Nav rows (y from window top, as in main.json):
| y | label | icon (phosphor) | behavior |
|---|---|---|---|
| 87 | New Conversation | `NotePencil` | opens Home; selected style while Home is active |
| 125 | Conversation History | `ClockCounterClockwise` | opens History page |
| 163 | Automations | `Lightning` | visual only: disabled with tooltip "Coming soon" |
| 201 | Customizations | `SlidersHorizontal` | opens Customizations page |

- Row states:
  - Selected: `row-selected` fill, 1px `border` stroke, fg text.
  - Unselected: transparent fill, fg-secondary text.
  - Hover: `row-hover` fill, fg text.
  - Disabled: 40% alpha, no hover fill.
- Projects header at y 262 (x 16, h 16): "Projects" 12/medium at 50% opacity. Clicking it collapses or expands the whole Projects section, and a `CaretDown`/`CaretRight` caret shows on hover. Header buttons are 20x20, fg-muted:
  - x 196, `Plus`: pick a workspace folder (async rfd), then remember it.
  - x 220, `ArrowsInLineVertical`: collapse/expand all groups.
- Project group (one per `workspace_history` entry, current workspace first):
  - Group header row, 240x32:
    - Fill `group-header`, `Folder`/`FolderOpen` icon, folder name 13 fg-muted. Click toggles the group.
    - Buttons 24x24 at x 192 (`DotsThree`: Open in Explorer, Remove from list) and x 220 (`NotePencil`: new conversation in this workspace).
  - Conversation rows at pitch 33:
    - The title is truncated with an ellipsis. The relative time ("now", "5m", "3h", "2d", "5mo") is right-aligned in fg-muted.
    - Hover hides the time and shows three 24x24 icons at x 168 / 194 / 220:
      - `PushPin`: visual only, disabled with tooltip "Coming soon" unless the store gains a pinned flag.
      - `PencilSimple`: inline rename.
      - `Trash`: delete, with an inline confirm.
    - The active conversation has a `row-selected` fill. A streaming conversation shows a small spinner in place of the time.
  - At most 7 rows per group, then "See all (N)" (x 30, w 218, h 32, 12.5 fg-muted), which opens History filtered to the project.
- The sidebar body scrolls vertically, with a thin 6px scrollbar shown on hover.
- Bottom (pinned, about y 852-892): account row with `UserCircle`, the truncated email, and a `GearSix` button that opens Customizations. When signed out, it is a "Sign in" pill (primary fill).

### 2.4 Home view (no active conversation)
Content area x 256-1400 (width 1144, center x 828).
- Workspace chip above the card (about y 366, aligned to the card's left - 5): 14 fg-muted, folder name plus `CaretDown`. It opens a menu of `workspace_history` plus "Open folder...".
- Composer card: x 444-1212 (w 768), vertically centered (top about y 404), `card` fill, 1px `card-border`, radius 16, padding 12.
  - Text area: multiline, min height 40, grows to 200 and then scrolls. Placeholder "Ask anything, @ to mention, / for workflows" in placeholder color. Enter sends; Shift+Enter inserts a newline (existing `is_send_key`).
  - `@` opens a mention popup of workspace files (fuzzy filter); it inserts `@path`. `/` opens a popup showing "No workflows yet" (visual only).
  - Bottom row, h 28, inside the card padding:
    - Left: `Plus` round 28x28 (attach: async file picker that inserts `@path`).
    - Model chip about 142x28: pill, 12 fg-secondary, label plus `CaretDown`. It opens the model picker (2.6).
    - Right: "Request Review"-style chip 130x28 with `Eye`. Visual only: disabled with tooltip "Coming soon".
    - `Microphone` 28x28. Visual only: disabled with tooltip "Coming soon".
    - Send 28x28 pill with `ArrowUp`: `send-bg` when empty, light fill when ready. While busy it becomes `Stop` (sends the existing cancel command).
- Below the card (+8 y, +6 x): "Local" chip 67x24, pill, 12 fg-muted, `Desktop` icon. It is informational only; the tooltip shows the workspace path.

### 2.5 Conversation view
- Header (toolbar row, content side):
  - Title at x 272, 14/medium fg. Click to rename inline.
  - Then `DotsThree` (Rename, Copy as Markdown, Delete) and the disabled "Open IDE" button.
  - At the window edge, the panel toggle (`SidebarSimple`, filled while open) opens the Progress panel (`ui/progress_panel.rs`): a 300 px right panel listing the current turn's tool steps (a spinner while running, then a check or a cross), or its task groups in Planning mode. Nothing about progress is drawn beside the column, so the column never narrows while tools run.
  - A 1px `border` line appears under the header only when scrolled.
- Message column: max width 768, centered, top padding 24.
- "Load older messages" text button (12 fg-muted) at the top when more than 200 items exist. It reveals 200 more.
- User message:
  - Right-aligned bubble, max 80% of the column: `secondary` fill, radius 12, padding 10x14, 14 fg, plain text with newlines kept.
  - Hover shows a `Copy` icon under it.
- Assistant turn: no bubble, full column width, markdown via egui_commonmark.
  - Progress header above the text: "Working..." with a spinner while streaming, then "Worked for 1m". Style 12 fg-muted with a `CaretRight`/`CaretDown` caret.
    Expanded, it lists thought subjects (existing merge-by-subject) and tool steps as compact rows.
  - Thought section: collapsed by default. The summary line is "Thought for Ns" or the subject; the body is italic 13 fg-secondary.
  - Tool card: `card` fill, 1px `card-border`, radius 10, 36 h when collapsed. Click to expand the output (monospace 12, max height 240, scrolls).
    - Icon by tool:

      | Tool | Icon |
      |---|---|
      | read_file | `FileText` |
      | list_directory | `FolderOpen` |
      | glob, grep_search | `MagnifyingGlass` |
      | write_file | `FilePlus` |
      | replace | `PencilSimpleLine` |
      | run_shell_command | `TerminalWindow` |
    - Label: tool verb plus the first line of the args (paths in monospace 12.5).
    - Status at the right: spinner (running), `Check` in success, `X` in error, `Prohibit` (denied).
  - File card (write_file/replace): the same frame with the filename and `+N -M` in success/error. Expanding shows a unified diff with line backgrounds at 15% alpha.
  - Code block: `card` fill, radius 8. A 28 h header strip carries the language (12 fg-muted) and a `Copy` button; the body is monospace 12.5.
  - Hover footer: `Copy`, `ArrowClockwise` (retry the last prompt).
- Confirmation (existing confirm.rs logic): an inline card in the stream instead of a modal. `card` fill and a 1px stroke in primary at 40%.
  - Title "Run command?" or "Apply edit?", then the command or diff preview.
  - Buttons: "Allow once" (primary fill), "Always allow" (secondary), "Deny" (ghost). Enter allows once; Esc denies.
- Docked composer: the same card as Home (w 768, centered), 16 from the bottom, with a 24px bg fade above it.
  - The view sticks to the bottom while streaming unless the user scrolled up. In that case a "Jump to latest" `ArrowDown` pill appears above the composer.
- Banners (quota/auth) sit at the top of the column: radius 8, 10% alpha warning/error fill, text plus an action ("Sign in again", "Switch to Flash").

### 2.6 Model picker popup
- Opens upward from the model chip (8 gap): w 300, `card` fill, 1px `card-border`, radius 10, padding 6, shadow 0 8 24 rgba(0,0,0,0.5).
- Header "Model" 12/medium fg-muted.
- Rows 32 h, radius 6, hover `secondary`. Each row has a label (13 fg) and an optional sub-label (12 fg-muted, the resolved backend id). The selected row has a `Check` on the right.
- Entries: Auto / Pro / Flash / Flash Lite aliases, then a separator, then `MODEL_CHOICES`.
  - Preview models carry a "Preview" pill tag (11, primary at 20% fill).
  - When `preview_access` is off, preview models are disabled with the tooltip "Enable preview access in Customizations".
- Closes on selection, Esc, or an outside click. The choice persists through the existing settings command.

### 2.7 Conversation History page
- Column max width 768, centered, top padding 32. Title "Conversation History" 20/semibold.
- Search box: h 36, `card` fill, 1px `card-border`, radius 8, `MagnifyingGlass` icon, placeholder "Search conversations". It filters title and project case-insensitively as you type and is focused on open.
- Project filter chips (pills, 12): "All" plus one chip per workspace. The active chip has a `secondary` fill.
- Date group headers (12/medium, fg-muted at 50%): Today, Yesterday, Previous 7 days, Previous 30 days, then month names ("September 2026").
- Rows 44 h, radius 8, hover `row-hover`. Click opens the conversation.
  - Line 1: title, 13.5 fg.
  - Line 2: project name and relative time, 12 fg-muted.
  - Hover actions: `PencilSimple`, `Trash`.
- Empty state: centered `ChatsCircle` 32 and the text "No conversations yet" in fg-muted.

### 2.8 Customizations page (replaces the settings.rs window)
- Column max width 768. Title "Customizations" 20/semibold.
- Tab bar: tabs 32 h, 13/medium fg-muted; the active tab has fg text and a 2px fg underline. Tabs: General, Agent, Workspaces, Account.
- Settings rows are grouped in cards (`card` fill, radius 10, 1px `border` dividers, row padding 12x16). The label (13.5 fg) and description (12 fg-muted) sit on the left, the control on the right.
- General:
  - Default model: dropdown with the picker entries.
  - Preview model access: toggle (`preview_access`). Description: "Turn on if your account can use preview models. AntiCraft does not detect this."
  - Latest Flash GA: toggle (`latest_flash_ga`).
  - Theme: segmented Dark / Light.
- Agent:
  - Auto-approve read/search tools: toggle (`auto_approve_read_tools`).
  - Shell timeout (seconds): drag value 5-3600 (`shell_timeout_secs`).
  - Shell output cap (KB): drag value (`shell_output_cap_bytes`).
  - Session "Always allow" rules: each rule has a `Trash` button. When there are no rules, show an empty state.
- Tools > Bypass Mode (after the Terminal section, before Plugins):
  - Section "Bypass Mode", warning-colored description: the first requests of each message go
    to a provider the user picks, then the conversation returns to the model in use; the
    provider sees those requests.
  - "Use Bypass Mode" toggle (`settings.bypass.enabled`).
  - "Bypass provider": a pill + menu of the provider profiles, each row its label and model,
    the chosen one checked; "Add provider..." opens the Providers tab's dialog on this tab and
    the profile it saves becomes the mode's provider.
  - "Requests before switching back": drag value 1-`MAX_BYPASS_REQUESTS`.
  - "Instruction files": one row per `.md` path with an `X`, then "Add .md file..." (native
    picker, `AppCommand::PickBypassFiles`). The text is read at turn build and rides in the
    bypass requests' system instruction only, never the transcript.
  - With no profiles, the section says a provider profile is needed.
- Workspaces:
  - The `workspace_history` list, up to `WORKSPACE_HISTORY_LIMIT` entries.
  - Each row shows the path and a "Current" tag, with an `ArrowSquareOut` button (Explorer) and an `X` button (remove).
  - An "Open folder..." button sits at the bottom.
- Account:
  - Email, plus "Sign out" (ghost button with error-colored text).
  - GCP project: text field (`gcp_project`).
  - OAuth client secret: the status line reads "Set via ANTICRAFT_OAUTH_CLIENT_SECRET" with `CheckCircle`, or "Missing" in error color with help text. The value is never shown and never saved to settings.json.
- Bypass Mode warning band (`ui/bypass.rs`): while the mode routes, every composer (conversation
  dock and Home) carries a red band naming the provider and its model, the request count and the
  model that takes over, with a "Turn off" link. Not dismissible.
- Changes apply immediately (no Save button) and persist through the existing save path.
- Toggles: pill 36x20; primary track when on, `#3a3a3a` when off.

## 3. Interaction states
- Hover: `secondary`/`row-hover` fill, text one step brighter (muted to secondary, secondary to fg), pointer cursor, about a 120 ms fade (`animate_bool`).
- Pressed: `#3a3a3a` fill.
- Selected: `row-selected` fill + `border` stroke (sidebar), or a `Check` icon (menus).
- Keyboard focus: 1px primary ring.
- Disabled: 40% alpha on text and icon, no hover fill, default cursor, with a tooltip ("Coming soon" or the specific reason).
- Busy: send becomes Stop. The composer stays editable, but send is blocked until the turn ends.
- Tooltip: `tooltip` fill, 1px `card-border`, radius 6, 12 fg, 150 ms delay.

## 4. Visual only (no AntiCraft backend)
Each of these is disabled with the tooltip "Coming soon":
- Automations nav row
- Open IDE button
- Request Review chip
- Microphone / voice input
- `/` workflows
- Pin conversation (unless the store adds a pinned flag)

## 5. Implementation notes
- `ui/theme.rs`: `const Color32` tokens, font setup, and the `Style`/`Visuals` overrides (corner radius, spacing, selection = primary).
- `ui/widgets.rs`: `nav_row`, `icon_button`, `pill_chip`, `toggle`, `card_frame`, and `relative_time(DateTime) -> String` (unit tested).
- `enum View { Home, Conversation(id), History, Customizations }` with a back/forward stack in `UiState`. The existing AppEvent/AppCommand plumbing stays unchanged.
- UI thread stays non-blocking: folder/file pickers use `rfd::AsyncFileDialog` on the tokio runtime, and the result returns as an AppEvent.
- Verify with offscreen snapshots of fixture conversations only. Never sign in or call Google APIs in tests.

## 6. Data mapping
Source: read-only audit of `src/` (UI model, AppCommand/AppEvent, AgentEvent, store, settings, auth). Each gap below has a resolution and an owner file. "Done" items exist in the current code; "Planned" items are not built yet.

### 6.1 Rules kept
- The UI thread only drains events with `try_recv` and pushes `AppCommand`s. Nothing blocks it.
- `UiState::apply` stays a pure reducer, tested without egui. Widgets emit `AppCommand` directly; there is no separate `UiAction` type.
- Persisted model IDs (`MODEL_CHOICES`, aliases) and the settings/conversation file formats are unchanged. New stored fields must use `#[serde(default)]`.

### 6.2 UiState (owner: `ui/mod.rs`), done
| Field | Purpose |
|---|---|
| `view: View{Chat, History, Customizations}`, plus `back`/`forward` stacks | Page routing; replaces `show_settings`. Home is not a separate variant. It is `View::Chat` with no items (`is_home()`), so sending from Home flows straight into the conversation. |
| `sidebar_open`, `projects_collapsed`, `collapsed_groups: HashSet<String>` | Sidebar toggles (Ctrl+B). Groups are keyed by `group_key(workspace)`. |
| `expanded: HashSet<String>` | Expanded tool cards and thoughts, and collapsed turn headers. Keys are `tool{idx}`, `thought{idx}` and `turn{idx}`, cleared when a conversation loads. |
| `history_query`, `history_project`, `focus_search` | History page filter; the search icon focuses the box. |
| `customize_tab`, `settings_dirty` | Customizations edit `settings` in place. `AntiCraftApp::ui` flushes them with `SaveSettings` once no pointer is down and no text field is focused, and again in `on_exit`. |
| `confirm_delete: Option<Uuid>` | Inline delete confirmation (check / x) in sidebar and history rows. |
| `visible_items` (pages of `ITEM_PAGE` = 200) | "Load older messages" on the client side. The whole history is already in memory. |
| `turn_started: Option<Instant>` | Set in `take_prompt`. On `Finished` it fills the turn header duration. |
| `last_usage: Option<UsageMetadata>` | Kept from `AgentEvent::Usage`, so it is no longer dropped. It is not displayed yet. |
| `running: Option<Uuid>` | Set by `TurnEvent`, cleared on `Finished` or `TurnRejected`. Drives the sidebar spinner. |

`ChatItem::TurnHeader{duration: Option<Duration>}` is pushed by `take_prompt` after the user item. It shows "Working..." while live and "Worked for 1m" after `Finished`. `TurnRejected` removes it. If the backend creates the conversation (`ConversationLoaded` with empty messages while `awaiting_turn`), the optimistic items are kept. `Conversations` refreshes `current_title`, so the toolbar shows the generated title.

### 6.3 Pure helpers (owner: `ui/format.rs`), done and unit tested
- `relative_time(t, now)`: now / 5m / 3h / 2d / 2w / 5mo / 2y.
- `duration_text`.
- `project_label(path)`: `file_name()`, falling back to the full path for roots.
- `group_by_project(list, workspace_history)`: groups ordered by newest `updated_at`. Known but empty workspaces are appended, and the "Other" group (no workspace) comes last.
- `date_bucket`: Today / Yesterday / Previous 7 days / Previous 30 days / month / month and year.
- `matches_filter`.
- `ModelChoice{id, label, note, preview, alias}` and `model_choices()`: four alias rows, then each concrete `MODEL_CHOICES` ID with a derived label ("Gemini 3.1 Pro"). Alias rows show `resolve_model(..)` as the subtitle. Preview rows are disabled when `preview_access` is off.

### 6.4 Gaps and resolutions
| Gap (audit) | Resolution | Owner | Status |
|---|---|---|---|
| Summary has no `created_at`, message count or preview | Not needed by any reference screen; the sidebar and history use `updated_at` only. If needed, add `message_count: usize` and `preview: String` (`#[serde(default)]`) to `ConversationSummary`, filled in `ConversationStore::list`. | `store.rs` | Not needed |
| No per-conversation running state | `UiState.running` from `TurnEvent` (6.2). | `ui/mod.rs` | Done |
| Workspace label and "Other" group | `project_label`, `OTHER_GROUP`. | `ui/format.rs` | Done |
| Recent projects on Home | Read from `settings.workspace_history` through the workspace chip menu; no new event. | `ui/composer.rs` | Done |
| (a) Thoughts not persisted | Leave as is (they are not protocol history). Reloaded conversations show no thought rows. | n/a | Accepted |
| (b) Tool `display` lost on reload | Add `Conversation.tool_displays: HashMap<String, (bool, String)>` with `#[serde(default)]`. In `send_prompt`'s forwarder (`app.rs`), collect `ToolCallResponse{call_id, ok, display}` into a map sent with `TurnDone`. `turn_finished` merges it into the conversation before `store.save`. Cap each display at 4 KB. `items_from_history` takes `&Conversation` and uses the map, falling back to "done"/"error". | `store.rs`, `app.rs`, `ui/mod.rs` | Planned |
| (c) Turn duration | `turn_started` plus `TurnHeader` (6.2). Not persisted: reloaded turns show no header. | `ui/mod.rs` | Done |
| (d) Usage dropped | `last_usage` kept. A token readout in the turn header hover is a follow-up. | `ui/mod.rs`, `ui/chat.rs` | Partly done |
| (e) No per-message timestamps | Not shown in the reference conversation view. Would need a side list `Conversation.turn_times: Vec<DateTime<Utc>>` (`#[serde(default)]`), appended in `turn_finished`. | `store.rs`, `app.rs` | Not needed |
| (f) No paging | Client-side `visible_items` (6.2); the file still loads whole. | `ui/chat.rs` | Done |
| Model display names and grouping | `ModelChoice` table (6.3); IDs are unchanged. | `ui/format.rs` | Done |
| Preview availability not detected | Still the manual `preview_access` toggle, with an explanation in Customizations > General. Detection would need a `loadCodeAssist` tier field and is out of scope. | n/a | Accepted (README) |
| Resolved model for `auto` | `resolve_model` is called from the UI for alias subtitles. | `ui/composer.rs` | Done |
| Settings sections and page | `customizations.rs` with tabs General / Agent / Workspaces / Account. The old `ui/settings.rs` window is removed. | `ui/customizations.rs` | Done |
| Session "Always allow" rules listing | `ApprovalMode` lives inside the agent. Add `AppCommand::ListApprovals` -> `AppEvent::Approvals(Vec<String>)` and `AppCommand::RevokeApproval(String)`, served by the backend without locking a running agent (snapshot via an `Arc<Mutex<..>>` shared with the agent). | `agent/mod.rs`, `app.rs`, `ui/customizations.rs` | Planned |
| Data dir display | Add `AppEvent::Paths{data_dir}`, emitted once in `Backend::run`; show it under Account. | `app.rs`, `ui/customizations.rs` | Planned |
| No `NotConfigured` auth state | For now `UiState::oauth_not_configured()` matches the "not configured" error text. This disables "Sign in" and points to Customizations > Account. The proper fix is in 6.5. | `ui/mod.rs`, `ui/login.rs` | Interim |
| Tier or project info | Out of scope (would need a `loadCodeAssist` result event). | n/a | Not needed |
| Automations | Disabled nav row with a "Coming soon" tooltip. | `ui/sidebar.rs` | Done |
| Open IDE, Request Review, microphone, pin | Visual only, disabled with a tooltip. | `ui/topbar.rs`, `ui/composer.rs`, `ui/sidebar.rs` | Done |

### 6.5 OAuth client secret configuration (planned)
1. Add `AuthStatus::NotConfigured` (owner `auth/mod.rs`). `Backend::run` emits it instead of `Error("OAuth client is not configured")`, and `oauth_not_configured()` matches the variant.
2. New `auth/client_config.rs`:
   - `StoredClient{client_secret}` stored at `<data_dir>\oauth_client.json`.
   - Written with `auth::store::atomic_write_private`, which applies an owner-only DACL.
   - A corrupt file is treated as absent, with a redacted warning.
   - `AuthConfig::load(data_dir)` resolves the secret in this order: file, runtime env, then `option_env!`. Empty values are rejected. `from_env()` stays for tests.
   - The source is reported as `ClientSource{File, Env, BuiltIn}`.
3. `app.rs`:
   - Keep `http: reqwest::Client` on `Backend`.
   - `AppCommand::SetOAuthClientSecret(String)`: trim and reject empty, write in `spawn_blocking`, rebuild `auth` and `client` with the same `http` and `ApiConfig`, `load_cached()`, then emit `Auth(status)` and `AppEvent::OAuthClientSource(Some(src))`. Reject the command while `turn_cancel` or `login_cancel` is `Some`.
   - `ClearOAuthClientSecret` deletes the file, sets `auth` and `client` to `None`, and emits `NotConfigured`.
4. UI (`ui/customizations.rs` Account tab, and the Home notice in `ui/login.rs`):
   - A masked `TextEdit::singleline(..).password(true)` bound to a UI-only buffer, plus a Save button. The buffer is cleared once the command is sent.
   - Show the source without the value.
5. Tests:
   - Precedence (file over env) using a temp dir.
   - The ACL is owner-only (icacls pattern from `auth/store.rs`).
   - `Debug` output is redacted.
   - Fixture-only backend test: not-configured -> `SetOAuthClientSecret` -> `LoggedOut`, with no network.

### 6.6 File ownership (UI rebuild)
| File | Content |
|---|---|
| `ui/theme.rs` | Tokens. Fonts: Segoe UI and Cascadia/Consolas read from `%WINDIR%\Fonts` at runtime, plus the vendored Phosphor font. Style. |
| `ui/icons.rs` | Phosphor codepoints. The font is in `assets/Phosphor-Regular.ttf` (MIT, `assets/Phosphor-LICENSE-MIT.txt`). It is vendored because `egui-phosphor` 0.14 requires egui 0.36. |
| `ui/widgets.rs` | `nav_row`, `icon_button`, `overlay_icon`, `pill_chip`, `send_button`, `toggle`, `card`, `text_link`, `paint_truncated`, `centered_column`. |
| `ui/topbar.rs` | Menu bar and toolbar (navigation, title rename, overflow menu). |
| `ui/sidebar.rs` | Nav rows, Projects grouping, row hover actions. |
| `ui/login.rs` | Account row and the Home notice. |
| `ui/chat.rs`, `ui/composer.rs`, `ui/confirm.rs` | Home/conversation, composer and model picker, inline confirmation. |
| `ui/history.rs`, `ui/customizations.rs` | Pages. |
| `ui/fixtures.rs` and `main.rs` | `--ui-fixture {home,conversation,history,customizations} [--snapshot out.png]`: offline fixture states with detached channels (no backend, network or credentials). |
