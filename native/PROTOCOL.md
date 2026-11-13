# AntiCraft protocol spec

Reference: google-gemini/gemini-cli (Copyright 2025 Google LLC, Apache-2.0), local copy at `C:\Users\desktop\Desktop\Anti_REA\_ref\gemini-cli`. Paths are relative to `packages/core/src/` unless noted. This file describes behavior for AntiCraft to reimplement in idiomatic Rust. It is not a port of the TypeScript. Attribution goes in `NOTICE`.

The Code Assist API (`cloudcode-pa.googleapis.com/v1internal`) is internal and undocumented. The field sets below are the subset gemini-cli uses, and they can change.

Ground rules:
- Never log, print or commit access tokens, refresh tokens, the client secret, or the `Authorization` header.
- Every base URL must be configurable: OAuth auth, token, userinfo and tokeninfo, plus the Code Assist endpoint. Tests run against a mock HTTP server. They never do a real login and never call Google with real credentials.

---

## 1. Auth (Google OAuth)

Sources: `code_assist/oauth2.ts`, `code_assist/oauth-credential-storage.ts`, `config/storage.ts`, `utils/paths.ts`, `utils/userAccountManager.ts`, `mcp/token-storage/*`, `services/fileKeychain.ts`, `packages/cli/src/ui/commands/authCommand.ts`.
The HTTP mechanics come from `google-auth-library` 10.9.0, pinned in `packages/core/package.json` but not vendored. Items marked [lib] are that library's documented defaults. They were not read from repo source.

### 1.1 Client credentials
- Google "installed application" (Desktop) client. The constants are `OAUTH_CLIENT_ID` and `OAUTH_CLIENT_SECRET` in `oauth2.ts`. Google's installed-app guidance says this secret is not confidential. AntiCraft still must not echo it.
- `client_id` = `681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com`
- `client_secret`: the `GOCSPX-…` value of `OAUTH_CLIENT_SECRET`. Load it as a build-time constant or config value. It is left out of this file on purpose.
- Scopes (space-joined): `https://www.googleapis.com/auth/cloud-platform`, `https://www.googleapis.com/auth/userinfo.email`, `https://www.googleapis.com/auth/userinfo.profile`.

### 1.2 Browser flow with loopback redirect (`authWithWeb`)
1. Port: use `OAUTH_CALLBACK_PORT` if set (1–65535, otherwise error). Otherwise pick a free port. gemini-cli probes port 0 and then rebinds. AntiCraft binds once and keeps the listener open, which avoids the race.
2. Bind host: `OAUTH_CALLBACK_HOST` or `127.0.0.1`. `redirect_uri` is always `http://127.0.0.1:{port}/oauth2callback`.
3. `state` = 32 random bytes as hex (64 chars). Compare it in constant time.
4. gemini-cli sends no PKCE in this flow. AntiCraft adds S256 PKCE, which Google accepts.
5. [lib] `GET https://accounts.google.com/o/oauth2/v2/auth` with `client_id`, `redirect_uri`, `response_type=code`, `access_type=offline`, `scope`, `state`. AntiCraft also sends `code_challenge` and `code_challenge_method=S256`. gemini-cli does not send `prompt=consent`, so Google normally returns a refresh token only on the first consent.
6. Open the system browser. The overall timeout is 5 minutes. A user cancel aborts the flow.
7. Callback handling (one request, then the server closes):
   - Path does not contain `/oauth2callback` → `301 Location: https://developers.google.com/gemini-code-assist/auth_failure_gemini`, fail.
   - `error` param present → 301 to the failure URL. Error text = `error` + `error_description`.
   - `state` mismatch → body `State mismatch. Possible CSRF attack`, fail.
   - `code` present → exchange it (1.4), fetch user info (1.7, non-fatal), then `301 Location: https://developers.google.com/gemini-code-assist/auth_success_gemini`.
   - Anything else → fail ("No authorization code received").

### 1.3 Manual user-code flow (`authWithUserCode`, used when there is no browser)
- `redirect_uri` = `https://codeassist.google.com/authcode`. Google's page shows the code for the user to copy.
- PKCE S256 (`code_challenge` = BASE64URL(SHA256(verifier))), plus `state` and `access_type=offline`.
- The user pastes the code (5-minute timeout, 2 attempts). The exchange includes `code_verifier`. This flow needs an interactive session.
- In a GUI app this is only a fallback. The loopback flow is primary.

### 1.4 Token exchange [lib]
`POST https://oauth2.googleapis.com/token`, `Content-Type: application/x-www-form-urlencoded`:
```
code={code}&client_id={id}&client_secret={secret}&redirect_uri={same redirect_uri}&grant_type=authorization_code[&code_verifier={verifier}]
```
Response:
```json
{"access_token":"ya29…","expires_in":3599,"refresh_token":"1//…","scope":"…","token_type":"Bearer","id_token":"eyJ…"}
```
Store `expiry_date` = now_ms + expires_in*1000. Error body: `{"error":"invalid_grant","error_description":"…"}`.

### 1.5 Refresh [lib]
`POST https://oauth2.googleapis.com/token` (form): `refresh_token={rt}&client_id={id}&client_secret={secret}&grant_type=refresh_token`.
- The response usually has no `refresh_token`. Keep the old one (merge-on-write).
- Refresh when `expiry_date` is within about 5 minutes or already past. Also refresh once on a 401, then retry the request once.
- Persist every new token set.
- `invalid_grant` means the refresh token was revoked or expired. Require a new login.

