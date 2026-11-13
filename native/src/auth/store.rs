//! On-disk credential storage at `%APPDATA%\AntiCraft\native\oauth_creds.json` (PROTOCOL 1.6).
//! Shallow merge on write, atomic temp-file + rename. The temp file is created already
//! restricted to the current user (DACL from the process token SID on Windows, mode 0600 on
//! unix), so the token is never on disk with inherited permissions.
//! Never reads or writes `~/.gemini`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::oauth::TokenResponse;

/// Stored token set. `Debug` is redacted; serialization skips `None` fields.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// Expiry as epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_date: Option<i64>,
    /// Cached account email (AntiCraft extension).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("has_access_token", &self.access_token.is_some())
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("expiry_date", &self.expiry_date)
            .field("email", &self.email)
            .finish()
    }
}

impl Credentials {
    /// Convert a token response, computing `expiry_date = now + expires_in`.
    pub fn from_token_response(t: TokenResponse) -> Self {
        let now = chrono::Utc::now().timestamp_millis();
        Self {
            expiry_date: t.expires_in.map(|s| now + s * 1000),
            access_token: Some(t.access_token),
            refresh_token: t.refresh_token,
            scope: t.scope,
            token_type: t.token_type,
            id_token: t.id_token,
            email: None,
        }
    }

    /// True when there is no access token or it expires within `skew`.
    pub fn needs_refresh(&self, skew: std::time::Duration) -> bool {
        match (self.access_token.as_ref(), self.expiry_date) {
            (Some(_), Some(exp)) => {
                chrono::Utc::now().timestamp_millis() + skew.as_millis() as i64 >= exp
            }
            _ => true,
        }
    }

    /// Shallow merge: every `Some` field in `newer` overwrites `self` (keeps old refresh token).
    pub fn merge(&mut self, newer: &Credentials) {
        macro_rules! m {
            ($($f:ident),*) => { $( if newer.$f.is_some() { self.$f = newer.$f.clone(); } )* };
        }
        m!(access_token, refresh_token, scope, token_type, id_token, expiry_date, email);
    }
}

/// File-backed credential store.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `{data_dir}/oauth_creds.json`.
    pub fn in_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join("oauth_creds.json"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> std::io::Result<Option<Credentials>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| {
                // Never serde's own text: a type error quotes the value it met ("invalid type:
                // string \"ya29.…\""), and that value is a token. Where it went wrong is enough.
                let what = match e.classify() {
                    serde_json::error::Category::Data => "is not a valid credentials file",
                    _ => "is not valid JSON",
                };
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("oauth_creds.json {what} at line {} column {}", e.line(), e.column()),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Merge `update` over the stored file, write atomically, return the merged result.
    pub fn save_merged(&self, update: &Credentials) -> std::io::Result<Credentials> {
        let mut merged = self.load()?.unwrap_or_default();
        merged.merge(update);
        let json = serde_json::to_vec_pretty(&merged).map_err(std::io::Error::other)?;
        atomic_write_private(&self.path, &json)?;
        Ok(merged)
    }

    /// Remove the file; a missing file is not an error.
    pub fn delete(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Write `bytes` to `path` via a sibling temp file that is created owner-only, then rename.
/// Fails (instead of writing) if the restricted file cannot be created.
pub fn atomic_write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| std::io::Error::other("path has no parent"))?;
    create_private_dir(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        uuid::Uuid::new_v4().simple()
    ));
    let mut f = create_private_file(&tmp)?;
    let res = f.write_all(bytes).and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Create `dir` (and parents) so the leaf is private to the current user: mode 0700 on Unix,
/// a protected owner-only DACL on Windows. An existing directory is left as is.
#[cfg(unix)]
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.exists() {
        return Ok(());
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

#[cfg(windows)]
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    // The leaf data directory gets a protected owner-only DACL (inherited by new children), so
    // other principals cannot delete or replace the credential files inside it.
    match win::create_dir_owner_only(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        r => r,
    }
}

#[cfg(not(any(unix, windows)))]
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)
}

