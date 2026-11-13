//! One file per signed-in account: `{data_dir}/accounts/<id>.json`.
//!
//! A file per account, rather than one list, because a refresh rewrites one account at a time and
//! a torn write can then only lose that account's file. Every write goes through
//! [`atomic_write_private`], so the directory and the file are owner-only from the first byte:
//! these files hold refresh tokens.
//!
//! The id is a file name and it comes from a hand-editable settings file, so [`valid_id`] gates
//! both directions — before a path is built on the way in, and again on the way out when a record
//! is read back.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::AccountProvider;
use crate::auth::store::{atomic_write_private, Credentials};

/// One account on disk.
///
/// `#[serde(flatten)]` reuses [`Credentials`]: its token fields keep their `skip_serializing_if`,
/// and they deserialize from the same keys, so a record is one flat JSON object rather than a
/// nested `creds` one. The redaction is this struct's own `Debug` below, not an inherited one.
#[derive(Clone, Serialize, Deserialize)]
pub struct AccountRecord {
    #[serde(flatten)]
    pub creds: Credentials,
    pub provider: AccountProvider,
    /// The account id; must also satisfy [`valid_id`].
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Codex `chatgpt_account_id` (sent as `Chatgpt-Account-Id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
    /// Antigravity: the discovered Cloud project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    /// Epoch millis of the last successful model fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_fetched_at: Option<i64>,
    /// `expires_in` of the token that is stored, so the refresh lead can be clamped to half of
    /// the lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime_secs: Option<i64>,
}

// Hand-written for the same reason `Credentials`' is: a derived one would print the tokens, and
// one `tracing::debug!(?record)` or panic message would put them in the log.
impl std::fmt::Debug for AccountRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountRecord")
            .field("account_id", &self.account_id)
            .field("provider", &self.provider)
            .field("label", &self.label)
            .field("plan", &self.plan)
            .field("has_access_token", &self.creds.access_token.is_some())
            .field("has_refresh_token", &self.creds.refresh_token.is_some())
            .field("expiry_date", &self.creds.expiry_date)
            .field("email", &self.creds.email)
            .field("has_wire_account_id", &self.wire_account_id.is_some())
            .field("project_id", &self.project_id)
            .field("models", &self.models.len())
            .field("models_fetched_at", &self.models_fetched_at)
            .field("lifetime_secs", &self.lifetime_secs)
            .finish()
    }
}

/// `^(antigravity|codex|claude)-[0-9a-f]{8}$`.
///
/// Lives here rather than in `mod.rs` because this is where an id is joined into a path; the
/// parent module re-exports it so settings and UI can say `external::valid_id` and mean this one.
pub fn valid_id(id: &str) -> bool {
    let Some((prefix, rest)) = id.split_once('-') else {
        return false;
    };
    AccountProvider::parse(prefix).is_some_and(|p| p.as_str() == prefix)
        && rest.len() == 8
        && rest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One file per account: `<data_dir>/accounts/<id>.json`.
#[derive(Debug, Clone)]
pub struct AccountStore {
    dir: PathBuf,
}

impl AccountStore {
    /// `{data_dir}/accounts`. The directory itself is created by `create_private_dir` inside the
    /// first write, never by `create_dir_all` here — the owner-only DACL is the point, and it has
    /// to be on the leaf, not inherited from a folder whose permissions we did not choose.
    pub fn in_dir(data_dir: &Path) -> Self {
        Self { dir: data_dir.join("accounts") }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// `<dir>/<id>.json`.
    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Every parsable record; an unreadable file, a bad file name or an id that does not match
    /// the file name is logged and skipped. A missing directory is simply "no accounts".
    ///
    /// One bad file must never cost the user the others: a half-written or hand-edited record is
    /// dropped on its own, exactly as `parse_providers` does for settings.
    pub fn load_all(&self) -> Vec<AccountRecord> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                tracing::warn!("could not read the accounts folder: {e}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            if path.extension().and_then(|s| s.to_str()) != Some("json") || !valid_id(stem) {
                continue;
            }
            match read_record(&path) {
                Ok(rec) if rec.account_id == stem => out.push(rec),
                Ok(rec) => tracing::warn!(
                    "account file {} names account {:?}, not {:?}; skipping it",
                    path.display(),
                    rec.account_id,
                    stem
                ),
                Err(e) => tracing::warn!("account file {} is not usable ({e}); skipping it", path.display()),
            }
        }
        out
    }

    /// Whether the account has no file at all, as opposed to one that exists but cannot be read
    /// (damaged, or held open by another program) — which is no reason to forget the account. An
    /// id that could never name a file has none.
    pub fn is_missing(&self, id: &str) -> bool {
        !valid_id(id) || matches!(std::fs::metadata(self.path(id)), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    }

    /// `Err(InvalidInput)` when `!valid_id(id)` — defence in depth, before the path is built.
    pub fn load(&self, id: &str) -> std::io::Result<Option<AccountRecord>> {
        if !valid_id(id) {
            return Err(bad_id(id));
        }
        let path = self.path(id);
        match read_record(&path) {
            Ok(rec) => Ok(Some(rec)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Atomic and owner-only. `InvalidInput` on an id that is not [`valid_id`], or on a record
    /// whose `account_id` differs from the id it is filed under.
    pub fn save(&self, rec: &AccountRecord) -> std::io::Result<()> {
        if !valid_id(&rec.account_id) {
            return Err(bad_id(&rec.account_id));
        }
        let json = serde_json::to_vec_pretty(rec).map_err(std::io::Error::other)?;
        atomic_write_private(&self.path(&rec.account_id), &json)
    }

    /// Missing file is not an error; `InvalidInput` on a bad id.
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        if !valid_id(id) {
            return Err(bad_id(id));
        }
        match std::fs::remove_file(self.path(id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Read and parse one record. A parse failure is reported as the file's own error, without the
/// content: serde's message can quote a value out of a hand-damaged file (`invalid type: string
/// "…"`, `unknown variant "…"`), and in this file that value can be a token. Only the position and
/// the kind of failure survive, which is all a person fixing the file needs.
fn read_record(path: &Path) -> std::io::Result<AccountRecord> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| {
        let what = match e.classify() {
            serde_json::error::Category::Data => "is not a valid account record",
            _ => "is not valid JSON",
        };
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("account file {what} at line {} column {}", e.line(), e.column()),
        )
    })
}