Startup check of cached credentials (`initOauthClient`): get an access token (refreshing if needed), then call [lib] `GET https://oauth2.googleapis.com/tokeninfo?access_token=…` to confirm it is not revoked. On failure, fall back to interactive login.

gemini-cli checks credential sources in this order: `GOOGLE_GENAI_USE_GCA` + `GOOGLE_CLOUD_ACCESS_TOKEN` env override → encrypted store (if forced) → `~/.gemini/oauth_creds.json` → `GOOGLE_APPLICATION_CREDENTIALS`. This is for reference only. AntiCraft needs just its own store.

### 1.6 Credential storage
gemini-cli's default file is `{home}/.gemini/oauth_creds.json`, where `{home}` is `GEMINI_CLI_HOME` or the OS home. It holds the pretty-printed google-auth-library `Credentials` with nulls removed. New fields are shallow-merged over the existing file. The write is atomic (temp file + rename) with mode 0600:
```json
{"access_token":"ya29…","refresh_token":"1//…","scope":"…","token_type":"Bearer","id_token":"eyJ…","expiry_date":1761400000000}
```
`expiry_date` is epoch milliseconds. The encrypted alternative (`GEMINI_FORCE_ENCRYPTED_FILE_STORAGE=true`) uses the OS keychain (service `gemini-cli-oauth`, key `main-account`) or an AES-256-GCM file fallback.

Account cache, `{home}/.gemini/google_accounts.json`: `{"active":"user@example.com","old":["previous@example.com"]}`.

AntiCraft decision: keep its own credentials separate, either in the OS credential manager or in an ACL-protected file under `%APPDATA%\AntiCraft`. Never read or write `~/.gemini`. Use the same merge and atomic-write behavior.

### 1.7 User info
`GET https://www.googleapis.com/oauth2/v2/userinfo` with `Authorization: Bearer {access_token}`. Only `email` is used. A non-2xx response or network error is logged (no token) and ignored.

### 1.8 Logout
gemini-cli deletes the credential file or keychain entry, then clears `active` in the account cache and the in-memory clients. It does not revoke on the server. Optional for AntiCraft [lib]: `POST https://oauth2.googleapis.com/revoke` with form body `token={token}`.

---

## 2. Code Assist API (cloudcode-pa `v1internal`)

Sources: `code_assist/{server.ts,setup.ts,types.ts,converter.ts,codeAssist.ts}`, `code_assist/experiments/{types.ts,client_metadata.ts}`, `core/contentGenerator.ts`, `utils/{retry.ts,googleQuotaErrors.ts,customHeaderUtils.ts,surface.ts}`, `billing/billing.ts`, `config/config.ts`.

### 2.1 URLs
| Item | Value |
|---|---|
| Endpoint | `https://cloudcode-pa.googleapis.com` (override: env `CODE_ASSIST_ENDPOINT`) |
| Version | `v1internal` (override: env `CODE_ASSIST_API_VERSION`) |
| Base | `{endpoint}/{version}` |
| Method URL | `{base}:{method}`, e.g. `…/v1internal:loadCodeAssist` |
| Operation URL | `{base}/{operationName}`, with the name appended as-is, e.g. `…/v1internal/operations/abc123` |

Related domains used in error classification: `cloudcode-pa.googleapis.com`, `staging-cloudcode-pa.googleapis.com`, `autopush-cloudcode-pa.googleapis.com`.

### 2.2 Headers
- `Authorization: Bearer <access_token>`, added and refreshed by the client (see 1.5).
- `Content-Type: application/json`, also on GET.
- `User-Agent`. gemini-cli sends `GeminiCLI/{version}/{model} ({platform}; {arch}; {surface})`. AntiCraft sends its own honest UA, e.g. `AntiCraft/{ver} (windows; x86_64)`, and does not impersonate Antigravity or gemini-cli.
- Optional extra headers (gemini-cli reads them from env `GEMINI_CLI_CUSTOM_HEADERS`).
- No API key header. `x-goog-api-key` is only for the public Gemini API.

### 2.3 ClientMetadata (sent in request bodies)
```json
{"ideType":"IDE_UNSPECIFIED","ideVersion":"0.1.0","pluginVersion":"…","platform":"WINDOWS_AMD64","updateChannel":"stable","duetProject":"my-project-123","pluginType":"GEMINI","ideName":"IDE_UNSPECIFIED"}
```
- `ideType`: `IDE_UNSPECIFIED | VSCODE | INTELLIJ | VSCODE_CLOUD_WORKSTATION | INTELLIJ_CLOUD_WORKSTATION | CLOUD_SHELL | GEMINI_CLI`
- `platform`: `PLATFORM_UNSPECIFIED | DARWIN_AMD64 | DARWIN_ARM64 | LINUX_AMD64 | LINUX_ARM64 | WINDOWS_AMD64`
- `pluginType`: `PLUGIN_UNSPECIFIED | CLOUD_CODE | GEMINI | AIPLUGIN_INTELLIJ | AIPLUGIN_STUDIO`

