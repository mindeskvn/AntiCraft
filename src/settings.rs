//! Settings layered on top of [`StorageManager`] (SPEC §6.1 known keys).

use crate::storage::{Changes, StorageManager, SubscriptionId};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub const KEY_RUN_IN_BACKGROUND: &str = "runInBackground";
pub const KEY_KEEP_COMPUTER_AWAKE: &str = "keepComputerAwake";
pub const KEY_AUTO_CHECK_FOR_UPDATES: &str = "autoCheckForUpdates";
pub const KEY_ZOOM_LEVEL: &str = "zoomLevel";
#[allow(dead_code)]
pub const KEY_IDE_WIZARD_SHOWN: &str = "ide-install-wizard-shown";

/// Defaults for Windows: runInBackground=false, keepComputerAwake=false,
/// autoCheckForUpdates=true. Booleans are string-encoded.
pub fn default_settings() -> BTreeMap<String, String> {
    [
        (KEY_RUN_IN_BACKGROUND, "false"),
        (KEY_KEEP_COMPUTER_AWAKE, "false"),
        (KEY_AUTO_CHECK_FOR_UPDATES, "true"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Typed setting change delivered to listeners. `None` means the value was
/// set to `null`, i.e. "revert to the default".
#[derive(Debug, Clone, PartialEq)]
pub enum SettingChange {
    RunInBackground(Option<bool>),
    KeepComputerAwake(Option<bool>),
    AutoCheckForUpdates(Option<bool>),
}

/// Listener for typed setting changes (the updater subscribes to
/// `AutoCheckForUpdates`).
pub type SettingListener = Arc<dyn Fn(&SettingChange) + Send + Sync>;

/// Keep-awake via `SetThreadExecutionState` (ES_CONTINUOUS|ES_DISPLAY_REQUIRED|
/// ES_SYSTEM_REQUIRED), the Windows equivalent of `prevent-display-sleep`.
/// Idempotent. The execution state is per-thread, so the blocker owns a
/// dedicated long-lived worker thread that issues every call; `set` may be
/// invoked from any thread.
pub struct PowerBlocker {
    active: bool,
    tx: Option<std::sync::mpsc::Sender<bool>>,
}

impl PowerBlocker {
    pub fn new() -> Self {
        Self { active: false, tx: None }
    }
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        self.active
    }
    /// Start (`true`) or stop (`false`) blocking display sleep.
    pub fn set(&mut self, on: bool) {
        if on == self.active {
            return;
        }
        if self.tx.is_none() {
            self.tx = spawn_power_thread();
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(on);
        }
        self.active = on;
    }
}

/// Worker thread that applies execution-state requests. When the sender is
/// dropped (blocker dropped) the thread clears the state and exits.
fn spawn_power_thread() -> Option<std::sync::mpsc::Sender<bool>> {
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    std::thread::Builder::new()
        .name("anticraft-power".into())
        .spawn(move || {
            while let Ok(on) = rx.recv() {
                apply_execution_state(on);
            }
            apply_execution_state(false);
        })
        .ok()?;
    Some(tx)
}

fn apply_execution_state(on: bool) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Power::{
            SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
        };
        let flags = if on {
            ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED
        } else {
            ES_CONTINUOUS
        };
        SetThreadExecutionState(flags);
    }
    #[cfg(not(windows))]
    let _ = on;
}

impl Default for PowerBlocker {
    fn default() -> Self {
        Self::new()
    }
}

/// Applies setting side effects and fans out typed changes.
pub struct SettingsService {
    storage: Arc<StorageManager>,
    blocker: Mutex<PowerBlocker>,
    listeners: Mutex<Vec<SettingListener>>,
    _sub: Mutex<Option<SubscriptionId>>,
}

impl SettingsService {
    /// Subscribe to storage and apply `keepComputerAwake` from merged items.
    pub fn new(storage: Arc<StorageManager>) -> Arc<Self> {
        let svc = Arc::new(Self {
            storage: storage.clone(),
            blocker: Mutex::new(PowerBlocker::new()),
            listeners: Mutex::new(Vec::new()),
            _sub: Mutex::new(None),
        });
        let weak = Arc::downgrade(&svc);
        let id = storage.subscribe(Arc::new(move |c: &Changes| {
            if let Some(s) = weak.upgrade() {
                s.on_storage_changed(c);
            }
        }));
        *svc._sub.lock().unwrap() = Some(id);
        let awake = svc.keep_computer_awake();
        svc.blocker.lock().unwrap().set(awake);
        svc
    }

    fn bool_setting(&self, key: &str) -> bool {
        self.storage.get_string(key).as_deref() == Some("true")
    }

