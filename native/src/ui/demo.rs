//! Demo UI states for offline screenshots (`--demo NAME --screenshot OUT.png`).
//! No backend, network or credentials are involved: the app runs with detached channels.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use super::{ChatItem, CustomizeTab, PendingConfirm, ProviderDialog, ToolCard, UiState, View};
use crate::agent::{ConfirmationDetails, ConfirmationRequest};
use crate::app::{AccountLoginInfo, AccountModelsState};
use crate::auth::external::{catalog, AccountProvider, AccountStatus, AccountView};
use crate::auth::AuthStatus;
use crate::providers::{ProviderKind, ProviderProfile};
use crate::settings::AccountEntry;
use crate::store::ConversationSummary;

pub const NAMES: &[&str] = &[
    "home",
    "conversation",
    "turn-start",
    "turn-error",
    "progress",
    "model-menu",
    "history",
    "customizations",
    "login",
    "accounts",
    "accounts-models",
    "providers",
    "provider-add",
    "provider-dialog",
    "provider-menu",
    "bypass",
    "bypass-chat",
    "thinking",
    "confirm-delete",
];

fn conv(title: &str, ws: Option<&str>, ago_min: i64) -> ConversationSummary {
    ConversationSummary {
        id: Uuid::new_v4(),
        title: title.into(),
        workspace: ws.map(PathBuf::from),
        model: "auto".into(),
        updated_at: Utc::now() - chrono::Duration::minutes(ago_min),
    }
}

const DEMO_APP: &str = r"C:\work\demo-app";
const API: &str = r"C:\work\api-server";
const NOTES: &str = r"C:\work\ghi-chu";

fn base() -> UiState {
    let mut s = UiState { auth: AuthStatus::LoggedIn { email: Some("user@example.com".into()) }, ..Default::default() };
    s.settings.workspace_history = vec![PathBuf::from(DEMO_APP), PathBuf::from(API), PathBuf::from(NOTES)];
    s.workspace = Some(PathBuf::from(DEMO_APP));
    s.settings.model = "auto".into();
    s.oauth_source = Some(crate::auth::ClientSource::Env);
    let day = 60 * 24;
    s.conversations = vec![
        conv("Sửa lỗi mã hóa tiếng Việt khi xuất CSV", Some(DEMO_APP), 4),
        conv("Add retry to the upload client", Some(DEMO_APP), 3 * 60),
        conv("Thêm chế độ tối cho trang cài đặt", Some(DEMO_APP), 26 * 60),
        conv("Untitled Conversation", Some(DEMO_APP), 2 * day),
        conv("Refactor the image cache", Some(DEMO_APP), 6 * day),
        conv("Write unit tests for the parser", Some(API), 40),
        conv("Tối ưu truy vấn danh sách đơn hàng", Some(API), 5 * 60),
        conv("Update deploy script", Some(API), 9 * day),
        conv("Migrate config to TOML", Some(API), 150 * day),
        conv("Tóm tắt biên bản họp tuần", Some(NOTES), 25 * day),
        conv("Draft release notes for v2.3", Some(NOTES), 40 * day),
        conv("Câu hỏi nhanh về regex", None, 70 * day),
    ];
    signed_in_accounts(&mut s);
    s
}

const ANTIGRAVITY_ID: &str = "antigravity-1a2b3c4d";
const CODEX_ID: &str = "codex-5e6f7a8b";
const CLAUDE_ID: &str = "claude-9c0d1e2f";

/// What a fetch of the demo Antigravity account returned: one of the spec's defaults
/// (`gemini-3.5-flash-lite`) is missing, so the popup has a muted chip and reads "Supports 6/7".
const ANTIGRAVITY_SERVED: &[&str] =
    &["claude-opus-4-6-thinking", "claude-sonnet-4-6", "claude-sonnet-5-5-high", "gemini-3-flash", "gemini-3.8-flash-high", "gpt-oss-120b-medium"];

/// One signed-in account, its model list checked `ago_min` minutes ago.
fn account(id: &str, provider: AccountProvider, email: &str, plan: Option<&str>, model: &str, models: Vec<String>, ago_min: i64) -> AccountView {
    AccountView {
        id: id.into(),
        provider,
        email: Some(email.into()),
        plan: plan.map(Into::into),
        model: model.into(),
        models,
        models_fetched_at: Some(Utc::now() - chrono::Duration::minutes(ago_min)),
        status: AccountStatus::SignedIn,
    }
}

