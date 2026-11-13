//! User-entered OAuth client secret at `<data_dir>\oauth_client.json` (DESIGN.md 6.5).
//! Written with [`super::store::atomic_write_private`] (owner-only DACL), never logged.
//! A corrupt file is treated as absent.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store::atomic_write_private;

pub const CLIENT_FILE: &str = "oauth_client.json";

/// Where the client secret was resolved from (shown in the UI without the value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientSource {
    File,
    Env,
    BuiltIn,
}

/// Stored client identity. `Debug` is redacted.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredClient {
    #[serde(default)]
    pub client_secret: String,
}

impl std::fmt::Debug for StoredClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredClient").field("client_secret", &"<redacted>").finish()
    }
}

pub fn path_in(data_dir: &Path) -> PathBuf {
    data_dir.join(CLIENT_FILE)
}

/// Load the stored secret; missing, empty or corrupt files return `None`.
pub fn load(data_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(path_in(data_dir)).ok()?;
    match serde_json::from_slice::<StoredClient>(&bytes) {
        Ok(c) => Some(c.client_secret.trim().to_owned()).filter(|s| !s.is_empty()),
        Err(_) => {
            tracing::warn!("{CLIENT_FILE} is not valid JSON; ignoring it");
            None
        }
    }
}

/// Save `secret` (trimmed, non-empty) owner-only.
pub fn save(data_dir: &Path, secret: &str) -> std::io::Result<()> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "client secret is empty"));
    }
    let json = serde_json::to_vec_pretty(&StoredClient { client_secret: secret.to_owned() }).map_err(std::io::Error::other)?;
    atomic_write_private(&path_in(data_dir), &json)
}

/// Delete the stored secret; a missing file is not an error.
pub fn delete(data_dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path_in(data_dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Resolve the secret: file, then runtime env, then the build-time value.
pub fn resolve(data_dir: &Path, env_value: Option<String>, built_in: Option<&str>) -> Option<(String, ClientSource)> {
    if let Some(s) = load(data_dir) {
        return Some((s, ClientSource::File));
    }
    if let Some(s) = env_value.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) {
        return Some((s, ClientSource::Env));
    }
    built_in.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).map(|s| (s, ClientSource::BuiltIn))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_wins_over_env() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve(dir.path(), Some("env".into()), Some("built")), Some(("env".into(), ClientSource::Env)));
        assert_eq!(resolve(dir.path(), None, Some("built")), Some(("built".into(), ClientSource::BuiltIn)));
        assert_eq!(resolve(dir.path(), Some(" ".into()), None), None);
        save(dir.path(), "  from-file \n").unwrap();
        assert_eq!(resolve(dir.path(), Some("env".into()), None), Some(("from-file".into(), ClientSource::File)));
        delete(dir.path()).unwrap();
        delete(dir.path()).unwrap();
        assert_eq!(load(dir.path()), None);
    }

    #[test]
    fn empty_and_corrupt_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(save(dir.path(), "   ").is_err());
        std::fs::write(path_in(dir.path()), b"{not json").unwrap();
        assert_eq!(load(dir.path()), None);
    }

    #[test]
    fn debug_is_redacted() {
        let c = StoredClient { client_secret: "SUPER-SECRET".into() };
        assert!(!format!("{c:?}").contains("SUPER-SECRET"));
    }

    #[cfg(windows)]
    #[test]
    fn client_file_acl_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), "s").unwrap();
        let out = std::process::Command::new("icacls").arg(path_in(dir.path())).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        let aces: Vec<_> = text.lines().filter(|l| l.contains(":(")).collect();
        assert_eq!(aces.len(), 1, "{text}");
        assert!(!aces[0].contains("(i)"), "{text}");
    }
}
