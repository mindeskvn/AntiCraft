# AntiCraft UI interfaces (egui 0.37)

Design tokens and layout: see DESIGN.md. Everything here is in `src/ui/`.

## Routing (`mod.rs`)
- `enum View { Chat, History, Customizations }` with `back`/`forward` stacks (`navigate`, `go_back`, `go_forward`).
- `enum Screen { Home, Conversation, History, Customizations }`, from `UiState::screen()`. Home is `View::Chat` with no items.
- `type UiAction = AppCommand`. Screens push backend commands directly (DESIGN 6.1).
- `UiState::apply(AppEvent)` is a pure reducer. New fields: `oauth_source: Option<ClientSource>`, `secret_input` (UI-only, cleared when sent) and `open_model_menu` (demo).
- `AntiCraftApp::ui` lays out the panels: `titlebar` (top, 74px), `sidebar` (left, 256px), then the central panel, which matches on `screen()`.

## Screens (each is `pub fn show(ui, &mut UiState, &mut Vec<UiAction>)`)
| File | Content |
|---|---|
| `titlebar.rs` | `show` draws the menu bar row (34) and toolbar row (40); `menu_bar` and `toolbar` are still public |
| `sidebar.rs` | Nav rows, Projects groups, conversation rows, account row |
| `home.rs` | Workspace chip, centered composer, Local chip, sign-in notice |
| `conversation.rs` | `show(ui, state, &mut CommonMarkCache, actions)`: transcript, tool cards, thoughts, turn header, docked composer + inline confirmation; `pub fn banner` |
| `history.rs` | Search, project filter pills, date-grouped rows |
| `customizations.rs` | Tabs General/Agent/Workspaces/Account; Account has the masked OAuth client secret field (Save/Clear) |
| `model_picker.rs` | `show` draws the model chip and popup above it; `model_menu(ui, &UiState) -> Option<String>` |
| `composer.rs` | `show(ui, state, actions, autofocus)`, `workspace_chip`, `local_chip`, `is_send_key` |
| `confirm.rs` | `inline(ui, state)`: edit diff / shell / read confirmation; Enter allows, Esc denies |
| `login.rs` | `account_row`, `home_notice` |
| `markdown.rs` | `render`, `render_diff` |
| `format.rs` | Pure helpers (relative_time, group_by_project, date_bucket, model_choices, ...) |

## Theme (`theme.rs`)
- Color tokens are `const Color32`: `BG FG FG_SECONDARY FG_MUTED PLACEHOLDER PRIMARY LINK CODE_FG SUCCESS WARNING ERROR SIDEBAR CARD CARD_BORDER BORDER SECONDARY ROW_SELECTED ROW_HOVER GROUP_HEADER SEND_BG PRESSED TOOLTIP`.
- Size, spacing and radius tokens: `SIDEBAR_WIDTH COLUMN_WIDTH TOP_BAR_HEIGHT TOOLBAR_HEIGHT CARD_PAD MESSAGE_GAP SECTION_GAP NAV_ROW_HEIGHT NAV_ROW_PITCH CONV_ROW_PITCH RADIUS_*`.
- Fonts: `font(size)`, `medium(size)` (Segoe UI Semibold), `mono(size)` (Cascadia Mono, then Consolas), `icon(size)` (Phosphor).
- Segoe UI and Consolas are read at runtime from `%WINDIR%\Fonts`. If they are missing, egui's default fonts are used; Segoe UI Symbol is a further fallback. Segoe UI covers Vietnamese diacritics.
- `apply(ctx)` (alias `install`) sets the fonts, the dark `Visuals`/`Style` (widget states, selection, thin scrollbars, 150 ms tooltip delay) and the dark theme.

## Icons (`icons.rs`)
Phosphor Regular codepoints as `&str` constants. The font is vendored at `assets/Phosphor-Regular.ttf` (MIT, see `assets/Phosphor-LICENSE-MIT.txt`), because `egui-phosphor` 0.14 targets egui 0.36. Use them with `theme::icon(size)`.

## Widgets (`widgets.rs`)
| Fn | Notes |
|---|---|
| `nav_row(ui, icon, label, selected, enabled) -> Response` | 240x32 row; when disabled it shows the "Coming soon" tooltip |
| `section_header(ui, label, &[(icon, tip, enabled)]) -> (Response, Option<usize>)` | 20px header with right-aligned 20x20 buttons |
| `icon_button(ui, icon, size, tip, enabled)`, `paint_icon_button`, `overlay_icon(ui, rect, id, icon, tip, enabled)` | 20/24/28 square buttons |
| `pill_chip(ui, icon, label, caret, enabled, small)`, `chip(ui, icon, label, enabled)`, `filter_pill(ui, label, selected)` | Pills |
| `send_button(ui, busy, ready)`, `toggle(ui, &mut bool)` | |
| `card(radius, pad)`, `card_frame()`, `composer_card()`, `popup_frame(ui)` | Frames |
| `popup_menu(anchor, width, add) -> Option<R>`, `menu_item(ui, icon, label, shortcut, enabled) -> bool` | Popup menu helpers |
| `tooltip(resp, tip)` | Hover tip, or "Coming soon" when disabled |
| `conv_row(ui, title, time, selected, running) -> (Response, Option<RowAction>)` | Sidebar row with hover Pin (disabled), Rename and Delete; `RowAction { Open, Pin, Rename, Delete }` |
| `text_link`, `section_label`, `paint_truncated`, `centered_column` | |

## Demo / screenshot harness (`demo.rs`, `main.rs`)
`anticraft-native --demo <home|conversation|model-menu|history|customizations|login> [--screenshot out.png]`
(the old flags `--ui-fixture`/`--snapshot` still work).
- The demo uses fixture data only: 3 projects, 12 conversations with Vietnamese and English titles, and a conversation with markdown, a code block, a collapsed thought, tool cards and a pending edit with a diff.
- No backend runs, so there is no auth, network or disk write. Channels are detached, and eframe storage points to a temp dir that is deleted on exit.
- The window is 1400x900. After 4 frames it calls `ViewportCommand::Screenshot` and saves the image with `demo::save_png` (using the `png` crate, pinned `=0.18.1`), then closes.

## OAuth client secret
- Stored at `<data-dir>\oauth_client.json` (`auth/client_config.rs`), written with `atomic_write_private` so only the owner can read it. Its `Debug` output is redacted and it is never logged.
- `AuthConfig::load(data_dir) -> (AuthConfig, ClientSource)` resolves the secret in this order: the file, the `ANTICRAFT_OAUTH_CLIENT_SECRET` env var, then the build-time value.
- Commands: `AppCommand::SetOAuthClientSecret(String)` and `ClearOAuthClientSecret`. Both are rejected while a turn or sign-in is running. They rebuild auth and the client and emit `AppEvent::OAuthClientSource(Option<ClientSource>)`, then `Auth(..)`.