/// One signed-in account per provider, as the backend reports them and settings list them, with
/// the Antigravity account sending turns. Codex and Claude carry their static catalog, which is
/// what a sign-in records for them.
fn signed_in_accounts(s: &mut UiState) {
    s.accounts = vec![
        account(
            ANTIGRAVITY_ID,
            AccountProvider::Antigravity,
            "an.nguyen@example.com",
            None,
            "gemini-3-flash",
            ANTIGRAVITY_SERVED.iter().map(|m| (*m).to_owned()).collect(),
            25,
        ),
        account(CODEX_ID, AccountProvider::Codex, "dev@example.com", Some("plus"), "gpt-6.1-sol", catalog::models_for(AccountProvider::Codex, Some("plus")), 3 * 60),
        account(CLAUDE_ID, AccountProvider::ClaudeCode, "team@example.org", Some("Acme Studio"), "claude-opus-5-5", catalog::models(AccountProvider::ClaudeCode), 26 * 60),
    ];
    s.settings.accounts = s
        .accounts
        .iter()
        .map(|v| AccountEntry { id: v.id.clone(), provider: v.provider, label: v.email.clone().unwrap_or_default(), model: v.model.clone() })
        .collect();
    s.settings.active_account = Some(ANTIGRAVITY_ID.into());
}

/// The Account tab with a row in every state: the three accounts signed in, Antigravity's and
/// Claude's sign-in rows idle, and a Codex sign-in that stopped on a busy port. That attempt is
/// kept, so its row offers "Paste redirect link" under the error.
fn accounts_state(s: &mut UiState) {
    s.view = View::Customizations;
    s.customize_tab = CustomizeTab::Account;
    s.back = vec![View::Chat];
    let error = "port 1455 on localhost is already in use by another program. Close it and try again, or paste the redirect link instead";
    s.account_errors.insert(AccountProvider::Codex, error.into());
    s.account_login = Some(AccountLoginInfo {
        id: 7,
        provider: AccountProvider::Codex,
        auth_url: "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&state=demo".into(),
        error: Some(error.into()),
    });
}

/// [`accounts_state`] with the supported-models popup of the Antigravity account open, and a
/// Claude Code sign-in running (so the screenshot also shows the spinner and Cancel).
///
/// Starting that sign-in let the kept Codex attempt go, as the backend does: the Codex row keeps
/// its error but no longer offers the paste link. Both screens together show every row state.
fn accounts_models_state(s: &mut UiState) {
    accounts_state(s);
    s.open_account_models = Some(ANTIGRAVITY_ID.into());
    s.accounts_models.insert(ANTIGRAVITY_ID.into(), AccountModelsState::default());
    s.accounts.push(AccountView {
        id: String::new(),
        provider: AccountProvider::ClaudeCode,
        email: None,
        plan: None,
        model: String::new(),
        models: Vec::new(),
        models_fetched_at: None,
        status: AccountStatus::SigningIn,
    });
    s.account_login = Some(AccountLoginInfo {
        id: 8,
        provider: AccountProvider::ClaudeCode,
        auth_url: "https://claude.ai/oauth/authorize?code=true&state=demo".into(),
        error: None,
    });
}

/// What a load of the demo gateway listed (CLIProxyAPI-style ids).
const GATEWAY_MODELS: &[&str] = &[
    "claude-haiku-5.5",
    "claude-opus-5.5",
    "claude-opus-5.5-thinking",
    "claude-sonnet-5.5",
    "claude-sonnet-5.5-thinking",
    "gemini-3-flash",
    "gemini-3.8-flash-high",
    "gpt-6.1-sol",
];

/// Two provider profiles, the gateway one in use: its models loaded and one chosen. The other
/// was saved without a model and has none loaded.
fn providers_state(s: &mut UiState) {
    s.settings.providers = vec![
        ProviderProfile {
            id: "pix4k".into(),
            label: "pix4k".into(),
            kind: ProviderKind::Anthropic,
            model: "claude-opus-5.5-thinking".into(),
            base_url: Some("http://localhost:8386/".into()),
            api_key: "demo-key".into(),
            max_output_tokens: None,
            context_window: None,
            models: GATEWAY_MODELS.iter().map(|m| (*m).to_owned()).collect(),
        },
        ProviderProfile {
            id: "openrouter".into(),
            label: "OpenRouter".into(),
            kind: ProviderKind::OpenAi,
            model: String::new(),
            base_url: Some("https://openrouter.ai/api/v1".into()),
            api_key: "demo-key".into(),
            max_output_tokens: None,
            context_window: None,
            models: Vec::new(),
        },
    ];
    s.settings.select_profile(Some("pix4k".into()));
}

