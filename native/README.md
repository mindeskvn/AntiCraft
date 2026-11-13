# AntiCraft native

## Tiếng Việt

### Đây là gì
AntiCraft native (`anticraft-native`) là một ứng dụng desktop viết bằng Rust (egui). Nó là một trợ lý lập trình: trò chuyện với các mô hình Gemini qua Google Code Assist API và có thể đọc, tìm, sửa file và chạy lệnh PowerShell trong một thư mục workspace do bạn chọn. Hành vi giao thức tham khảo từ gemini-cli (Apache-2.0) và được viết lại hoàn toàn; xem `NOTICE` và `PROTOCOL.md`.

### Build và chạy
```
cargo build --release --manifest-path native/Cargo.toml
target\release\anticraft-native.exe [--workspace DIR] [--model ID] [--data-dir DIR] [--logout]
```
### OAuth client secret
Đăng nhập Antigravity dùng OAuth client riêng của Antigravity. Bản exe trong Releases đã có sẵn client này, bạn không cần làm gì. Secret của client này không nằm trong mã nguồn: khi tự build, đặt biến môi trường `ANTICRAFT_ANTIGRAVITY_CLIENT_SECRET`, hoặc ghi dòng `ANTICRAFT_ANTIGRAVITY_CLIENT_SECRET=giá trị` vào file `native/antigravity-client.local.env` (git bỏ qua file này; tạo file xong thì `touch native/build.rs` để build đọc lại). Thiếu nó thì bản build vẫn chạy, chỉ riêng đăng nhập Antigravity báo lỗi.

Chỉ đăng nhập Gemini CLI (Customizations > Account > "Gemini CLI sign-in (Google)") cần client secret của OAuth client cài đặt công khai của gemini-cli. Có hai cách đặt secret:
- Trong app: Customizations > Account, nhập vào ô "Client secret" rồi bấm Save (hoặc Enter). Giá trị được lưu vào `oauth_client.json` trong thư mục dữ liệu, chỉ tài khoản Windows hiện tại đọc được. Nút Clear xóa nó.
- Biến môi trường `ANTICRAFT_OAUTH_CLIENT_SECRET`, lúc chạy hoặc lúc build.

Thứ tự ưu tiên: file đã lưu, rồi biến môi trường lúc chạy, rồi giá trị lúc build. Giá trị không bao giờ được ghi log hay hiển thị lại.

### Giao diện
- Thanh trên: nút quay lại/tiến tới, tiêu đề hội thoại (bấm để đổi tên, Esc để hủy), menu `...` (Rename, Copy as Markdown, Delete cần bấm hai lần và bị khóa khi đang chạy).
- Sidebar: Conversation History, Customizations, danh sách hội thoại nhóm theo project, hàng tài khoản ở dưới cùng.
- Home: ô nhập ở giữa. Conversation: cột đọc với tin nhắn markdown, code block, thẻ tool call, tiêu đề lượt "Worked for ...", composer gắn ở dưới với chọn model và nút Stop.
- Xác nhận tool hiện ngay trong transcript (diff hoặc lệnh) với Approve / Reject / Always allow trong composer. Enter duyệt, Esc từ chối (khi không có menu nào đang mở).
- Nút dưới câu trả lời cuối: Copy và "Send again" (gửi lại câu hỏi cuối thành một lượt mới).
- Model hiện trên chip là model lượt tiếp theo dùng, kể cả khi mở hội thoại cũ.
- Icon dùng phông Phosphor (MIT).

### Demo và ảnh chụp
Chạy bằng dữ liệu giả, không đăng nhập, không gọi mạng:
```
anticraft-native.exe --demo home|conversation|model-menu|history|customizations|login|accounts|accounts-models [--screenshot OUT.png]
```
Có `--screenshot` thì app lưu PNG rồi thoát. Ảnh hiện tại nằm ở `target\shots\`.

### Đăng nhập lần đầu
Nút "Sign in" ở sidebar và ở Home là đăng nhập **Antigravity**: dùng OAuth client riêng của Antigravity, không cần client secret. Trình duyệt mở trang đồng ý của Google, Google chuyển hướng về `http://localhost:51121/oauth-callback`. Nếu cổng bận hoặc trình duyệt không quay về được, dán địa chỉ trang trình duyệt dừng lại vào "Paste redirect link" trên hàng Antigravity. Tài khoản tự làm mới token và được dùng cho các lượt chat.

