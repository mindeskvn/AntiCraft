# AntiCraft UI design spec (egui)

Target: match the layout, spacing, colors and interaction model of the Antigravity 2.31 desktop window (the 2.30 visual refresh and the 2.28-2.31 features).
All code and art are original. No Google logos or images; the product name is "AntiCraft", and "Antigravity" appears only where it names the account provider.
Icons come from the Phosphor icon font (Regular weight, MIT), vendored at `assets/Phosphor-Regular.ttf`.

Sources: colors and geometry were measured on a capture of the reference window at 1073x900 (DPR 1), kept with its notes in `_ref/ui2` outside the repository (it shows private data and is never published). The earlier DOM dump in `_ref/ui` still informs the rows and popups.
Geometry below is given for a 1400x900 window. Screens without a measurement reuse the same tokens; compare screenshots side by side when changing them.

## 1. Design tokens

### Colors (dark only; the Light theme setting is stored but not styled)
Constants in `ui/theme.rs`.
| Token | Const | Value | Use |
|---|---|---|---|
| chrome | `SIDEBAR` | `#111114` | menu bar, toolbar row, sidebar, and the gutter around the content card |
| bg | `BG` | `#070708` | the content card the screens sit on |
| edge | `EDGE` | `#19191d` | card outline, divider between panes |
| fg | `FG` | `#e3e3e5` | primary text |
| fg-secondary | `FG_SECONDARY` | `#bcbdc6` | inactive rows, toolbar icons, chips |
| fg-muted | `FG_MUTED` | `#85868d` | descriptions, project headers, relative time |
| fg-dim | `FG_DIM` | `#67686e` | "See all", breadcrumb separator, version line |
| placeholder | `PLACEHOLDER` | `#75767c` | message box and search placeholders |
| primary | `PRIMARY` | `#2f70e3` | focus ring, primary buttons, text selection |
| unread | `UNREAD` | `#2f70e3` | unread conversation dot |
| link | `LINK` | `#8ab0e8` | markdown links |
| success / warning / error | `SUCCESS` / `WARNING` / `ERROR` | `#7aae66` / `#f3c949` / `#de5555` | tool results, banners, diff lines |
| card | `CARD` | `#15151a` | raised surfaces: message box, popups, tool cards, code blocks, the Settings card |
| card-border | `CARD_BORDER` | `#232328` | 1px stroke of `card` |
| popup-border | `POPUP_BORDER` | `#2a2a30` | popup and dialog frames |
| field | `FIELD_BG` | `#101013` | text fields, segmented tracks |
| secondary | `SECONDARY` | `#25252b` | hover fill of chips, buttons and segments |
| pressed | `PRESSED` | `#303037` | pressed fill |
| row-selected | `ROW_SELECTED` / `ROW_SELECTED_EDGE` | `#1f1f24` / `#2a2a2f` | selected nav or conversation row |
| box | `BOX_FILL` / `FIELD_BORDER` | `#1c1c1f` / `#29292c` | the boxed "New Conversation" row |
| row-hover | `ROW_HOVER` | white at about 4% | hover on list rows |
| bubble | `BUBBLE` / `BUBBLE_EDGE` | `#1a1a1f` / `#24242a` | your messages |
| send-bg | `SEND_BG` | `#25252b` | send button while the box is empty |
| toggle-on | `TOGGLE_ON` | `#19b26b` | switch track when on |

### Typography
- Font (Settings > Models > Appearance > Font): System UI (Segoe UI), Inter or Google Sans. Fonts are read at runtime from the Windows font folders (all users, then the current user) and never bundled. A font that is not installed is shown disabled, and Segoe UI stays in use. Code uses Cascadia Mono, else Consolas.
- Font size (same section): 12, 14 (default) or 16 px. The sizes below are the 14 px values; the other settings scale them by 12/14 or 16/14. Icons keep their size.

| Role | Size / weight |
|---|---|
| nav and conversation rows | 13 / regular |
| "Projects" header | 12.5 / regular, fg-muted |
| menu bar items | 13 / medium, fg-muted |
| breadcrumb title | 13 / regular, fg-secondary |
| message box text | 14 / regular |
| chips | 12-12.5 / regular |
| body markdown | 14 / regular, line height about 1.5 |
| code | monospace 12.5 |
| Home greeting | 28 / medium, with a 15 fg-muted line under it |
| Settings title, section titles | 15.5 / medium, 17.5 / medium |