fn conversation_items() -> Vec<ChatItem> {
    vec![
        ChatItem::User(r#"Script xuất CSV ghi tiếng Việt bị lỗi font (ví dụ "Nguyá»…n"). Can you fix the encoding?"#.into()),
        ChatItem::TurnHeader { duration: Some(Duration::from_secs(62)) },
        ChatItem::Thought {
            subject: Some("Inspecting the export script".into()),
            text: "The file is opened without an explicit encoding, so Windows falls back to cp1252.".into(),
        },
        ChatItem::Tool(ToolCard {
            call_id: "1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"file_path": "scripts/export.py"}),
            result: Some((true, "Read 42 lines".into())),
        }),
        ChatItem::Tool(ToolCard {
            call_id: "2".into(),
            name: "grep_search".into(),
            args: serde_json::json!({"pattern": r"open\("}),
            result: Some((true, "3 matches".into())),
        }),
        ChatItem::Tool(ToolCard {
            call_id: "3".into(),
            name: "run_shell_command".into(),
            args: serde_json::json!({"command": "python scripts/export.py --check"}),
            result: Some((true, "ok".into())),
        }),
        ChatItem::Assistant(ASSISTANT_REPLY.into()),
        ChatItem::Tool(ToolCard {
            call_id: "4".into(),
            name: "replace".into(),
            args: serde_json::json!({"file_path": "scripts/export.py"}),
            result: None,
        }),
    ]
}

/// A greeting, then a question whose turn read six files and was refused when it sent them back.
fn turn_error_items() -> Vec<ChatItem> {
    let read = |id: &str, path: &str, lines: &str| tool_card(id, "read_file", serde_json::json!({"file_path": path}), Some((true, lines)));
    vec![
        ChatItem::User("Xin chào bro".into()),
        ChatItem::TurnHeader { duration: Some(Duration::from_secs(3)) },
        ChatItem::Assistant("Chào bạn! Hôm nay mình giúp gì cho dự án?".into()),
        ChatItem::User("Dự án của tôi đang làm gì".into()),
        ChatItem::TurnHeader { duration: Some(Duration::from_secs(4)) },
        ChatItem::Thought {
            subject: Some("Exploring the project".into()),
            text: "Reading the README and the entry points first.".into(),
        },
        tool_card("1", "list_directory", serde_json::json!({"dir_path": "."}), Some((true, "12 entries"))),
        read("2", "README.md", "Read 88 lines"),
        read("3", "Cargo.toml", "Read 41 lines"),
        read("4", "src/main.rs", "Read 120 lines"),
        read("5", "src/lib.rs", "Read 64 lines"),
        read("6", "src/export.rs", "Read 97 lines"),
    ]
}

/// A greeting, then the question about the project answered after eight steps.
fn progress_items() -> Vec<ChatItem> {
    let list = |id: &str, dir: &str, n: &str| tool_card(id, "list_directory", serde_json::json!({"dir_path": dir}), Some((true, n)));
    let read = |id: &str, path: &str, n: &str| tool_card(id, "read_file", serde_json::json!({"file_path": path}), Some((true, n)));
    vec![
        ChatItem::User("Xin chào".into()),
        ChatItem::TurnHeader { duration: Some(Duration::from_secs(3)) },
        ChatItem::Assistant("Xin chào! Mình có thể giúp gì cho dự án của bạn?".into()),
        ChatItem::User("Dự án của tôi đang làm gì".into()),
        ChatItem::TurnHeader { duration: Some(Duration::from_secs(38)) },
        list("1", ".", "14 entries"),
        list("2", "scripts", "6 entries"),
        read("3", "README.md", "Read 88 lines"),
        read("4", "scripts/export.py", "Read 42 lines"),
        read("5", "scripts/import_orders.py", "Read 131 lines"),
        tool_card("6", "grep_search", serde_json::json!({"pattern": "def main"}), Some((true, "3 matches"))),
        read("7", "web/README.md", "Read 61 lines"),
        tool_card("8", "run_shell_command", serde_json::json!({"command": "python -m pytest -q"}), Some((true, "12 passed"))),
        ChatItem::Assistant(PROGRESS_REPLY.into()),
    ]
}

const PROGRESS_REPLY: &str = r#"Dự án là một công cụ nhỏ để **xuất và nhập đơn hàng** dưới dạng CSV.

## Thành phần chính

1. `scripts/export.py`: đọc đơn hàng từ SQLite và ghi CSV cho Excel (UTF-8 có BOM).
2. `scripts/import_orders.py`: đọc lại CSV, kiểm tra từng dòng rồi ghi vào cơ sở dữ liệu.
3. `web/`: trang quản trị nhỏ để xem đơn hàng và tải file xuất.

## Kiểm thử

`pytest` chạy 12 test, tất cả đều qua.

Bạn muốn tìm hiểu kỹ phần nào?"#;

const ASSISTANT_REPLY: &str = r#"Nguyên nhân: file được mở **không chỉ định encoding**, nên Windows dùng `cp1252`.

Changes:

1. Open the output with `encoding="utf-8-sig"` so Excel detects UTF-8.
2. Pass `newline=""` to avoid blank rows.

```python
with open(path, "w", encoding="utf-8-sig", newline="") as f:
    writer = csv.writer(f)
    writer.writerows(rows)
```

I'll apply the edit now."#;

const DEMO_DIFF: &str = r#"--- a/scripts/export.py
+++ b/scripts/export.py
@@ -12,3 +12,3 @@
 def export(path, rows):
-    with open(path, "w") as f:
+    with open(path, "w", encoding="utf-8-sig", newline="") as f:
         writer = csv.writer(f)
"#;

/// Pending edit confirmation with a diff. The reply channel is detached (answers are dropped).
fn pending_edit() -> PendingConfirm {
    let (tx, _rx) = tokio::sync::oneshot::channel();
    PendingConfirm {
        request: ConfirmationRequest {
            call_id: "4".into(),
            tool_name: "replace".into(),
            details: ConfirmationDetails::Edit {
                path: "scripts/export.py".into(),
                diff: DEMO_DIFF.into(),
                is_new_file: false,
                resolved: PathBuf::from(r"C:\work\demo-app\scripts\export.py"),
                old_hash: None,
            },
            respond: tx,
        },
    }
}

const DEMO_DONE_THOUGHT: &str = "The user wants the old build output gone. List the workspace root first.";
const DEMO_LIVE_THOUGHT: &str =
    "Only build/ and dist/ are generated. src/ and the config files must stay, so the command removes those two folders and nothing else.";
const DEMO_DELETE_COMMAND: &str = "Remove-Item -Recurse -Force build, dist";

/// A live turn: a finished thought and its tool call, then the thought being reasoned now.
fn thinking_items() -> Vec<ChatItem> {
    vec![
        ChatItem::User("Dọn thư mục build cũ giúp tôi".into()),
        ChatItem::TurnHeader { duration: None },
        ChatItem::Thought { subject: Some("Looking for build output".into()), text: DEMO_DONE_THOUGHT.into() },
        tool_card("1", "list_directory", serde_json::json!({"dir_path": "."}), Some((true, "12 entries"))),
        ChatItem::Thought { subject: Some("Choosing what to remove".into()), text: DEMO_LIVE_THOUGHT.into() },
    ]
}

fn pending_shell(call_id: &str, command: &str) -> PendingConfirm {
    let (tx, _rx) = tokio::sync::oneshot::channel();
    PendingConfirm {
        request: ConfirmationRequest {
            call_id: call_id.into(),
            tool_name: "run_shell_command".into(),
            details: ConfirmationDetails::Shell {
                command: command.into(),
                dir: r"C:\work\demo-app".into(),
                root_command: crate::agent::tools::root_command(command),
                description: Some("Remove the generated build folders".into()),
                simple: crate::agent::tools::is_simple_command(command),
            },
            respond: tx,
        },
    }
}

/// Build the demo state `name`.
pub fn state(name: &str) -> Option<UiState> {
    let mut s = base();
    match name {
        "home" => {}
        "model-menu" => s.open_model_menu = true,
        "login" => {
            // Nothing signed in: the sidebar pill and Home offer the Antigravity sign-in.
            s.auth = AuthStatus::LoggedOut;
            s.accounts.clear();
            s.settings.accounts.clear();
            s.settings.active_account = None;
        }
        "accounts" => accounts_state(&mut s),
        "accounts-models" => accounts_models_state(&mut s),
        "providers" | "provider-add" | "provider-dialog" => {
            s.view = View::Customizations;
            s.customize_tab = CustomizeTab::Providers;
            s.back = vec![View::Chat];
            providers_state(&mut s);
            if name == "provider-add" {
                // "Add provider" pressed: nothing typed yet.
                s.provider_dialog = Some(ProviderDialog::add(ProviderKind::OpenAi));
            } else if name == "provider-dialog" {
                // Edit on pix4k, its list just loaded.
                let mut d = ProviderDialog::edit(&s.settings.providers[0]);
                d.models_from = Some("http://localhost:8386/v1/models".into());
                s.provider_dialog = Some(d);
            }
        }
        "provider-menu" => {
            providers_state(&mut s);
            s.open_model_menu = true;
        }
        "bypass" => {
            // Customizations > Tools and approvals, scrolled to Bypass Mode: switched on, its
            // provider picked, three requests, two instruction files.
            s.view = View::Customizations;
            s.customize_tab = CustomizeTab::Agent;
            s.back = vec![View::Chat];
            providers_state(&mut s);
            s.settings.bypass.enabled = true;
            s.settings.bypass.provider = Some("pix4k".into());
            s.settings.bypass.requests = 3;
            s.settings.bypass.files = vec![PathBuf::from(r"C:\work\rules\house-style.md"), PathBuf::from(r"C:\work\rules\answering.md")];
        }
        "bypass-chat" => {
            // A conversation with the mode on: the red band over the composer. The mode's
            // provider is the gateway while Code Assist sends the rest, so the band names two
            // different models, as it would in use.
            let c = &s.conversations[1];
            s.current = Some(c.id);
            s.current_title = c.title.clone();
            s.items = conversation_items();
            providers_state(&mut s);
            s.settings.select_code_assist();
            s.settings.model = "gemini-3.1-pro-preview".into();
            s.settings.bypass.enabled = true;
            s.settings.bypass.provider = Some("pix4k".into());
            s.settings.bypass.requests = 2;
        }
        "conversation" => {
            let c = &s.conversations[0];
            s.current = Some(c.id);
            s.current_title = c.title.clone();
            s.items = conversation_items();
            s.busy = true;
            s.running = Some(c.id);
            s.pending_confirm = Some(pending_edit());
        }
        "turn-start" => {
            // The moment after a first prompt: the turn has its header and nothing else yet.
            s.items = vec![ChatItem::User("Xin chào".into()), ChatItem::TurnHeader { duration: None }];
            s.busy = true;
        }
        "thinking" | "confirm-delete" => {
            // "thinking": the thought being reasoned is open, the finished one above it is
            // only its row. "confirm-delete": the same turn once the model asks to delete.
            let c = &s.conversations[0];
            s.current = Some(c.id);
            s.current_title = c.title.clone();
            s.items = thinking_items();
            s.busy = true;
            s.running = Some(c.id);
            if name == "confirm-delete" {
                s.items.push(tool_card("2", "run_shell_command", serde_json::json!({"command": DEMO_DELETE_COMMAND}), None));
                s.pending_confirm = Some(pending_shell("2", DEMO_DELETE_COMMAND));
            }
        }
        "turn-error" => {
            // A turn the provider refused once its tools had run. The error sits on the composer,
            // where it is seen however long the turn was.
            s.conversations.insert(0, conv("Dự án của tôi đang làm gì", Some(DEMO_APP), 1));
            let c = &s.conversations[0];
            s.current = Some(c.id);
            s.current_title = c.title.clone();
            s.items = turn_error_items();
            s.banner = Some(super::Banner::Error("HTTP 400: Request contains an invalid argument.".into()));
        }
        "progress" => {
            // That question answered after eight steps, with the Progress panel opened from the
            // toolbar.
            s.conversations.insert(0, conv("Dự án của tôi đang làm gì", Some(DEMO_APP), 1));
            let c = &s.conversations[0];
            s.current = Some(c.id);
            s.current_title = c.title.clone();
            s.items = progress_items();
            s.progress_open = true;
        }
        "history" => {
            s.view = View::History;
            s.back = vec![View::Chat];
        }
        "customizations" => {
            s.view = View::Customizations;
            s.customize_tab = CustomizeTab::General;
            s.back = vec![View::Chat];
        }
        other => return orchestration_state(other),
    }
    Some(s)
}

/// Save an egui screenshot as RGBA PNG.
pub fn save_png(path: &Path, img: &egui::ColorImage) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let [w, h] = img.size;
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(std::io::Error::other)?;
    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
    writer.write_image_data(&bytes).map_err(std::io::Error::other)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_util::{context, painted};

    #[test]
    fn all_fixtures_build() {
        for n in super::NAMES {
            assert!(super::state(n).is_some(), "{n}");
        }
        assert!(super::state("nope").is_none());
    }

    /// Draw the Customizations page of `s` for `frames` frames; the texts of the last one.
    fn page(s: &mut UiState, frames: usize) -> Vec<(String, egui::Rect)> {
        let ctx = context();
        let mut commands = Vec::new();
        let mut texts = Vec::new();
        for _ in 0..frames {
            texts = painted(&ctx, Vec::new(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| crate::ui::customizations::show(ui, s, &mut commands));
            });
        }
        assert!(commands.is_empty(), "a demo screen sends nothing by being drawn");
        texts
    }

    fn shows(texts: &[(String, egui::Rect)], part: &str) -> bool {
        texts.iter().any(|(t, _)| t.contains(part))
    }

    #[test]
    fn demo_accounts_and_accounts_models_build_and_open_the_popup() {
        // Every account is valid as settings keep them, so the screens show what the app can.
        let s = super::state("accounts").unwrap();
        assert_eq!(s.settings.clone().sanitized().accounts, s.settings.accounts);
        assert_eq!((s.view, s.customize_tab), (View::Customizations, CustomizeTab::Account));

        let mut s = super::state("accounts").unwrap();
        let texts = page(&mut s, 2);
        for part in [
            "an.nguyen@example.com",
            "dev@example.com · plus",
            "team@example.org · Acme Studio",
            "Antigravity · gemini-3-flash",
            "Current",
            "port 1455 on localhost is already in use",
            "Paste redirect link",
            "Sign in with your Claude Code account.",
            "These are consumer subscription sign-ins.",
            "Model list last checked ",
        ] {
            assert!(shows(&texts, part), "{part:?} missing from {texts:?}");
        }

        let mut s = super::state("accounts-models").unwrap();
        // The popup opens on the first frame and is drawn from the next one.
        let texts = page(&mut s, 3);
        assert!(s.open_account_models.is_none(), "consumed");
        assert!(shows(&texts, "Supported models · an.nguyen@example.com"), "{texts:?}");
        assert!(shows(&texts, "Supports 6/7 models · updated "), "{texts:?}");
        assert!(texts.iter().any(|(t, _)| t == "gemini-3.8-flash-high"), "a served chip");
        assert!(texts.iter().any(|(t, _)| t == "gemini-3.5-flash-lite"), "an unserved chip");
        // The running Claude sign-in, and the Codex error without its paste link.
        assert!(shows(&texts, "Finish the sign-in in your browser."));
        assert!(shows(&texts, "port 1455 on localhost is already in use") && !shows(&texts, "Paste redirect link"));
    }

    /// The conversation of a demo, drawn for two frames; the texts of the last one.
    fn conversation(s: &mut UiState) -> Vec<(String, egui::Rect)> {
        let ctx = context();
        let mut cache = egui_commonmark::CommonMarkCache::default();
        let mut commands = Vec::new();
        let mut texts = Vec::new();
        for _ in 0..2 {
            texts = painted(&ctx, Vec::new(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| crate::ui::conversation::show(ui, s, &mut cache, &mut commands));
            });
        }
        assert!(commands.is_empty(), "a demo screen sends nothing by being drawn");
        texts
    }

    /// The progress screen has the turn's steps in the open panel; the other conversation
    /// screens keep it closed, so their steps are only in the transcript.
    #[test]
    fn only_the_progress_screens_open_the_panel() {
        let mut s = super::state("progress").unwrap();
        let texts = conversation(&mut s);
        assert!(texts.iter().any(|(t, _)| t == "Progress") && texts.iter().any(|(t, _)| t == "8/8"), "{texts:?}");
        assert!(texts.iter().any(|(t, _)| t == "Ran python -m pytest -q"), "{texts:?}");
        let mut s = super::state("planning").unwrap();
        let texts = conversation(&mut s);
        assert!(texts.iter().any(|(t, _)| t == "Progress") && texts.iter().any(|(t, _)| t == "1/2"), "{texts:?}");
        for name in ["conversation", "turn-error", "review"] {
            let mut s = super::state(name).unwrap();
            assert!(!s.progress_open, "{name}");
            let texts = conversation(&mut s);
            assert!(!texts.iter().any(|(t, _)| t == "Progress"), "{name}: {texts:?}");
        }
    }

    /// The thought being reasoned is open; a finished thought is only its row, and the live one
    /// collapses as well once the turn ends.
    #[test]
    fn only_the_live_thought_is_open() {
        let shows = |texts: &[(String, egui::Rect)], t: &str| texts.iter().any(|(x, _)| x.contains(t));
        let mut s = super::state("thinking").unwrap();
        let texts = conversation(&mut s);
        assert!(shows(&texts, DEMO_LIVE_THOUGHT), "{texts:?}");
        assert!(!shows(&texts, DEMO_DONE_THOUGHT), "{texts:?}");
        assert!(shows(&texts, "Looking for build output"), "the finished thought keeps its row: {texts:?}");
        s.busy = false;
        let texts = conversation(&mut s);
        assert!(!shows(&texts, DEMO_LIVE_THOUGHT), "{texts:?}");
        // Anything after the thought closes it too, while the turn still runs.
        let mut s = super::state("confirm-delete").unwrap();
        let texts = conversation(&mut s);
        assert!(!shows(&texts, DEMO_LIVE_THOUGHT), "{texts:?}");
    }

    /// A shell line that deletes files is announced as a deletion, with the warning.
    #[test]
    fn a_shell_deletion_warns() {
        let mut s = super::state("confirm-delete").unwrap();
        let texts = conversation(&mut s);
        assert!(texts.iter().any(|(t, _)| t == "Delete files?"), "{texts:?}");
        assert!(texts.iter().any(|(t, _)| t.starts_with("This command deletes files.")), "{texts:?}");
        // An ordinary command keeps the ordinary card.
        let mut s = super::state("confirm-delete").unwrap();
        s.pending_confirm = Some(pending_shell("2", "cargo build"));
        let texts = conversation(&mut s);
        assert!(texts.iter().any(|(t, _)| t == "Run command?"), "{texts:?}");
        assert!(!texts.iter().any(|(t, _)| t.starts_with("This command deletes files.")), "{texts:?}");
    }

    #[test]
    fn the_login_demo_is_signed_out_of_everything() {
        let s = super::state("login").unwrap();
        assert!(s.accounts.is_empty() && s.settings.accounts.is_empty() && !s.any_backend_ready());
        assert_eq!(s.auth, AuthStatus::LoggedOut);
        // The other screens keep the Antigravity account in use.
        assert_eq!(super::state("home").unwrap().settings.active_account.as_deref(), Some(ANTIGRAVITY_ID));
    }
}