Đăng nhập Gemini CLI (Customizations > Account > "Gemini CLI sign-in (Google)") là đường cũ qua OAuth client của gemini-cli: cần client secret ở trên, chuyển hướng về `http://127.0.0.1:<port>/oauth2callback` (PKCE + state, kết nối rác hoặc state sai bị bỏ qua). Token được lưu vào file, lần chạy sau được kiểm tra và tự làm mới. Lỗi `invalid_grant` buộc bạn đăng nhập lại.

### Công cụ và mô hình xác nhận
- Đọc/tìm: `read_file`, `list_directory`, `glob`, `grep_search` (bí danh `search_file_content`). Mặc định chạy không hỏi. Nếu tắt "Run read-only tools without asking" thì mỗi lần gọi đều hỏi.
- Sửa: `write_file`, `replace`. Luôn hiện diff để duyệt. Nếu file thay đổi sau khi bạn duyệt, thao tác bị từ chối. Ghi file theo kiểu atomic.
- Shell: `run_shell_command` (PowerShell). Luôn hỏi. "Always allow" chỉ áp dụng cho lệnh đơn có cùng từ đầu tiên; lệnh ghép (`;`, `|`, `&`, `$()`, chuyển hướng, khối `{}`) luôn hỏi lại. Hết giờ hoặc Stop sẽ dừng toàn bộ cây tiến trình (Job Object).
- Mọi đường dẫn bị giới hạn trong workspace, kể cả symlink/junction treo.

### Dữ liệu lưu ở đâu
`%APPDATA%\AntiCraft\native\` (đổi được bằng `--data-dir`):
- `oauth_creds.json`: token của đăng nhập Gemini CLI. ACL chỉ cho người dùng hiện tại, áp ngay từ lúc tạo file. Thư mục dữ liệu mới tạo cũng có ACL chỉ cho người dùng hiện tại.
- `accounts\{id}.json`: token của từng tài khoản thuê bao (Antigravity, Codex, Claude Code), cũng chỉ người dùng hiện tại đọc được.
- `oauth_client.json`: client secret nhập trong Customizations.
- `settings.json`
- `conversations\{id}.json`
- `logs\native.log`: không ghi token.

Ứng dụng không đọc và không ghi `~/.gemini`.

### Rủi ro tài khoản
Ứng dụng dùng API Code Assist **không công khai** (`cloudcode-pa.googleapis.com/v1internal`) qua OAuth client công khai của gemini-cli. Đây không phải sản phẩm của Google, và Google có thể đổi API, giới hạn hoặc khóa tài khoản bất cứ lúc nào. Bạn tự chịu rủi ro.

**Tài khoản thuê bao** (Customizations > Account > Accounts: Antigravity, ChatGPT Codex, Claude Code). Đây là đăng nhập dành cho gói thuê bao cá nhân. Nhà cung cấp có thể hạn chế ứng dụng bên thứ ba: yêu cầu bị từ chối sẽ hiện trên hàng của tài khoản đó, và API key trên tab Providers là cách thay thế. Dùng gói thuê bao theo cách này có thể vi phạm điều khoản của nhà cung cấp và khiến tài khoản gặp rủi ro.

AntiCraft gửi danh tính của chính nó (User-Agent `anticraft-native/...`), không giả làm ứng dụng chính thức. Ngoại lệ duy nhất là dòng system `You are Claude Code, Anthropic's official CLI for Claude.` trên đường OAuth của Claude, vì máy chủ của Anthropic từ chối suy luận nếu thiếu nó; User-Agent, header billing và chữ ký request của CLI chính thức không được gửi. Token chỉ nằm trong `accounts\{id}.json` (chỉ người dùng hiện tại đọc được), không bao giờ vào log, URL hay giao diện, và không có gì được ghi vào `~/.gemini`, `~/.codex` hay `~/.claude`. Mỗi tài khoản được đăng xuất riêng trên tab Account; `--logout` chỉ xóa đăng nhập Gemini CLI.

### Bypass Mode
Customizations > Tools and approvals > Bypass Mode cho phép gửi **những request đầu tiên của mỗi tin nhắn** tới một provider profile bạn tự chọn, sau đó cuộc hội thoại quay lại model đang dùng. Số request chỉnh được (1-50). Công cụ, lịch sử và phê duyệt giữ nguyên.