### Radii
content card 8; nav rows 6 when selected, 8 for hover and the boxed row; icon buttons 6 (under 28 px) or 8; message box 16; tool and file cards 10; code blocks 8; popups 10; Settings card 12; your message bubble 14 with a 4 px top-right corner; chips and round buttons are pills (radius = h/2).

### Spacing and sizes
- Sidebar 235 wide (fixed) with 8 px padding. Rows sit at a 30 px pitch; their text starts 27 px in.
- Content card: inset 7 px (`SHELL_GUTTER`) from the window's right and bottom edges, and from the left edge while the sidebar is hidden. Its top is the toolbar's bottom.
- Reading column (Settings > Models > Appearance > Conversation width): Default 768, Wide 960 (the default) or Fill (the whole card), always keeping a 32 px gutter on each side. Home uses the same column.
- Card padding 12; message gap 16; section gap 24.
- In a window under 1024 px wide, the sidebar and a side panel are never shown together (2.3).

## 2. Regions at 1400x900

### 2.0 Shell
- The menu bar, toolbar row and sidebar share the `chrome` fill. The screens sit on it as one rounded card (`bg` fill, 1px `edge`, radius 8).
- Panels inside the card (side panels, message box docks) have no fill of their own, so the card's rounded corners stay visible; the transcript is clipped to the card.
- The Settings overlay (2.8) is drawn over the card and the chrome, never inside the card.

### 2.1 Menu bar (y 0-32)
- Under the native OS title bar: text buttons "AntiCraft", "File", "View" and "Window" (13 / medium, fg-muted; hover gives a `secondary` fill).
- Menus:
  - AntiCraft: Settings (Ctrl+,), Sign out.
  - File: New Conversation (Ctrl+N), Open Workspace..., Conversation History, Settings (Ctrl+,), Quit.
  - View: Toggle Sidebar (Ctrl+B), Zoom In (Ctrl+=), Zoom Out (Ctrl+-), Reset Zoom (Ctrl+0).
  - Window: Minimize.

### 2.2 Toolbar row (y 32-68, icons centered at y 49)
- Left: the sidebar toggle (a drawn window-with-pane icon whose pane is filled while the sidebar shows), back and forward (an in-memory stack of views, disabled at its ends), and search (opens History with its field focused). A New Conversation icon joins them while the sidebar is hidden.
- Breadcrumb: "Project / Title". The project label is fg-muted, the `/` fg-dim, the title 13 fg-secondary; a click on the title renames the conversation. History shows its page name instead.
- Right, ending at the card's right edge (in a conversation): the Progress and Artifacts panel toggles (an amber dot on Artifacts while one waits for review), "Open IDE" (outlined; opens the workspace in VS Code, else in Explorer) and a `DotsThreeVertical` menu with Rename, Copy as Markdown and Delete.

### 2.3 Sidebar (x 0-235, `chrome` fill, 8 px padding)
- "New Conversation": a boxed row (`box` fill and border, radius 8, bright text), selected while Home shows.
- "Automations": visual only (disabled, "Coming soon").
- "Projects" header (12.5 fg-muted): a click collapses or expands the section (caret on hover); `FolderPlus` at its right opens a folder.
- One group per project (the current workspace first, then "Other"): its 5 newest conversations, plus the open one when it is older, then "See all (N)" or "Show less".
- Conversation rows (30 px pitch, title 13 starting 27 px in, truncated):
  - A mark at the right edge: a spinner while its turn runs (Working), an amber dot while it waits for an approval or a review (Action required), a blue dot with a halo when its last turn ended while it was not on screen (Unread). Hovering the row shows its title and state.
  - Hover shows pin (visual only), rename and delete; delete asks inline with a check and a cross.
- Bottom: a "Settings" row (`GearSix`) once any account is signed in, opening Settings on Models; before that, the sign-in pill, whose gear opens Settings on Account.
- In a window under 1024 px wide, opening a side panel hides the sidebar. The sidebar toggle (and Ctrl+B) then closes the side panel, which brings the sidebar back.