// --- Orchestration demos (`--demo planning`, `--demo review`) -------------------------------

/// Names of the orchestration demo screens handled by [`orchestration_state`].
#[cfg_attr(not(test), allow(dead_code))] // listed in main.rs USAGE by its owner
pub const ORCHESTRATION_NAMES: &[&str] = &["planning", "review"];

fn tool_card(id: &str, name: &str, args: serde_json::Value, result: Option<(bool, &str)>) -> ChatItem {
    ChatItem::Tool(ToolCard { call_id: id.into(), name: name.into(), args, result: result.map(|(ok, d)| (ok, d.to_owned())) })
}

fn boundary(id: &str, name: &str, summary: &str, phase: &str) -> ChatItem {
    tool_card(id, crate::agent::orchestrate::TASK_BOUNDARY, serde_json::json!({"task_name": name, "summary": summary, "phase": phase}), Some((true, "ok")))
}

const DEMO_TASK_MD: &str = "# Task: retry for the upload client\n\n- [x] Read the upload client and its callers\n- [x] Write the implementation plan\n- [/] Add `RetryPolicy` with exponential backoff\n- [ ] Retry only idempotent requests (PUT, GET)\n- [ ] Unit tests with a mock server\n- [ ] Walkthrough\n";

const DEMO_PLAN_MD: &str = "# Implementation Plan: retry uploads\n\n## Goal\nUploads fail on short network drops. Retry idempotent requests with backoff, without changing the public API.\n\n## Proposed changes\n\n### `src/upload/client.rs`\n- Add `RetryPolicy { max_attempts: 4, base_delay: 250 ms, max_delay: 4 s }`.\n- Wrap `send_chunk` in a retry loop; retry on timeouts, 429 and 5xx only.\n- Honor `Retry-After` when the server sends it.\n\n### `src/upload/config.rs`\n- New optional `retry` section; defaults keep today's behavior plus retries.\n\n## Verification\n1. `cargo test upload::` with a mock server that drops the first two requests.\n2. Manual run against the staging bucket with the network throttled.\n";