- Chọn provider trong mục "Bypass provider"; có thể thêm profile mới ngay tại đó.
- "Instruction files" nhận các file `.md`; nội dung của chúng chỉ được chèn vào system instruction của các request đi qua provider đó, không vào lịch sử hội thoại và không tới model bình thường.
- Provider đó **thấy nội dung** của những request được gửi tới nó, theo điều khoản của chính nó. Trong lúc bật, khung chat luôn có dải cảnh báo đỏ nêu tên provider, model, số request và model tiếp nhận, kèm nút "Turn off".
- Đổi chế độ chỉ ảnh hưởng tin nhắn tiếp theo; tin nhắn đang chạy giữ lộ trình lúc nó bắt đầu.

### Hạn chế đã biết
- Ứng dụng không tự phát hiện quyền truy cập mô hình preview hay Flash GA mới nhất; hai cài đặt này phải chỉnh tay trong Settings.
- Khi hết quota, ứng dụng không tự chuyển từ Pro sang Flash.
- Không có MCP, web fetch, web search hay memory tool.
- Kiểm tra lệnh ghép dựa trên ký tự, không phải bộ phân tích cú pháp PowerShell đầy đủ, nên có thể hỏi thừa.
- Lúc sửa file, đường dẫn được kiểm tra lại và nội dung so bằng hash, nhưng vẫn còn một khoảng hở TOCTOU rất nhỏ (file không mở với no-follow).
- Bản unix được viết sẵn nhưng chỉ được kiểm thử trên Windows.

## English

### What it is
AntiCraft native (`anticraft-native`) is a Rust/egui desktop coding assistant. It chats with Gemini models through the Google Code Assist API and can read, search and edit files and run PowerShell commands inside a workspace folder you choose. Protocol behavior was studied from gemini-cli (Apache-2.0) and reimplemented. See `NOTICE` and `PROTOCOL.md`.

### Build and run
```
cargo build --release --manifest-path native/Cargo.toml
target\release\anticraft-native.exe [--workspace DIR] [--model ID] [--data-dir DIR] [--logout]
```
Tests use mock servers only. Set `ANTICRAFT_LOG=debug` for more logging.

### OAuth client secret
The Antigravity sign-in uses Antigravity's own OAuth client. The exe on the Releases page has it built in, so there is nothing to set. The client's secret is not in the source: when you build it yourself, set `ANTICRAFT_ANTIGRAVITY_CLIENT_SECRET`, or write the line `ANTICRAFT_ANTIGRAVITY_CLIENT_SECRET=value` into `native/antigravity-client.local.env` (git ignores that file; after creating it, touch `native/build.rs` so the build reads it). Without it the build still runs, and only the Antigravity sign-in reports an error.

Only the Gemini CLI sign-in (Customizations > Account > "Gemini CLI sign-in (Google)") needs the gemini-cli public installed-app client secret. Set it either way:
- In the app: Customizations > Account, type it into "Client secret" and press Save (or Enter). It is stored in `oauth_client.json` in the data folder with an owner-only DACL. Clear removes it. The field's undo history is dropped after saving.
- The `ANTICRAFT_OAUTH_CLIENT_SECRET` environment variable, at run time or build time.

Precedence: saved file, then run-time env, then build-time value. The value is never logged or shown again.

### UI overview
- Top bar: back/forward, conversation title (click to rename, Esc cancels), `...` menu with Rename, Copy as Markdown and Delete (two clicks, disabled while a turn runs).
- Sidebar: Conversation History, Customizations, conversations grouped by project, account row at the bottom.
- Home: centered composer. Conversation: reading column with markdown, code blocks, tool-call cards, "Worked for ..." turn headers, and a docked composer with the model picker and Stop.
- Tool confirmations appear inline in the transcript (diff or command preview) with Approve / Reject / Always allow in the composer. Enter approves and Esc rejects, unless a menu is open.
- Under the last answer: Copy and "Send again" (re-sends the last question as a new turn).
- The model chip is always the model the next turn uses, including after opening an older conversation.
- Icons come from the Phosphor icon font (MIT).