Setup calls send the minimal form `{ideType:"IDE_UNSPECIFIED", platform:"PLATFORM_UNSPECIFIED", pluginType:"GEMINI"}`, plus `duetProject` when a project is known. Experiments and metrics send the full form.

### 2.4 Methods
All methods are `POST {base}:{method}` with a JSON body unless noted.

| Method | HTTP | Use | AntiCraft |
|---|---|---|---|
| `loadCodeAssist` | POST | tier and project discovery, health check | required |
| `onboardUser` | POST | onboarding, returns an LRO | required |
| `{operationName}` | GET `{base}/{name}` | poll the LRO | required |
| `streamGenerateContent?alt=sse` | POST | streaming chat | required |
| `generateContent` | POST | non-streaming chat (utility calls) | optional |
| `countTokens` | POST | token count (compression) | optional |
| `retrieveUserQuota` | POST | quota buckets | optional |
| `listExperiments` | POST | feature flags | skip in v1 |
| `fetchAdminControls` | POST | enterprise policy | skip in v1 |
| `getCodeAssistGlobalUserSetting` | GET `{base}:getCodeAssistGlobalUserSetting` | user setting | skip in v1 |
| `setCodeAssistGlobalUserSetting` | POST | user setting | skip in v1 |
| `recordCodeAssistMetrics` | POST | telemetry | do not send |

`embedContent` is not supported.

### 2.5 loadCodeAssist
Request:
```json
{"cloudaicompanionProject":"my-project-123","metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI","duetProject":"my-project-123"},"mode":"HEALTH_CHECK"}
```
- Leave out `cloudaicompanionProject` and `duetProject` when there is no user project. In Rust: `skip_serializing_if = "Option::is_none"`.
- `mode`: `MODE_UNSPECIFIED | FULL_ELIGIBILITY_CHECK | HEALTH_CHECK`. Setup omits it.

Response:
```json
{
  "currentTier":{"id":"free-tier","name":"Gemini Code Assist for individuals","hasOnboardedPreviously":true},
  "allowedTiers":[{"id":"free-tier","name":"…","description":"…","isDefault":true,"userDefinedCloudaicompanionProject":false,"privacyNotice":{"showNotice":true,"noticeText":"…"},"hasAcceptedTos":true}],
  "ineligibleTiers":[{"reasonCode":"VALIDATION_REQUIRED","reasonMessage":"…","tierId":"standard-tier","tierName":"…","validationUrl":"https://…","validationUrlLinkText":"…","validationErrorMessage":"…","validationLearnMoreUrl":"…","validationLearnMoreLinkText":"…"}],
  "cloudaicompanionProject":"managed-project-abc",
  "paidTier":{"id":"…","name":"…","availableCredits":[{"creditType":"GOOGLE_ONE_AI","creditAmount":"1000"}]}
}
```
- Every tier field is optional, and some can be null.
- Tier IDs: `free-tier`, `legacy-tier`, `standard-tier`. Treat them as open strings.
- `IneligibleTierReasonCode`: `DASHER_USER, INELIGIBLE_ACCOUNT, NON_USER_ACCOUNT, RESTRICTED_AGE, RESTRICTED_NETWORK, UNKNOWN, UNKNOWN_LOCATION, UNSUPPORTED_LOCATION, VALIDATION_REQUIRED`. This set is also open.
- `CreditType`: `CREDIT_TYPE_UNSPECIFIED | GOOGLE_ONE_AI`. `creditAmount` is an int64 sent as a JSON string.
- If an error has `error.details[].reason == "SECURITY_POLICY_VIOLATED"` (VPC-SC), treat the result as `{currentTier:{id:"standard-tier"}}`.

### 2.6 onboardUser and LRO polling
```json
{"tierId":"free-tier","metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}}
```
- Non-free tiers add `cloudaicompanionProject` and `metadata.duetProject`. The free tier must not send a project, or the server returns `Precondition Failed`.
- Response: `{"name":"operations/xyz","done":false}`, and later `{"name":"operations/xyz","done":true,"response":{"cloudaicompanionProject":{"id":"managed-project-abc","name":"…"}}}`.
- Polling: while `!done && name`, sleep 5 s and `GET {base}/{name}`. gemini-cli has no limit. AntiCraft adds an overall timeout and cancellation.

### 2.7 Project and tier selection (`setup.ts`)
1. `projectId` comes from a user setting (gemini-cli reads env `GOOGLE_CLOUD_PROJECT` / `GOOGLE_CLOUD_PROJECT_ID`). Reject all-digit values, since those are project numbers.
2. Call `loadCodeAssist`.
   - An empty response is an error.
   - No `currentTier` plus an ineligible tier with `VALIDATION_REQUIRED` and a `validationUrl` means validation is required. Show the URL, let the user verify, and retry up to 3 times.
3. If `currentTier` is present:
   - `userTier = paidTier.id ?? currentTier.id ?? "standard-tier"`
   - `tierName = paidTier.name ?? currentTier.name`
   - `hasOnboardedPreviously = currentTier.hasOnboardedPreviously ?? true`
   - `project = response.cloudaicompanionProject ?? projectId`. If neither exists: `IneligibleTier` (reason messages joined with `, `) when `ineligibleTiers` is non-empty, otherwise `ProjectIdRequired`.