#[cfg(windows)]
fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    win::create_owner_only(path)
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// String SID of the user the process token belongs to (not the spoofable USERNAME).
    pub fn current_user_sid() -> std::io::Result<String> {
        // SAFETY: standard token query; every handle/allocation is released before returning.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
            CloseHandle(token);
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid_str: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid_str) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut n = 0;
            while *sid_str.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, n));
            LocalFree(sid_str.cast());
            Ok(s)
        }
    }

    /// Create directory `path` (must not exist) with a protected, inheritable DACL granting
    /// full access to the current user only.
    pub fn create_dir_owner_only(path: &Path) -> std::io::Result<()> {
        let sid = current_user_sid()?;
        let sddl = wide(std::ffi::OsStr::new(&format!("D:P(A;OICI;FA;;;{sid})")));
        let wpath = wide(path.as_os_str());
        // SAFETY: the descriptor lives until LocalFree after CreateDirectoryW.
        unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };
            let ok = CreateDirectoryW(wpath.as_ptr(), &sa);
            let err = std::io::Error::last_os_error();
            LocalFree(sd);
            if ok == 0 {
                return Err(err);
            }
            Ok(())
        }
    }

    /// Create `path` (must not exist) with a protected DACL granting full access to the
    /// current user only.
    pub fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
        let sid = current_user_sid()?;
        let sddl = wide(std::ffi::OsStr::new(&format!("D:P(A;;FA;;;{sid})")));
        let wpath = wide(path.as_os_str());
        // SAFETY: the descriptor lives until LocalFree after CreateFileW; the returned handle
        // is owned by the File.
        unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };
            let h = CreateFileW(wpath.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, &sa, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, std::ptr::null_mut());
            let err = std::io::Error::last_os_error();
            LocalFree(sd);
            if h == INVALID_HANDLE_VALUE {
                return Err(err);
            }
            Ok(std::fs::File::from_raw_handle(h as _))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_refresh_token_and_delete_works() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        store
            .save_merged(&Credentials {
                access_token: Some("a1".into()),
                refresh_token: Some("r1".into()),
                ..Default::default()
            })
            .unwrap();
        let m = store
            .save_merged(&Credentials { access_token: Some("a2".into()), ..Default::default() })
            .unwrap();
        assert_eq!(m.refresh_token.as_deref(), Some("r1"));
        assert_eq!(m.access_token.as_deref(), Some("a2"));
        store.delete().unwrap();
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn debug_is_redacted() {
        let c = Credentials { access_token: Some("SECRET".into()), ..Default::default() };
        assert!(!format!("{c:?}").contains("SECRET"));
    }

    #[test]
    fn a_damaged_file_is_reported_without_quoting_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        // A token where a number belongs: serde's own message would quote it.
        std::fs::write(store.path(), br#"{"access_token":"x","expiry_date":"ya29.SECRET-TOKEN"}"#).unwrap();
        let err = store.load().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(!err.to_string().contains("SECRET"), "{err}");
        assert!(err.to_string().contains("line 1"), "{err}");
        std::fs::write(store.path(), b"{not json").unwrap();
        assert!(store.load().unwrap_err().to_string().contains("is not valid JSON"));
    }

    #[cfg(windows)]
    #[test]
    fn credential_file_acl_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        store.save_merged(&Credentials { refresh_token: Some("r".into()), ..Default::default() }).unwrap();
        let out = std::process::Command::new("icacls").arg(store.path()).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        // Exactly one ACE, not inherited (no "(i)").
        let aces: Vec<_> = text.lines().filter(|l| l.contains(":(")).collect();
        assert_eq!(aces.len(), 1, "{text}");
        assert!(!aces[0].contains("(i)"), "{text}");
        assert!(win::current_user_sid().unwrap().starts_with("S-1-"));
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_mode_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::in_dir(dir.path());
        store.save_merged(&Credentials { refresh_token: Some("r".into()), ..Default::default() }).unwrap();
        assert_eq!(std::fs::metadata(store.path()).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