### Demo and screenshots
Fixture data only, no sign-in, no network:
```
anticraft-native.exe --demo home|conversation|model-menu|history|customizations|login|accounts|accounts-models [--screenshot OUT.png]
```
With `--screenshot` the app saves a PNG and exits. Current renders are in `target\shots\`.

### First login
"Sign in" in the sidebar and on Home is the **Antigravity** sign-in. It uses Antigravity's own OAuth client and needs no client secret.
1. Your browser opens Google consent and Google redirects to `http://localhost:51121/oauth-callback`. Forged states are ignored.
2. If the port is busy or the browser cannot come back, paste the address the browser landed on into "Paste redirect link" on the Antigravity row. The state is checked the same way.
3. The account refreshes its own tokens and sends your turns.

The Gemini CLI sign-in (Customizations > Account > "Gemini CLI sign-in (Google)") is the older path through gemini-cli's OAuth client. It needs the client secret above:
1. Click its "Sign in". Your browser opens Google consent (PKCE plus a random state).
2. Google redirects to `http://127.0.0.1:<port>/oauth2callback`. The app keeps accepting connections until a request with the exact path and the matching state arrives. Preconnects, favicon requests and forged states are ignored. Set `OAUTH_CALLBACK_PORT` to pin the port.
3. Tokens are stored, verified at the next start and refreshed before they expire. A 401 forces one refresh and a retry. `invalid_grant` asks you to log in again.

### Tools and confirmation model
- **Read/search**: `read_file`, `list_directory`, `glob`, `grep_search` (alias `search_file_content`). They run without asking unless you turn off "Run read-only tools without asking"; then every call shows a prompt. Sizes are capped: 20 MB per read, 2000 chars per line, 256 KB of output.
- **Edit**: `write_file`, `replace`. These always show a diff. The approval is tied to the previewed file hash and resolved path, so a file that changed after approval is not written. Writes are atomic.
- **Shell**: `run_shell_command` (PowerShell). This always asks. "Always allow" covers only simple commands with the same first word. Compound commands (`;`, `|`, `&`, backtick, `$()`, `@()`, redirection, script blocks, newlines) always ask again. The model's description is shown as unverified, below the command. Output is capped while it streams. A timeout or Stop kills the whole process tree (Windows Job Object).
- Every path is confined to the workspace. Dangling symlinks and junctions are rejected.
- Rejecting a call stops the turn. Stop cancels streaming, setup, confirmations and tools. The history stays a well-formed user/model alternation, and partial replies are kept.

### Models
The dropdown offers `auto`, Pro, Flash and Flash-lite IDs. `auto`/`pro` resolve to `gemini-3-pro-preview` when "preview access" is on, otherwise to `gemini-2.5-pro`. `gemini-3.5-flash` is sent as `gemini-3-flash`, or as `gemini-3.8-flash` when "latest Flash GA" is on (PROTOCOL 3.11). Retryable errors (429 with RetryInfo, 5xx, network) are retried with backoff while nothing has been shown yet.

### Bypass Mode
Customizations > Tools and approvals > Bypass Mode sends the **first requests of each message** to a provider profile you choose, then the conversation returns to the model in use. The count is configurable (1-50). Tools, history and approvals are unchanged.

- Pick the profile under "Bypass provider"; a new one can be added right there.
- "Instruction files" takes `.md` files. Their text is added to the system instruction of the requests that go to that provider only: never to the conversation history, and never to the normal model.
- That provider **sees the content** of the requests sent to it, under its own terms. While the mode is on, the composer always carries a red band naming the provider, its model, the request count and the model that takes over, with a "Turn off" link.
- Switching the mode affects the next message; a message already running keeps the route it began with.

### Where data is stored
`%APPDATA%\AntiCraft\native\` (override with `--data-dir`):
- `oauth_creds.json`: the Gemini CLI sign-in's tokens. The file is created with a protected DACL for the current user's SID only (mode 0600 on unix). A newly created data folder also gets an owner-only, inheritable DACL (an existing `--data-dir` keeps its ACL).
- `accounts\{id}.json`: one file per subscription account (Antigravity, Codex, Claude Code), owner-only as well.
- `oauth_client.json`: the client secret entered in Customizations.
- `settings.json`
- `conversations\{id}.json`
- `logs\native.log`: never contains tokens.

`~/.gemini` is never read or written.

### Account risk
This app calls the **non-public** Code Assist API (`cloudcode-pa.googleapis.com/v1internal`) using gemini-cli's public OAuth client. It is not a Google product. Google may change the API, rate-limit or restrict accounts that use it at any time. Use at your own risk.

### Known gaps
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