fn bad_id(id: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{id:?} is not an account id"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> AccountRecord {
        AccountRecord {
            creds: Credentials {
                access_token: Some("ACCESS-TOKEN-VALUE".into()),
                refresh_token: Some("REFRESH-TOKEN-VALUE".into()),
                expiry_date: Some(1_700_000_000_000),
                email: Some("me@example.com".into()),
                ..Default::default()
            },
            provider: AccountProvider::ClaudeCode,
            account_id: id.into(),
            label: Some("Me".into()),
            plan: Some("max".into()),
            wire_account_id: Some("acct-123".into()),
            org_uuid: None,
            project_id: None,
            models: vec!["claude-sonnet-4-6".into()],
            models_fetched_at: Some(1_700_000_000_000),
            lifetime_secs: Some(3600),
        }
    }

    #[test]
    fn valid_id_rejects_paths() {
        for bad in ["../../oauth_creds", "claude-ZZZZZZZZ", "claude-1234567", "", "claude-012345678", "codex0123abcd", "Claude-0123abcd"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
        assert!(valid_id("claude-0123abcd"));
        assert!(valid_id("antigravity-deadbeef"));
        assert!(valid_id("codex-00000000"));
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::in_dir(dir.path());
        let err = store.load("../x").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(store.save(&record("../x")).unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(store.delete("../x").unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn store_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::in_dir(dir.path());
        assert!(store.load_all().is_empty(), "a missing folder is no accounts, not an error");
        assert!(store.load("claude-0123abcd").unwrap().is_none(), "a missing file is not an error");

        let rec = record("claude-0123abcd");
        store.save(&rec).unwrap();
        let path = dir.path().join("accounts").join("claude-0123abcd.json");
        assert!(path.exists());
        let loaded = store.load("claude-0123abcd").unwrap().unwrap();
        assert_eq!(loaded.account_id, rec.account_id);
        assert_eq!(loaded.provider, rec.provider);
        assert_eq!(loaded.creds.refresh_token.as_deref(), Some("REFRESH-TOKEN-VALUE"));
        assert_eq!(loaded.creds.access_token.as_deref(), Some("ACCESS-TOKEN-VALUE"));
        assert_eq!(loaded.models, rec.models);
        assert_eq!(loaded.lifetime_secs, Some(3600));
        assert_eq!(loaded.plan.as_deref(), Some("max"));
        // One flat object: `Credentials` is flattened, so the keys sit beside `provider`.
        let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["provider"], "claude");
        assert_eq!(raw["refresh_token"], "REFRESH-TOKEN-VALUE");
        assert!(raw.get("creds").is_none());

        assert_eq!(store.load_all().len(), 1);
        store.delete("claude-0123abcd").unwrap();
        assert!(store.load("claude-0123abcd").unwrap().is_none());
        store.delete("claude-0123abcd").unwrap();
    }

    #[test]
    fn account_record_debug_has_no_token() {
        let text = format!("{:?}", record("claude-0123abcd"));
        for leaked in ["ACCESS-TOKEN-VALUE", "REFRESH-TOKEN-VALUE"] {
            assert!(!text.contains(leaked), "{leaked} leaked in {text}");
        }
        assert!(text.contains("has_refresh_token: true"), "{text}");
        assert!(text.contains("me@example.com"), "{text}");
    }

    #[test]
    fn corrupt_files_are_skipped_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::in_dir(dir.path());
        store.save(&record("claude-0123abcd")).unwrap();
        store.save(&record("codex-deadbeef")).unwrap();
        let folder = dir.path().join("accounts");
        std::fs::write(folder.join("claude-11111111.json"), b"{ not json").unwrap();
        // A valid file whose name is not an id, and a stray file that is not JSON at all.
        std::fs::write(folder.join("notes.txt"), b"hello").unwrap();
        std::fs::write(folder.join("evil-zzzzzzzz.json"), b"{}").unwrap();
        // A record filed under someone else's name is refused rather than trusted.
        let mut moved = record("codex-deadbeef");
        moved.account_id = "claude-22222222".into();
        std::fs::write(folder.join("codex-22222222.json"), serde_json::to_vec(&moved).unwrap()).unwrap();

        let ids: Vec<String> = {
            let mut v: Vec<String> = store.load_all().into_iter().map(|r| r.account_id).collect();
            v.sort();
            v
        };
        assert_eq!(ids, ["claude-0123abcd", "codex-deadbeef"]);
        // The corrupt one is an error when asked for by name, never a silent `None`.
        assert_eq!(store.load("claude-11111111").unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_damaged_file_s_error_quotes_no_value_from_it() {
        // serde names the offending value for a wrong type or an unknown variant; the error built
        // from it is logged, so it must carry the position only.
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::in_dir(dir.path());
        store.save(&record("claude-0123abcd")).unwrap();
        let path = dir.path().join("accounts").join("claude-0123abcd.json");
        let secret = "SECRET-TOKEN-IN-THE-WRONG-FIELD";
        for (damaged, what) in [
            // A token pasted where a number belongs.
            (format!(r#"{{"provider":"claude","account_id":"claude-0123abcd","expiry_date":"{secret}"}}"#), "not a valid account record"),
            // A token pasted over the provider.
            (format!(r#"{{"provider":"{secret}","account_id":"claude-0123abcd"}}"#), "not a valid account record"),
            // Cut off half-way through a token.
            (format!(r#"{{"provider":"claude","access_token":"{secret}"#), "not valid JSON"),
        ] {
            std::fs::write(&path, damaged.as_bytes()).unwrap();
            let err = store.load("claude-0123abcd").unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            let text = err.to_string();
            assert!(!text.contains(secret), "{text}");
            assert!(!text.contains("SECRET"), "{text}");
            assert!(text.contains(what) && text.contains("line 1 column"), "{text}");
        }
        // `load_all` skips it (and logs that same value-free message).
        assert!(store.load_all().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn account_file_acl_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::in_dir(dir.path());
        store.save(&record("claude-0123abcd")).unwrap();
        let path = dir.path().join("accounts").join("claude-0123abcd.json");
        let out = std::process::Command::new("icacls").arg(&path).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        // Exactly one ACE, not inherited (no "(i)").
        let aces: Vec<_> = text.lines().filter(|l| l.contains(":(")).collect();
        assert_eq!(aces.len(), 1, "{text}");
        assert!(!aces[0].contains("(i)"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn account_file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::in_dir(dir.path());
        store.save(&record("claude-0123abcd")).unwrap();
        let path = dir.path().join("accounts").join("claude-0123abcd.json");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(dir.path().join("accounts")).unwrap().permissions().mode() & 0o777, 0o700);
    }
}