4. Otherwise (onboarding needed):
   - Tier = the `allowedTiers` entry with `isDefault`, or the fallback `{id:"legacy-tier", userDefinedCloudaicompanionProject:true}`.
   - Call `onboardUser` and poll.
   - `project = lro.response.cloudaicompanionProject.id ?? projectId`, otherwise the same errors as step 3.
   - `hasOnboardedPreviously` defaults to false.
5. gemini-cli caches the result for 30 s per (client, projectId).

Result: `{projectId, userTier, userTierName?, paidTier?, hasOnboardedPreviously?}`. `projectId` is sent as `project` in every generate call.

### 2.8 generateContent / streamGenerateContent envelope (`converter.ts`)
Casing is mixed. `user_prompt_id`, `enabled_credit_types` and the inner `session_id` are snake_case. Everything else is camelCase.
```json
{
  "model":"gemini-2.5-pro",
  "project":"managed-project-abc",
  "user_prompt_id":"<per user prompt id>",
  "request":{
    "contents":[{"role":"user","parts":[{"text":"hello"}]}],
    "systemInstruction":{"role":"user","parts":[{"text":"You are…"}]},
    "tools":[{"functionDeclarations":[{"name":"read_file","description":"…","parametersJsonSchema":{"type":"object"}}]}],
    "generationConfig":{"temperature":1,"topP":0.95,"topK":64,"thinkingConfig":{"includeThoughts":true,"thinkingBudget":8192}},
    "session_id":"<session uuid>"
  },
  "enabled_credit_types":["GOOGLE_ONE_AI"]
}
```
- `model` is the bare ID, with no `models/` prefix.
- `toolConfig`, `safetySettings`, `labels` and `cachedContent` are copied from the caller's config (`converter.ts` L172-174). When unset they are undefined, so they are missing from the JSON. AntiCraft leaves them out unless it sets a value.
- `generationConfig` keys passed through: `temperature, topP, topK, candidateCount, maxOutputTokens, stopSequences, responseLogprobs, logprobs, presencePenalty, frequencyPenalty, seed, responseMimeType, responseSchema, responseJsonSchema, routingConfig, modelSelectionConfig, responseModalities, mediaResolution, speechConfig, audioTimestamp, thinkingConfig`. Thinking defaults are in 3.6.
- `session_id`: one UUID per conversation.
- `enabled_credit_types`: sent only on streaming, and only when auto-using Google One AI credits for overage-eligible models (`billing/billing.ts`: `gemini-3-pro-preview`, `gemini-3.1-pro-preview`, preview flash). Omit it by default.
- `contents` normalization:
  - A bare string becomes a user text part.
  - Null parts are dropped.
  - A part with a `thought` field is kept as-is if it has `functionCall`, `functionResponse`, `inlineData` or `fileData`. Otherwise the thought is folded into the text as `"{text}\n[Thought: …]"`.
  - Thought parts are normally already filtered out of stored history (see 3.5).

Response (the same shape for unary calls and for each SSE chunk):
```json
{
  "traceId":"abc123",
  "response":{
    "candidates":[{"content":{"role":"model","parts":[{"text":"Hi"},{"functionCall":{"name":"read_file","args":{"file_path":"a.rs"}}}]},"finishReason":"STOP","index":0}],
    "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15,"thoughtsTokenCount":0,"cachedContentTokenCount":0},
    "modelVersion":"gemini-2.5-pro","promptFeedback":{},"automaticFunctionCallingHistory":[]
  },
  "consumedCredits":[{"creditType":"GOOGLE_ONE_AI","creditAmount":"3"}],
  "remainingCredits":[{"creditType":"GOOGLE_ONE_AI","creditAmount":"997"}]
}
```
Unwrapping:
- `responseId = traceId`.
- Missing `response` → `candidates = []`.
- Otherwise copy `candidates`, `automaticFunctionCallingHistory`, `promptFeedback`, `usageMetadata` and `modelVersion`.
- Usage total = `max(prompt + candidates + toolUsePrompt + thoughts, totalTokenCount)`.

### 2.9 countTokens
```json
{"request":{"model":"models/gemini-2.5-pro","contents":[{"role":"user","parts":[{"text":"hi"}]}]}}
```
Response: `{"totalTokens":3}`. A missing value counts as 0. Unlike generate, `model` has the `models/` prefix here.

### 2.10 retrieveUserQuota
Request: `{"project":"managed-project-abc"}`, with an optional `userAgent`. Response: `{"buckets":[{"remainingAmount":"100","remainingFraction":0.8,"resetTime":"2026-10-26T00:00:00Z","tokenType":"REQUESTS","modelId":"gemini-2.5-pro"}]}`. Every field is optional. The source does not list the `tokenType` values.

### 2.11 Other methods (not used in v1)
- `listExperiments`: `{project, metadata}` → `{experimentIds, flags[{flagId, boolValue, floatValue, intValue, stringValue, int32ListValue, stringListValue}], filteredFlags, debugString}`. Needs a project.
- `fetchAdminControls`: `{project}` → `secureModeEnabled, strictModeDisabled, mcpSetting, cliFeatureSetting, adminControlsApplicable`.
- `get/setCodeAssistGlobalUserSetting`: `{cloudaicompanionProject?, freeTierDataCollectionOptin?}`.
- `recordCodeAssistMetrics`: telemetry. AntiCraft does not send it.