const DEMO_PLAN_V1_MD: &str = "# Implementation Plan: retry uploads\n\n## Goal\nRetry failed uploads.\n";

/// Planning turn: research group done, plan approved, execution group running.
fn planning_items() -> Vec<ChatItem> {
    use crate::agent::orchestrate::{REQUEST_REVIEW, WRITE_ARTIFACT};
    vec![
        ChatItem::User("Add retry with backoff to the upload client. Không đổi API public nhé.".into()),
        ChatItem::TurnHeader { duration: None },
        boundary("p1", "Research the upload client", "Reading the client and its callers", "planning"),
        tool_card("1", "read_file", serde_json::json!({"file_path": "src/upload/client.rs"}), Some((true, "Read 214 lines"))),
        tool_card("2", "grep_search", serde_json::json!({"pattern": "send_chunk"}), Some((true, "6 matches"))),
        boundary("p2", "Research the upload client", "Found 3 call sites; chunks are sent with PUT", "planning"),
        tool_card("3", "read_file", serde_json::json!({"file_path": "src/upload/config.rs"}), Some((true, "Read 58 lines"))),
        tool_card("4", WRITE_ARTIFACT, serde_json::json!({"name": "task", "content": DEMO_TASK_MD, "summary": "6 steps"}), Some((true, "v1"))),
        tool_card("5", WRITE_ARTIFACT, serde_json::json!({"name": "implementation_plan", "content": DEMO_PLAN_MD, "summary": "Retry idempotent requests with backoff"}), Some((true, "v1"))),
        tool_card("6", REQUEST_REVIEW, serde_json::json!({"artifacts": ["implementation_plan"], "message": "Plan ready"}), Some((true, "approved"))),
        ChatItem::Assistant("Plan approved. Implementing it step by step.".into()),
        boundary("e1", "Add RetryPolicy", "Adding the policy type and defaults", "executing"),
        tool_card("7", "replace", serde_json::json!({"file_path": "src/upload/config.rs", "old_string": "pub struct UploadConfig {", "new_string": "pub struct UploadConfig {\n    #[serde(default)]\n    pub retry: RetryPolicy,"}), Some((true, "ok"))),
        tool_card("8", "write_file", serde_json::json!({"file_path": "src/upload/retry.rs", "content": "pub struct RetryPolicy;\n\nimpl RetryPolicy {}\n"}), Some((true, "ok"))),
        boundary("e2", "Add RetryPolicy", "Wrapping send_chunk in the retry loop", "executing"),
        tool_card("9", "replace", serde_json::json!({"file_path": "src/upload/client.rs", "old_string": "self.send_chunk(c)", "new_string": "retry(&self.policy, || self.send_chunk(c))"}), None),
    ]
}

