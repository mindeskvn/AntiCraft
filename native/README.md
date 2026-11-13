# AntiCraft native

## What it is
AntiCraft native (`anticraft-native`) is a Rust/egui desktop coding assistant. It chats with Gemini models through the Google Code Assist API and can read, search and edit files and run PowerShell commands inside a workspace folder you choose. Protocol behavior was studied from gemini-cli (Apache-2.0) and reimplemented. See `NOTICE` and `PROTOCOL.md`.

## Build and run
```
cargo build --release --manifest-path native/Cargo.toml
target\release\anticraft-native.exe [--workspace DIR] [--model ID] [--data-dir DIR] [--logout]
```
Tests use mock servers only. Set `ANTICRAFT_LOG=debug` for more logging.

## OAuth client secret
The Antigravity sign-in uses Antigravity's own OAuth client. The exe on the Releases page has it built in, so there is nothing to set. The client's secret is not in the source: when you build it yourself, set `ANTICRAFT_ANTIGRAVITY_CLIENT_SECRET`, or write the line `ANTICRAFT_ANTIGRAVITY_CLIENT_SECRET=value` into `native/antigravity-client.local.env` (git ignores that file; after creating it, touch `native/build.rs` so the build reads it). Without it the build still runs, and only the Antigravity sign-in reports an error.

Only the Gemini CLI sign-in (Customizations > Account > "Gemini CLI sign-in (Google)") needs the gemini-cli public installed-app client secret. Set it either way:
- In the app: Customizations > Account, type it into "Client secret" and press Save (or Enter). It is stored in `oauth_client.json` in the data folder with an owner-only DACL. Clear removes it. The field's undo history is dropped after saving.
- The `ANTICRAFT_OAUTH_CLIENT_SECRET` environment variable, at run time or build time.

Precedence: saved file, then run-time env, then build-time value. The value is never logged or shown again.

## UI overview
- Top bar: back/forward, conversation title (click to rename, Esc cancels), `...` menu with Rename, Copy as Markdown and Delete (two clicks, disabled while a turn runs).
- Sidebar: Conversation History, Customizations, conversations grouped by project, account row at the bottom.
- Home: centered composer. Conversation: reading column with markdown, code blocks, tool-call cards, "Worked for ..." turn headers, and a docked composer with the model picker and Stop.
- Tool confirmations appear inline in the transcript (diff or command preview) with Approve / Reject / Always allow in the composer. Enter approves and Esc rejects, unless a menu is open.
- Under the last answer: Copy and "Send again" (re-sends the last question as a new turn).
- The model chip is always the model the next turn uses, including after opening an older conversation.
- Icons come from the Phosphor icon font (MIT).

## Demo and screenshots
Fixture data only, no sign-in, no network:
```
anticraft-native.exe --demo <screen> [--screenshot OUT.png] [--window-size WxH]
```
Screens: `home`, `conversation`, `turn-start`, `turn-error`, `progress`, `planning`, `review`, `model-menu`, `history`, `customizations`, `login`, `accounts`, `accounts-models`, `providers`, `provider-add`, `provider-dialog`, `provider-menu`, `bypass`, `bypass-chat`, `thinking`, `confirm-delete`.

With `--screenshot` the app saves a PNG and exits. The pictures in the repository's main README are in `docs/screenshots/` and come from these screens.

## First login
"Sign in" in the sidebar and on Home is the **Antigravity** sign-in. It uses Antigravity's own OAuth client and needs no client secret.
1. Your browser opens Google consent and Google redirects to `http://localhost:51121/oauth-callback`. Forged states are ignored.
2. If the port is busy or the browser cannot come back, paste the address the browser landed on into "Paste redirect link" on the Antigravity row. The state is checked the same way.
3. The account refreshes its own tokens and sends your turns.

The Gemini CLI sign-in (Customizations > Account > "Gemini CLI sign-in (Google)") is the older path through gemini-cli's OAuth client. It needs the client secret above:
1. Click its "Sign in". Your browser opens Google consent (PKCE plus a random state).
2. Google redirects to `http://127.0.0.1:<port>/oauth2callback`. The app keeps accepting connections until a request with the exact path and the matching state arrives. Preconnects, favicon requests and forged states are ignored. Set `OAUTH_CALLBACK_PORT` to pin the port.
3. Tokens are stored, verified at the next start and refreshed before they expire. A 401 forces one refresh and a retry. `invalid_grant` asks you to log in again.