### 2.12 SSE streaming
`POST {base}:streamGenerateContent?alt=sse`, with the same headers and body as generateContent.
- Split lines on `\n` or `\r\n`.
- A `data: ` line: push `line[6..].trim()` into a buffer.
- A blank line: if the buffer is non-empty, join it with `\n`, parse it as the response above, emit it, and clear the buffer.
- Other lines (`:` comments, `id:`, `event:`) are ignored.
- Malformed JSON: drop the chunk and log "invalid chunk" without its content. Keep reading.
- gemini-cli does not flush a leftover buffer at EOF. AntiCraft does.
- Each chunk is a partial candidate. Text deltas and function calls build up across chunks. `usageMetadata` usually comes last. Track credits per chunk.

### 2.13 Errors and retry
Transport level (`server.ts`):
- Unary POST: up to 3 retries on `429`, `499` and `5xx`. The first delay is 100 ms (1000 ms for `generateContent`), then exponential.
- GET: client defaults.
- Streaming: no retry at this level. The layer above handles it.

Application level (`utils/retry.ts retryWithBackoff`):
- `maxAttempts = 10`, `initialDelayMs = 5000`, `maxDelayMs = 30000`, exponential with jitter.
- Retry on network errors (reset, timeout, broken pipe, DNS, refused, TLS protocol) and on HTTP `429 | 499 | 5xx`.
- Never retry 400.

Error body (Google RPC format):
```json
{"error":{"code":429,"message":"…","status":"RESOURCE_EXHAUSTED","details":[
  {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED","domain":"cloudcode-pa.googleapis.com","metadata":{}},
  {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"12s"},
  {"@type":"type.googleapis.com/google.rpc.QuotaFailure","violations":[{"quotaId":"…PerDay…"}]},
  {"@type":"type.googleapis.com/google.rpc.Help","links":[{"description":"…","url":"…"}]}]}}
```
Classification (`utils/googleQuotaErrors.ts`):
- 404 → model not found (terminal).
- 403 with `VALIDATION_REQUIRED` from a cloudcode domain → validation flow, using the URL from the `Help` link.
- For 429/499/503:
  - Message contains `limit: 0` → terminal.
  - A `QuotaFailure` `quotaId` containing `PerDay` or `Daily` → terminal (daily quota used up).
  - `ErrorInfo.reason` is `MODEL_CAPACITY_EXHAUSTED`, `MODEL_CAPACITY_EXCEEDED` or `INSUFFICIENT_G1_CREDITS_BALANCE` → terminal. gemini-cli then falls back to another model.
  - Cloudcode `RATE_LIMIT_EXCEEDED` → retry after `RetryInfo.retryDelay`. Terminal if there is no delay or it is over 300 s.
  - Cloudcode `QUOTA_EXHAUSTED` → terminal.
  - `RetryInfo` on its own → retryable if the delay is ≤ 300 s.
  - No details: parse `Please retry in ([0-9.]+(?:ms|s))` from the message. Otherwise a message matching `exhausted your capacity|capacity exceeded|MODEL_CAPACITY_EXHAUSTED` is terminal. Anything else is a retryable rate limit.
- `retryDelay` is a proto Duration string (`"12s"`, `"0.5s"`, or `ms`).

Rust model:
```rust
enum CaError {
    Retryable { delay: Option<Duration> },
    TerminalQuota { reason: String },
    ValidationRequired { url: String, description: String },
    IneligibleTier(Vec<IneligibleTier>),
    ProjectIdRequired,
    NotFound,
    Http { status: u16, body_redacted: String },
    Transport(String),
}
```
`Display` must never include headers or tokens.

### 2.14 Minimal call sequence
1. OAuth → access token.
2. `:loadCodeAssist` → either `currentTier` + project, or `:onboardUser` → poll `GET /{name}` every 5 s → project.
3. Chat: `:streamGenerateContent?alt=sse` with the envelope. Parse the SSE and unwrap `.response`.
4. Optional: `:countTokens`, `:retrieveUserQuota`.
5. Tests against the mock server should check:
   - path `/v1internal:streamGenerateContent`
   - query `alt=sse`
   - that `Authorization` is present (never its value)
   - envelope casing

---

## 3. Agent loop and tools

Sources: `core/{turn.ts,geminiChat.ts,client.ts,prompts.ts,tokenLimits.ts}`, `prompts/{snippets.ts,promptProvider.ts}`, `tools/*`, `scheduler/*`, `policy/*`, `services/loopDetectionService.ts`, `context/chatCompressionService.ts`, `config/{models.ts,defaultModelConfigs.ts,config.ts}`, `utils/{workspaceContext.ts,thoughtUtils.ts,nextSpeakerChecker.ts,constants.ts}`. Reference version: 0.67.0-nightly.20261020.

### 3.1 Turn transport
One turn is one `streamGenerateContent` call (2.8, 2.12). It carries the full history plus the new user content, the system instruction and the tool declarations.

### 3.2 System prompt structure
gemini-cli builds the prompt from the sections below, drops empty ones, and trims the result. AntiCraft writes its own text in the same layout and does not copy Google's prose.
1. Preamble: an interactive agent for software engineering tasks.
2. `# Core Mandates`:
   - Security: no secrets, stay inside the workspace.
   - Context efficiency.
   - Engineering standards: follow project conventions, check that a library exists before using it, add tests, keep comments minimal.