/// Review pending: the plan (v2) waits for Proceed / Request changes; the panel opens itself.
fn review_items() -> Vec<ChatItem> {
    use crate::agent::orchestrate::{REQUEST_REVIEW, WRITE_ARTIFACT};
    let task = DEMO_TASK_MD.replace("[/]", "[ ]").replace("- [x] Write", "- [/] Write");
    vec![
        ChatItem::User("Add retry with backoff to the upload client. Không đổi API public nhé.".into()),
        ChatItem::TurnHeader { duration: None },
        boundary("p1", "Research the upload client", "Reading the client and its callers", "planning"),
        tool_card("1", "read_file", serde_json::json!({"file_path": "src/upload/client.rs"}), Some((true, "Read 214 lines"))),
        tool_card("2", "grep_search", serde_json::json!({"pattern": "send_chunk"}), Some((true, "6 matches"))),
        boundary("p2", "Research the upload client", "Found 3 call sites; chunks are sent with PUT", "planning"),
        boundary("p3", "Write the plan", "Drafting task list and implementation plan", "planning"),
        tool_card("3", WRITE_ARTIFACT, serde_json::json!({"name": "task", "content": task, "summary": "6 steps"}), Some((true, "v1"))),
        tool_card("4", WRITE_ARTIFACT, serde_json::json!({"name": "implementation_plan", "content": DEMO_PLAN_V1_MD}), Some((true, "v1"))),
        tool_card("5", WRITE_ARTIFACT, serde_json::json!({"name": "implementation_plan", "content": DEMO_PLAN_MD, "summary": "Retry idempotent requests with backoff"}), Some((true, "v2"))),
        boundary("p4", "Write the plan", "Waiting for your review", "planning"),
        ChatItem::Assistant("The plan is ready. It only touches `client.rs` and `config.rs`; the public API stays the same.".into()),
        tool_card("6", REQUEST_REVIEW, serde_json::json!({"artifacts": ["implementation_plan"], "message": "Retry plan for the upload client: review before I edit code."}), None),
    ]
}