## Tools and confirmation model
- **Read/search**: `read_file`, `list_directory`, `glob`, `grep_search` (alias `search_file_content`). They run without asking unless you turn off "Run read-only tools without asking"; then every call shows a prompt. Sizes are capped: 20 MB per read, 2000 chars per line, 256 KB of output.
- **Edit**: `write_file`, `replace`. These always show a diff. The approval is tied to the previewed file hash and resolved path, so a file that changed after approval is not written. Writes are atomic.
- **Shell**: `run_shell_command` (PowerShell). This always asks. "Always allow" covers only simple commands with the same first word. Compound commands (`;`, `|`, `&`, backtick, `$()`, `@()`, redirection, script blocks, newlines) always ask again. The model's description is shown as unverified, below the command. Output is capped while it streams. A timeout or Stop kills the whole process tree (Windows Job Object).
- Every path is confined to the workspace. Dangling symlinks and junctions are rejected.
- Rejecting a call stops the turn. Stop cancels streaming, setup, confirmations and tools. The history stays a well-formed user/model alternation, and partial replies are kept.

## Models
The dropdown offers `auto`, Pro, Flash and Flash-lite IDs. `auto`/`pro` resolve to `gemini-3-pro-preview` when "preview access" is on, otherwise to `gemini-2.5-pro`. `gemini-3.5-flash` is sent as `gemini-3-flash`, or as `gemini-3.8-flash` when "latest Flash GA" is on (PROTOCOL 3.11). Retryable errors (429 with RetryInfo, 5xx, network) are retried with backoff while nothing has been shown yet.

## Bypass Mode
Customizations > Tools and approvals > Bypass Mode sends the **first requests of each message** to a provider profile you choose, then the conversation returns to the model in use. The count is configurable (1-50). Tools, history and approvals are unchanged.

- Pick the profile under "Bypass provider"; a new one can be added right there.
- "Instruction files" takes `.md` files. Their text is added to the system instruction of the requests that go to that provider only: never to the conversation history, and never to the normal model.
- That provider **sees the content** of the requests sent to it, under its own terms. While the mode is on, the composer always carries a red band naming the provider, its model, the request count and the model that takes over, with a "Turn off" link.
- Switching the mode affects the next message; a message already running keeps the route it began with.

## Where data is stored
`%APPDATA%\AntiCraft\native\` (override with `--data-dir`):
- `oauth_creds.json`: the Gemini CLI sign-in's tokens. The file is created with a protected DACL for the current user's SID only (mode 0600 on unix). A newly created data folder also gets an owner-only, inheritable DACL (an existing `--data-dir` keeps its ACL).
- `accounts\{id}.json`: one file per subscription account (Antigravity, Codex, Claude Code), owner-only as well.
- `oauth_client.json`: the client secret entered in Customizations.
- `settings.json`
- `conversations\{id}.json`
- `logs\native.log`: never contains tokens.

`~/.gemini` is never read or written.

## Account risk
This app calls the **non-public** Code Assist API (`cloudcode-pa.googleapis.com/v1internal`) using gemini-cli's public OAuth client. It is not a Google product. Google may change the API, rate-limit or restrict accounts that use it at any time. Use at your own risk.

## Known gaps
- Preview access and latest-Flash-GA are manual settings, not detected from the account.
- No automatic Pro to Flash fallback on quota errors.
- No MCP, web fetch/search or memory tools.
- The compound-command check is character based, not a full PowerShell parser, so it can over-prompt.
- Edits re-verify the path and content hash, but files are not opened with no-follow semantics, so a very small TOCTOU window remains.
- The unix code paths compile by design but are only tested on Windows.

## Subscription accounts
Customizations > Account > Accounts signs in with a subscription instead of an API key: Antigravity (a Google account, through Antigravity's own OAuth client), ChatGPT Codex and Claude Code. Each signed-in account lists its models (Models shows which ones it serves and checks again on request), can be put in use for turns, and is signed out of on its own row. The composer's model picker switches between accounts and their models.

These are consumer subscription sign-ins. The providers may restrict third-party clients: a refused request appears on the row, and an API key on the Providers tab is the alternative. Using a subscription this way may violate that provider's terms and put the account at risk.

AntiCraft sends its own identity (`User-Agent: anticraft-native/...`) and does not pose as the official clients. The one exception is the system line `You are Claude Code, Anthropic's official CLI for Claude.` on Claude's OAuth path, because Anthropic's endpoint refuses inference for these tokens without it; the official CLI's User-Agent, billing header and request signing are not sent. Tokens live only in `accounts\{id}.json` (owner-only) and never reach a log, a URL or the UI. Nothing is written to `~/.gemini`, `~/.codex` or `~/.claude`. `--logout` removes only the Gemini CLI sign-in; sign out of an account on its row.