3. Optional: sub-agents, skills, hook context.
4. `# Primary Workflows`: understand → plan → implement → test → lint/build, plus scaffolding for new apps. Plan mode replaces this section with an approval-mode section.
5. Optional: task-management section.
6. `# Operational Guidelines`:
   - Keep the tone concise.
   - Explain a mutating shell command before running it.
   - Use absolute paths, run independent calls in parallel, put long-running processes in the background, avoid interactive commands.
7. Optional:
   - Autonomous (YOLO) mode.
   - Sandbox notice.
   - `# Git Repository`: check status, diff and log before committing; never push unless asked.
8. Memory wrapper: `# Contextual Instructions (<files>)`, holding the hierarchical context files.

AntiCraft additions:
- An environment block: OS, cwd, date, and a shallow folder tree.
- An `ANTICRAFT.md` context file (optionally also `GEMINI.md`), appended as the memory section.

### 3.3 Tool declarations
Sent as `tools[0].functionDeclarations[]`, each `{name, description, parametersJsonSchema}`. This version uses `parametersJsonSchema` (plain JSON Schema), not `parameters`. Sources: `tools/definitions/base-declarations.ts`, `model-family-sets/default-legacy.ts` and `gemini-3.ts`, `dynamic-declaration-helpers.ts`.

| name | Kind | required | properties |
|---|---|---|---|
| `read_file` | read | `file_path` | `file_path` string; `start_line`, `end_line` integer (1-based, inclusive) |
| `write_file` | edit | `file_path`, `content` | full file content |
| `replace` | edit | `file_path`, `instruction`, `old_string`, `new_string` | `allow_multiple` bool |
| `list_directory` | search | `dir_path` | `ignore` string[]; `file_filtering_options` {`respect_git_ignore`, `respect_gemini_ignore`} |
| `glob` | search | `pattern` | `dir_path`; `case_sensitive`, `respect_git_ignore`, `respect_gemini_ignore` bool |
| `grep_search` | search | `pattern` | `dir_path`, `include_pattern`, `exclude_pattern`; `names_only`; `max_matches_per_file`, `total_max_matches`. The ripgrep variant adds `case_sensitive`, `fixed_strings`, `no_ignore`, `context`, `after`, `before` |
| `run_shell_command` | execute | `command` | `description` (one line for the user); `dir_path` (must exist inside the workspace); `is_background` bool; `delay_ms` integer |
| `web_fetch` | fetch | `prompt` | prompt containing the URL(s) and instructions |
| `google_web_search` | search | `query` | |
| `read_many_files` | read | `include` | `include`, `exclude` string[]; `recursive`; `useDefaultExcludes`; `file_filtering_options` |
| `write_todos` | other | `todos` | array of {`description`, `status`: `pending`/`in_progress`/`completed`/`cancelled`/`blocked`}, `additionalProperties:false` |
| `ask_user` | communicate | `questions` | items: `question`, `header`, `type`; optional `options[{label,description}]`, `multiSelect`, `placeholder` |

Other names in the registry, not needed in v1: `get_internal_docs`, `activate_skill`, `enter_plan_mode`, `exit_plan_mode`, `update_topic`, `complete_task`, `read_mcp_resource`, `list_mcp_resources`. gemini-cli no longer has a `save_memory` tool; it keeps memory by editing md files. AntiCraft may define its own `save_memory {fact}`.

Example:
```json
{"name":"read_file","description":"Reads a file. Use start_line/end_line for large files.",
 "parametersJsonSchema":{"type":"object","properties":{
   "file_path":{"type":"string","description":"Path to the file."},
   "start_line":{"type":"integer","description":"1-based start line."},
   "end_line":{"type":"integer","description":"1-based inclusive end line."}},
 "required":["file_path"]}}
```

Behaviors to reproduce:
- `read_file`:
  - Shows 2000 lines by default and cuts each line at 2000 chars.
  - When output is cut, add a header that says which line range was shown.
  - Images and PDFs come back as `inlineData`.
- `replace`:
  - With `allow_multiple` false, exactly one match is required.
  - Error types: `EDIT_NO_OCCURRENCE_FOUND`; `EDIT_EXPECTED_OCCURRENCE_MISMATCH` ("Expected 1 occurrence but found N … set 'allow_multiple' to true"); `EDIT_NO_CHANGE`; `FILE_NOT_FOUND`; `ATTEMPT_TO_CREATE_EXISTING_FILE`; `FILE_WRITE_FAILURE`.
  - An empty `old_string` on a missing file creates the file.
  - The LLM "fix edit" fallback is skipped in v1.
- `write_file`: create or overwrite, creating parent dirs.
- `run_shell_command`:
  - On Windows, run `powershell.exe -NoProfile -Command`.
  - Return a block with Command, Directory, Output (stdout+stderr), Error, Exit Code, Signal, Background PIDs, and Process Group PGID.

### 3.4 functionCall → functionResponse
1. Streamed parts are one of:
   - `{text}`
   - `{text, thought:true}`
   - `{functionCall:{id?,name,args}}`, which may carry `thoughtSignature`
   - `{inlineData}`