/// Orchestration demo state `name` (fallback of [`state`]).
pub fn orchestration_state(name: &str) -> Option<UiState> {
    let items = match name {
        "planning" => planning_items(),
        "review" => review_items(),
        _ => return None,
    };
    let mut s = base();
    let c = &s.conversations[1];
    s.current = Some(c.id);
    s.current_title = c.title.clone();
    s.running = Some(c.id);
    s.items = items;
    s.busy = true;
    // The planning screen shows the turn's task groups in the Progress panel as well.
    s.progress_open = name == "planning";
    Some(s)
}

#[cfg(test)]
mod orchestration_tests {
    use crate::agent::artifacts::ReviewState;
    use crate::agent::orchestrate::TaskStatus;
    use crate::ui::{artifacts_panel, taskgroups};

    #[test]
    fn orchestration_fixtures_build() {
        for n in super::ORCHESTRATION_NAMES {
            assert!(super::state(n).is_some(), "{n}");
        }
    }

    #[test]
    fn review_fixture_has_pending_plan() {
        let s = super::state("review").unwrap();
        let d = artifacts_panel::derive_fresh(&s.items, s.busy);
        let (a, p) = (d.artifacts, d.pending);
        assert_eq!(p.map(|p| p.call_id), Some("6".into()));
        let plan = a.iter().find(|a| a.name == "implementation_plan").unwrap();
        assert_eq!((plan.version, plan.review), (2, ReviewState::Pending));
        let g = taskgroups::derive(&s.items, s.busy, false);
        assert_eq!(g.groups.len(), 2);
        assert_eq!(g.groups[0].status, TaskStatus::Completed);
    }

    #[test]
    fn planning_fixture_groups() {
        let s = super::state("planning").unwrap();
        let g = taskgroups::derive(&s.items, s.busy, false);
        assert_eq!(g.groups.len(), 2);
        assert_eq!(g.groups[1].status, TaskStatus::InProgress);
        assert_eq!(g.groups[1].edited_files, vec!["src/upload/config.rs", "src/upload/retry.rs", "src/upload/client.rs"]);
    }
}