### 2.4 Home (an empty conversation)
- Greeting at about 20% of the card's height: "Good morning", "Good afternoon" or "Good evening" by the local hour (28 / medium), and "What should we work on in {project}?" under it (15 fg-muted).
- Starter suggestions: four pills ("Explain this project", "Find bugs", "Write tests", "Plan a feature"). A click puts the prompt in the message box and focuses it with the caret at the end; nothing is sent.
- Under them, a dismissable info card about how tools are confirmed, or the sign-in card while nothing can send a turn.
- The message box (2.6) is pinned to the bottom of the card, in the same column as a conversation's, so the first prompt does not move it. Above it: the project picker chip, and the Git branch checked out in that folder (read from `.git/HEAD`, worktrees included, every 3 s; AntiCraft never switches branches).

### 2.5 Conversation view
- The transcript starts 20 px under the card's top, in the reading column (1). It fades out above the message box, and under the card's top edge once scrolled.
- "Load older messages" text button (12 fg-muted) at the top when more than 200 items exist. It reveals 200 more.
- Your messages: right-aligned bubbles, at most 80% of the column (`bubble` fill, 1px `bubble-edge`, radius 14 with a 4 px top-right corner, 14 fg, newlines kept). Hover shows its actions under it.
- Navigator rail on the column's left edge, from two turns on: one dot per turn, the one being read brighter. Turns sent on different local days are split into chapters by a gap with a tick. Hovering a dot shows the prompt, the start of the reply and when it was sent ("Today 14:32", "Yesterday 09:10", "Nov 12, 09:10"); a click scrolls to that turn, loading older pages first. In a long conversation the dots tighten to a 5 px pitch, then the oldest give way.
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
- Side panels: Progress (300 px; the current turn's tool steps, or its task groups in Planning mode) and Artifacts open at the card's right edge, with a 1px `edge` line and no fill. Their toggles are in the toolbar (2.2). Nothing about progress is drawn beside the column, so the column never narrows while tools run.
- Docked message box: the box of 2.6, in the reading column, 14 px above the card's bottom edge.
  - The view sticks to the bottom while streaming unless the user scrolled up. In that case a "Jump to latest" `ArrowDown` pill appears above the message box.
- Banners (quota/auth) sit at the top of the column: radius 8, 10% alpha warning/error fill, text plus an action ("Sign in again", "Open Account", "Open Providers").

### 2.6 Message box and its menus
- Card: `card` fill, 1px `card-border`, radius 16, padding 12 / 10 / 12 / 8, soft shadow. Placeholder "Ask anything, @ to mention, / for actions". Enter sends; Shift+Enter adds a line; the box grows to a limit, then scrolls. `@` mentions a workspace file; `/` lists the commands.
- Controls row (28 h): `+` (attach a file, open a folder), then the model chip, which names the model and the agent mode ("Gemini 3 Flash  Planning", caret up). On the right: the permission chip, the microphone (visual only) and send (a 28 px circle: white with a dark arrow when there is text, `send-bg` when empty, a stop square while a turn runs).
- Model menu (opens upward from the chip, w 300, `card` fill, radius 10, popup shadow):
  - Agent section first: Planning (researches, writes a plan, works in task groups) and Fast (acts at once), each with a one-line note, the current one checked. It stays usable while a turn runs; the model list is then replaced by "The model can change once the turn ends".
  - Then the signed-in accounts' and the provider profiles' models, and the Code Assist models (aliases first). Rows 32 h, radius 6, hover `secondary`; the selected row has a `Check`. Preview models are disabled with "Enable preview access in Settings" while preview access is off.
  - Closes on a pick, Esc, or an outside click.
- Permission chip: how shell commands run in this conversation. Ask (`Hand`): every command asks. Auto (`ShieldCheck`): read-only and build commands run. Turbo (`Shield`): everything not denylisted runs. Its menu opens upward under "Commands in this conversation" and ends with "The default is in Settings > Tools and approvals." A pick made on Home goes to the conversation the first prompt starts. With `--dangerously-skip-permissions` the chip reads "Unrestricted".

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

### 2.8 Settings overlay
Settings is a modal card over whatever screen is open, not a page: the screen underneath keeps its state (scroll position, typed text, a running turn) and shows behind a 55% black backdrop.
- Opened from the sidebar's Settings row (Models), the sign-in pill's gear (Account), AntiCraft > Settings and File > Settings or Ctrl+, (the section shown last), the banners' "Open Account" / "Open Providers", and the model menu's "Load models..." (Providers, with that profile's dialog open and loading).
- Closed by its close button, Esc, Ctrl+, again, or any navigation (a new conversation, opening one, History). A click outside does not close it: a typed API key or secret would be lost. While the provider dialog is open, Esc closes the dialog first.
- A turn that ends while the overlay covers its conversation marks it unread; closing the overlay shows it, and the mark goes.
- Card: 1040x720, shrunk to leave 40 px at the window's edges (at least 560x360); `card` fill, 1px `popup-border`, radius 12, popup shadow.
  - Title row (52 h): "Settings" (15.5 / medium) on the left; the "Search settings" field (200x28) and a close `X` on the right; a 1px `edge` line under it. The search filters the open section's rows by label and description; "No settings match your search" when none does.
  - Section rail (200 wide, 1px `edge` on its right): Models, Tools and approvals, Workspaces, Providers, Account as nav rows, the open one selected; "AntiCraft {version}" (fg-dim) at its foot.
  - Content: the open section's rows in a column of at most 758, scrolling inside the card.
- Rows: a 30x40 accent tile (gradient, white glyph), the label (13.5 fg) and description (12 fg-muted), the control on the right. Toggles are 40x22 pills (`toggle-on` when on, `#3c3c3c` off, 16 px white knob) in a column of their own. A row's hover fill is `row-hover`.
- Models:
  - Default model (a pill with the picker's entries), Preview model access, Latest Flash GA.
  - Appearance: Theme (Light / Dark; only Dark is styled), Conversation width (Default / Wide / Fill), Font (System UI / Inter / Google Sans; fonts that are not installed are disabled and named in the description), Font size (12 / 14 / 16). Changes apply at once.
- Tools and approvals:
  - Agent: Default mode (Planning / Fast), Plan review (Request review / Agent decides / Always proceed), Messages sent while busy (Queue / Send now).
  - Tools and approvals: auto-approve read and search tools, apply file edits without asking, and a note that "Always allow" choices last for the session.
  - Terminal: Command auto-run (Off / Auto / Turbo; the default the permission chip starts from), Shell timeout (5-3600 s), Shell output cap (KB), then the Allowlist and Denylist rule editors.
  - Bypass Mode (after Terminal, before Plugins):
    - Section "Bypass Mode", warning-colored description: the first requests of each message go to a provider the user picks, then the conversation returns to the model in use; the provider sees those requests.
    - "Use Bypass Mode" toggle (`settings.bypass.enabled`).
    - "Bypass provider": a pill + menu of the provider profiles, each row its label and model, the chosen one checked; "Add provider..." opens the Providers dialog here, and the profile it saves becomes the mode's provider.
    - "Requests before switching back": drag value 1-`MAX_BYPASS_REQUESTS`.
    - "Instruction files": one row per `.md` path with an `X`, then "Add .md file..." (native picker, `AppCommand::PickBypassFiles`). The text is read at turn build and rides in the bypass requests' system instruction only, never the transcript.
    - With no profiles, the section says a provider profile is needed.
  - Plugins: one toggle per discovered plugin (its version, description, source and skills), the MCP servers' status, and "Reconnect MCP".
- Workspaces: the `workspace_history` list (up to `WORKSPACE_HISTORY_LIMIT`), each row with Use (or a "Current" tag), `ArrowSquareOut` (Explorer) and `X` (remove), then "Open folder...".
- Providers: the API-key profiles (Gemini Developer API, OpenAI-compatible, Anthropic) with Edit, Delete (click twice) and Use, then "Add provider". The Add / Edit dialog (type, label, base URL, key, "Load models", the model list) is a modal of its own, hung 90 px from the window's top.
- Account:
  - Accounts: per provider (Antigravity, ChatGPT Codex, Claude Code), its signed-in accounts (Models, Use / Current, Sign out) and its sign-in row (with "Paste redirect link" while an attempt can still be finished), then the subscription terms note.
  - Gemini CLI sign-in (Google), the session another Google tool saved, the Google Cloud project, and the OAuth client secret (status line and field; the value is never shown or written to settings.json).
- Bypass Mode warning band (`ui/bypass.rs`): while the mode routes, every message box (conversation dock and Home) carries a red band naming the provider and its model, the request count and the model that takes over, with a "Turn off" link. Not dismissible.
- Changes apply immediately (no Save button). They are flushed through `SaveSettings` once the pointer is released and no text field has focus.

## 3. Interaction states
- Hover: `secondary`/`row-hover` fill, text one step brighter (muted to secondary, secondary to fg), pointer cursor, about a 120 ms fade (`animate_bool`).
- Pressed: `pressed` fill.
- Selected: `row-selected` fill + `row-selected-edge` stroke (sidebar and the Settings rail), or a `Check` icon (menus).
- Keyboard focus: 1px primary ring.
- Disabled: 40% alpha on text and icon, no hover fill, default cursor, with a tooltip ("Coming soon" or the specific reason).
- Busy: send becomes Stop. The message box stays editable; what is sent meanwhile is queued or steers the turn (Settings > Tools and approvals > Messages sent while busy).
- Tooltip: `card` fill, 1px `popup-border`, radius 10, 12 fg, 150 ms delay.

## 4. Visual only (no AntiCraft backend)
Each of these is disabled with the tooltip "Coming soon":
- Automations nav row
- Microphone / voice input
- Pin conversation (unless the store adds a pinned flag)

## 5. Implementation notes
- `ui/theme.rs`: `const Color32` tokens, font setup, the `Style`/`Visuals` overrides, and Settings > Appearance (`set_appearance`: the text scale and column width are thread locals set every frame; fonts and text styles are rebuilt only when they change).
- `ui/widgets.rs`: `nav_row`, `icon_button`, `pill_chip`, `pill_chip_up`, `toggle`, `card`, `segmented`, `send_button`.
- `enum View { Chat, History }` with a back/forward stack in `UiState`; `Screen` (Home, Conversation, History) is derived from it. Settings is not a view: `UiState::customize_open` and `customize_tab`, opened with `open_customize(tab)` and closed with `close_customize()` or any `navigate`.
- UI thread stays non-blocking: folder/file pickers use `rfd::AsyncFileDialog` on the tokio runtime, and the result returns as an AppEvent.
- Verify with offscreen snapshots of fixture conversations (`--demo NAME --screenshot OUT.png`) and a live run that closes cleanly. Never sign in or call Google APIs in tests.

## 6. Data mapping
Source: read-only audit of `src/` (UI model, AppCommand/AppEvent, AgentEvent, store, settings, auth). Each gap below has a resolution and an owner file. "Done" items exist in the current code; "Planned" items are not built yet.

### 6.1 Rules kept
- The UI thread only drains events with `try_recv` and pushes `AppCommand`s. Nothing blocks it.
- `UiState::apply` stays a pure reducer, tested without egui. Widgets emit `AppCommand` directly; there is no separate `UiAction` type.
- Persisted model IDs (`MODEL_CHOICES`, aliases) and the settings/conversation file formats are unchanged. New stored fields must use `#[serde(default)]`.

### 6.2 UiState (owner: `ui/mod.rs`), done
| Field | Purpose |
|---|---|
| `view: View{Chat, History}`, plus `back`/`forward` stacks | Page routing. Home is not a separate variant. It is `View::Chat` with no items (`is_home()`), so sending from Home flows straight into the conversation. |
| `sidebar_open`, `sidebar_shown`, `projects_collapsed`, `expanded_projects` | Sidebar toggles (Ctrl+B); `sidebar_shown` is false while a side panel takes its room in a narrow window. `expanded_projects` holds the projects showing all their rows. |
| `expanded: HashSet<String>` | Expanded tool cards and thoughts, and collapsed turn headers. Keys are `tool{idx}`, `thought{idx}` and `turn{idx}`, cleared when a conversation loads. |
| `history_query`, `history_project`, `focus_search` | History page filter; the search icon focuses the box. |
| `customize_open`, `customize_tab`, `settings_dirty` | The Settings overlay and its section. It edits `settings` in place; `AntiCraftApp::ui` flushes the edits with `SaveSettings` once no pointer is down and no text field is focused, and again in `on_exit`. |
| `unread`, `prompt_times` | Conversations whose last turn ended off screen (sidebar dot), and when each prompt of the open conversation was sent (navigator; saved with the conversation). |
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
| (e) No per-message timestamps | `Conversation.prompt_times: BTreeMap<usize, DateTime<Utc>>` (`#[serde(default)]`), keyed by user message and set when a turn starts; rewinding drops the later ones. The navigator shows them (2.5). | `store.rs`, `app.rs`, `ui/navigator.rs` | Done |
| (f) No paging | Client-side `visible_items` (6.2); the file still loads whole. | `ui/chat.rs` | Done |
| Model display names and grouping | `ModelChoice` table (6.3); IDs are unchanged. | `ui/format.rs` | Done |
| Preview availability not detected | Still the manual `preview_access` toggle, with an explanation in Settings > Models. Detection would need a `loadCodeAssist` tier field and is out of scope. | n/a | Accepted (README) |
| Resolved model for `auto` | `resolve_model` is called from the UI for alias subtitles. | `ui/composer.rs` | Done |
| Settings sections and page | `customizations.rs`: the Settings overlay with the sections Models / Tools and approvals / Workspaces / Providers / Account (2.8). The old `ui/settings.rs` window is removed. | `ui/customizations.rs` | Done |
| Session "Always allow" rules listing | `ApprovalMode` lives inside the agent. Add `AppCommand::ListApprovals` -> `AppEvent::Approvals(Vec<String>)` and `AppCommand::RevokeApproval(String)`, served by the backend without locking a running agent (snapshot via an `Arc<Mutex<..>>` shared with the agent). | `agent/mod.rs`, `app.rs`, `ui/customizations.rs` | Planned |
| Data dir display | Add `AppEvent::Paths{data_dir}`, emitted once in `Backend::run`; show it under Account. | `app.rs`, `ui/customizations.rs` | Planned |
| No `NotConfigured` auth state | For now `UiState::oauth_not_configured()` matches the "not configured" error text. This disables "Sign in" and points to Settings > Account. The proper fix is in 6.5. | `ui/mod.rs`, `ui/login.rs` | Interim |
| Tier or project info | Out of scope (would need a `loadCodeAssist` result event). | n/a | Not needed |
| Automations | Disabled nav row with a "Coming soon" tooltip. | `ui/sidebar.rs` | Done |
| Microphone, pin | Visual only, disabled with a tooltip. ("Open IDE" opens the workspace in VS Code, else Explorer.) | `ui/composer.rs`, `ui/sidebar.rs` | Done |

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
| `ui/widgets.rs` | `nav_row`, `icon_button`, `overlay_icon`, `pill_chip`, `pill_chip_up`, `send_button`, `toggle`, `card`, `segmented`, `text_link`, `paint_truncated`, `centered_column`. |
| `ui/titlebar.rs` | Menu bar and toolbar (navigation, breadcrumb and rename, panel toggles, Open IDE, overflow menu). |
| `ui/sidebar.rs` | Nav rows, Projects grouping, row status marks and hover actions, the Settings row. |
| `ui/login.rs` | Sign-in pill and the Home notice. |
| `ui/home.rs`, `ui/git.rs` | Home (greeting, suggestions, pinned message box) and the read-only Git branch label. |
| `ui/conversation.rs`, `ui/navigator.rs`, `ui/composer.rs`, `ui/model_picker.rs`, `ui/confirm.rs` | Conversation, navigator rail, message box, model menu, inline confirmation. |
| `ui/history.rs` | History page. |
| `ui/customizations.rs` | Settings overlay. |
| `ui/demo.rs` and `main.rs` | `--demo NAME [--screenshot OUT.png] [--window-size WxH]`: offline demo states with detached channels (no backend, network or credentials). |