2. Call IDs:
   - Use `fnCall.id` if present. Otherwise generate `"{name}_{epochMs}_{counter}"`.
   - Prefix it as `"{name}__{id}"` if it is not already prefixed, and write it back into the history part.
   - An empty name becomes `generic_tool`.
3. When the stream ends, schedule all calls:
   - Check args against the schema; on failure, return an error response.
   - Run the policy check and confirmation (3.7).
   - Execute. Read-only kinds may run in parallel. Mutators run in order.
4. Results:
```json
{"functionResponse":{"id":"read_file__abc","name":"read_file","response":{"output":"<text>"}}}
{"functionResponse":{"id":"…","name":"…","response":{"error":"message"}}}
```
   - Cancel and deny use `"[Operation Cancelled] Reason: <reason>"`.
   - Binary results go in `functionResponse.parts:[{inlineData}]` when the model supports multimodal function responses. Otherwise they go in sibling parts.
   - Audio and video are replaced with a text note.
5. Put every response into one `{"role":"user","parts":[…]}` content:
   - Keep call order, with one response per call. Make up a cancelled response for any that are missing.
   - Send it as the next request.
   - Repeat until a model turn has no function calls.

### 3.5 History
- History is a `Content[]` that alternates `user` and `model`. Tool responses use `user`.
- At the end of a stream:
  - Merge the model's parts into one model content, joining adjacent text parts.
  - Drop thought parts from stored history.
- An empty or invalid model turn (no candidates, or empty parts) gives `invalid_stream`, then a retry.
- Thought signature:
  - Before each request, the first `functionCall` part of every model content must carry `thoughtSignature`.
  - Keep the real one.
  - If it is missing (for example in edited or restored history), set `"skip_thought_signature_validator"`. Otherwise the API returns 400.

```json
[
 {"role":"user","parts":[{"text":"fix the bug in main.rs"}]},
 {"role":"model","parts":[{"text":"Reading it."},{"functionCall":{"id":"read_file__1","name":"read_file","args":{"file_path":"C:/ws/src/main.rs"}},"thoughtSignature":"<opaque>"}]},
 {"role":"user","parts":[{"functionResponse":{"id":"read_file__1","name":"read_file","response":{"output":"fn main(){...}"}}}]},
 {"role":"model","parts":[{"text":"Fixed."}]}
]
```

### 3.6 Thinking
Checked in `config/defaultModelConfigs.ts` and `config/models.ts`:
- Chat base: `temperature 1`, `topP 0.95`, `topK 64`, `thinkingConfig.includeThoughts: true`.
- Gemini 2.5 chat: `thinkingBudget: 8192` (`DEFAULT_THINKING_MODE`).
- Gemini 3.x chat: `thinkingLevel: "HIGH"` instead of a budget.
- Utility calls (classifier, loop check, next speaker): `thinkingBudget: 0` or `512`, `temperature 0`.
- Thought parts in responses (`thought: true`):
  - Parse them as `**Subject**` plus a description.
  - Show them as a collapsible "Thinking" block.
  - Never store them in history.
- `thoughtSignature` is opaque. Keep it and never show it.

### 3.7 Confirmation, policy, approval modes
- Kinds: `read, edit, delete, move, search, execute, think, agent, fetch, communicate, plan, switch_mode, other`. Mutators: `edit, delete, move, execute`.
- `ApprovalMode`: `default`, `autoEdit`, `yolo`, `plan`. `PolicyDecision`: `allow`, `deny`, `ask_user`.
- Default rules:
  - Read and search tools are always allowed.
  - `default` asks before `replace`, `write_file`, `run_shell_command` and `web_fetch`.
  - `autoEdit` allows edits, writes (still path-checked) and fetch. Shell still asks.
  - `yolo` allows everything.
  - `plan` denies mutators.
- Confirmation UI:
  - Edits and writes: file name and unified diff.
  - Shell: the command and its root command(s).
  - Fetch: the URLs.
- AntiCraft outcomes:
  - `proceed_once`.
  - `proceed_always`: per tool, or per root command (such as `cargo`) for shell. Lasts for the session.
  - `cancel`: sends a cancelled functionResponse and stops the loop.