    pub fn run_in_background(&self) -> bool {
        self.bool_setting(KEY_RUN_IN_BACKGROUND)
    }
    pub fn keep_computer_awake(&self) -> bool {
        self.bool_setting(KEY_KEEP_COMPUTER_AWAKE)
    }
    pub fn auto_check_for_updates(&self) -> bool {
        self.bool_setting(KEY_AUTO_CHECK_FOR_UPDATES)
    }

    /// Persisted zoom level (`parseFloat`; NaN/missing → `None`).
    pub fn zoom_level(&self) -> Option<f64> {
        self.storage
            .get_string(KEY_ZOOM_LEVEL)
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|f| f.is_finite())
    }

    /// Persist zoom level as a string.
    pub fn set_zoom_level(&self, level: f64) {
        let mut c = Changes::new();
        c.insert(KEY_ZOOM_LEVEL.into(), serde_json::Value::String(level.to_string()));
        if let Err(e) = self.storage.update_items(&c) {
            tracing::warn!("failed to persist zoom level: {e}");
        }
    }

    #[allow(dead_code)]
    pub fn storage(&self) -> &Arc<StorageManager> {
        &self.storage
    }

    /// Register a typed setting listener.
    pub fn on_change(&self, l: SettingListener) {
        self.listeners.lock().unwrap().push(l);
    }

    fn on_storage_changed(&self, changes: &Changes) {
        let parse = |v: &serde_json::Value| -> Option<bool> {
            match v {
                serde_json::Value::Null => None,
                serde_json::Value::Bool(b) => Some(*b),
                serde_json::Value::String(s) => Some(s == "true"),
                _ => Some(false),
            }
        };
        let mut typed = Vec::new();
        for (k, v) in changes {
            match k.as_str() {
                KEY_RUN_IN_BACKGROUND => typed.push(SettingChange::RunInBackground(parse(v))),
                KEY_KEEP_COMPUTER_AWAKE => {
                    let on = parse(v).unwrap_or(false);
                    self.blocker.lock().unwrap().set(on);
                    typed.push(SettingChange::KeepComputerAwake(parse(v)));
                }
                KEY_AUTO_CHECK_FOR_UPDATES => typed.push(SettingChange::AutoCheckForUpdates(parse(v))),
                _ => {}
            }
        }
        let ls: Vec<_> = self.listeners.lock().unwrap().clone();
        for c in &typed {
            for l in &ls {
                l(c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_change_fanout() {
        let d = tempfile::tempdir().unwrap();
        let st = Arc::new(StorageManager::new(d.path().join("s.json"), default_settings()));
        let svc = SettingsService::new(st.clone());
        assert!(!svc.run_in_background());
        assert!(svc.auto_check_for_updates());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        svc.on_change(Arc::new(move |c| s2.lock().unwrap().push(c.clone())));
        let mut c = Changes::new();
        c.insert(KEY_AUTO_CHECK_FOR_UPDATES.into(), serde_json::Value::Null);
        st.update_items(&c).unwrap();
        assert_eq!(seen.lock().unwrap()[0], SettingChange::AutoCheckForUpdates(None));
    }

    #[test]
    fn zoom_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let st = Arc::new(StorageManager::new(d.path().join("s.json"), default_settings()));
        let svc = SettingsService::new(st);
        assert_eq!(svc.zoom_level(), None);
        svc.set_zoom_level(1.5);
        assert_eq!(svc.zoom_level(), Some(1.5));
    }

    #[test]
    fn keep_awake_follows_storage() {
        let d = tempfile::tempdir().unwrap();
        let st = Arc::new(StorageManager::new(d.path().join("s.json"), default_settings()));
        let svc = SettingsService::new(st.clone());
        assert!(!svc.blocker.lock().unwrap().is_active());
        let mut c = Changes::new();
        c.insert(KEY_KEEP_COMPUTER_AWAKE.into(), serde_json::Value::String("true".into()));
        st.update_items(&c).unwrap();
        assert!(svc.keep_computer_awake());
        assert!(svc.blocker.lock().unwrap().is_active());
        let mut c = Changes::new();
        c.insert(KEY_KEEP_COMPUTER_AWAKE.into(), serde_json::Value::Null);
        st.update_items(&c).unwrap();
        assert!(!svc.blocker.lock().unwrap().is_active());
        assert!(!svc.keep_computer_awake());
    }

    #[test]
    fn keep_awake_applied_at_init() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        std::fs::write(&p, r#"{"keepComputerAwake":"true","runInBackground":"true"}"#).unwrap();
        let st = Arc::new(StorageManager::new(&p, default_settings()));
        let svc = SettingsService::new(st);
        assert!(svc.blocker.lock().unwrap().is_active());
        assert!(svc.run_in_background());
    }
}
