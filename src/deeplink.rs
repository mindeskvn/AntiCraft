//! `antigravity://` deep links (SPEC §9).

use std::sync::{Arc, Mutex};

/// Derive the scheme from an app name: lowercase, runs of non `[a-z0-9]`
/// replaced by `-`, leading/trailing `-` trimmed. `Antigravity` → `antigravity`.
pub fn scheme_for(app_name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in app_name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// The scheme used by this build.
pub fn scheme() -> String {
    scheme_for(crate::paths::APP_NAME)
}

/// First argv entry with a plain `<scheme>://` prefix.
pub fn find_in_argv<S: AsRef<str>>(argv: &[S], scheme: &str) -> Option<String> {
    let prefix = format!("{scheme}://");
    argv.iter().map(AsRef::as_ref).find(|a| a.starts_with(&prefix)).map(str::to_owned)
}

/// Pending deep link pulled by `deep-link:get-stored` (cleared on every read).
#[derive(Clone, Default)]
pub struct PendingDeepLink(Arc<Mutex<Option<String>>>);

impl PendingDeepLink {
    pub fn store(&self, url: String) {
        *self.0.lock().unwrap() = Some(url);
    }
    /// Return and clear.
    pub fn take(&self) -> Option<String> {
        self.0.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_rules() {
        assert_eq!(scheme_for("Antigravity"), "antigravity");
        assert_eq!(scheme_for("Antigravity Insiders!"), "antigravity-insiders");
        assert_eq!(scheme(), "antigravity");
    }

    #[test]
    fn argv_and_pending() {
        let a = ["x.exe", "--flag", "antigravity://auth?c=1", "antigravity://b"];
        assert_eq!(find_in_argv(&a, "antigravity").as_deref(), Some("antigravity://auth?c=1"));
        let p = PendingDeepLink::default();
        p.store("u".into());
        assert_eq!(p.take().as_deref(), Some("u"));
        assert_eq!(p.take(), None);
    }
}