### 3.8 Workspace path restriction
- Resolve relative paths against the workspace root, then canonicalize, following symlinks. On Windows, strip `\\?\` (e.g. with `dunce`).
- A path is allowed only if it is inside a workspace root (or an added dir) by component-wise `starts_with`, or inside AntiCraft's temp dir. Extra read-only dirs may be allowed for reads.
- A path outside these returns `response.error` saying it is outside the workspace. Shell `dir_path` goes through the same check.

### 3.9 Turn limits and loop detection
- `MAX_TURNS = 100` model requests per user prompt (`core/client.ts`). There is no session turn limit by default. If one is set and exceeded, emit `max_session_turns`.
- Loop detection:
  - The same tool name and args hash 5 times in a row → `CONSECUTIVE_IDENTICAL_TOOL_CALLS`.
  - A 50-char chunk repeated 10 times at small, regular distances → `CONTENT_CHANTING_LOOP`. The window is 5000 chars. Code fences, tables and lists reset tracking.
  - Optional LLM check → `LLM_DETECTED_LOOP`. It starts after 30 turns, runs about every 10 turns on the last 20 items, and triggers at confidence ≥ 0.9.
  - On detection: abort the stream, emit `loop_detected`, and send no more tool responses.
- Next-speaker check: optional, off by default in AntiCraft. When a turn ends with no tool calls and a small model says `model` should speak next, send `"Please continue."`.
- Event enum that drives the egui UI (`GeminiEventType`): `content`, `thought`, `tool_call_request`, `tool_call_response`, `tool_call_confirmation`, `user_cancelled`, `error`, `retry`, `chat_compressed`, `max_session_turns`, `finished`, `loop_detected`, `citation`, `context_window_will_overflow`, `invalid_stream`, `model_info`, `agent_execution_stopped`, `agent_execution_blocked`.

### 3.10 Chat compression
- Trigger: before a turn, when history tokens are ≥ 0.5 × the model token limit (1,048,576 for current Gemini models). Count with `countTokens`, or estimate as chars/4.
- Old function responses:
  - Collapse each to about 2 KB.
  - The kept tail has a 50,000-token budget for function responses.
  - The last 3 turns are protected.
- Keep the newest 30% of history by JSON char count. The cut must fall on a `user` content that has no `functionResponse`.
- Summarize the older part with a separate request:
  - Its system prompt asks for a private scratchpad, then an XML `<state_snapshot>` with `<overall_goal>`, `<active_constraints>`, `<key_knowledge>`, `<artifact_trail>`, `<file_system_state>`, `<recent_actions>` and `<task_state>`.
  - It merges any previous snapshot.
  - gemini-cli runs a second self-critique pass.
  - AntiCraft writes its own prompt text.
- New history: `[user: <state_snapshot>…</state_snapshot>, model: short acknowledgement, …kept tail]`. If it is not smaller than before, discard it. Emit `chat_compressed {originalTokenCount, newTokenCount}`.

### 3.11 Models
- Model IDs:
  - Pro: `gemini-2.5-pro` (`DEFAULT_GEMINI_MODEL`), `gemini-3-pro-preview`, `gemini-3.1-pro-preview`, `gemini-3.1-pro-preview-customtools`.
  - Flash: `gemini-3-flash-preview` (preview), `gemini-3.5-flash` (GA base), `gemini-3.8-flash` (latest, experiment-gated), `gemini-3-flash` (legacy).
  - Flash-lite: `gemini-3.1-flash-lite`, `gemini-3.5-flash-lite`.
- Aliases: `auto`, `pro`, `flash`, `flash-lite`. `auto` and `pro` resolve to the preview pro model when the account has preview access, otherwise to `gemini-2.5-pro`.
- Backend remapping: `gemini-3.5-flash` is sent as `gemini-3-flash` unless latest-flash GA is enabled. When it is, both are sent as `gemini-3.8-flash`.
- On terminal quota or capacity errors, gemini-cli falls back from pro to flash for the rest of the session. AntiCraft should ask the user first instead of switching silently.
- Model dropdown: `auto`, `gemini-2.5-pro`, `gemini-3-pro-preview`, `gemini-3.1-pro-preview`, `gemini-3-flash-preview`, `gemini-3.5-flash`, `gemini-3.1-flash-lite`. Default: `gemini-2.5-pro`. Which models work depends on the tier, so show API errors to the user.

---

## 4. Open questions

Auth
1. The OAuth endpoint URLs (auth, token, tokeninfo, revoke) and the ~5-minute eager-refresh threshold are google-auth-library 10.9.0 defaults, not read from repo source. Check them against the library if exact parity matters.
2. PKCE on the loopback flow is recommended and planned. `prompt=consent`, which forces a refresh token on every login, is undecided. Recommendation: send it only when no refresh token is stored.
3. Credential store: Windows Credential Manager, or an ACL-protected file under `%APPDATA%\AntiCraft`. Either way it stays separate from `~/.gemini`.

Code Assist API
4. The source does not list the `retrieveUserQuota` `tokenType` values or the full set of tier IDs. Treat both as open strings.
5. The source cannot show whether cloudcode-pa enforces a User-Agent or client prefix. AntiCraft sends its own honest UA. This can only be confirmed against the real service, which tests never touch.

Agent loop
6. Per-model tool schemas: `model-family-sets/gemini-3.ts` was not compared field by field with `default-legacy.ts`. Check it before tuning declarations for Gemini 3.
7. `toolConfig` / `safetySettings`: resolved. gemini-cli passes them through only when set and leaves them out otherwise. Unverified: whether an explicit `functionCallingConfig.mode: "AUTO"` changes behavior. Default: omit it.
8. Which preview models a free-tier account can use depends on server-side tier and experiment flags. That can only be seen at runtime with a real account.

## Resolved contradictions
- Thinking config: one draft used `thinkingBudget: -1`. `config/defaultModelConfigs.ts` uses `8192` (`DEFAULT_THINKING_MODE` in `config/models.ts`) for 2.5 and `thinkingLevel: HIGH` for 3.x. This spec follows the source.
- `toolConfig` / `safetySettings`: one draft showed them as always present. `converter.ts` copies `req.config?.toolConfig` and `req.config?.safetySettings`, so they are missing when unset rather than sent as empty values.
- `read_file` argument: the parameter is `file_path`, not `path`. All examples now use `file_path`.

## Attribution
Protocol facts are derived from google-gemini/gemini-cli, Copyright 2025 Google LLC, licensed under Apache-2.0. Record this in AntiCraft's `NOTICE`.
