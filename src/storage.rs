//! `app_storage.json` key/value store (SPEC §6.1).
//!
//! Flat JSON object, 2-space indented, no atomic rename. Defaults are merged
//! on read only and never written. `null` in a change set deletes the key.

use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A change set: `null` deletes a key, any other value is stored as-is.
pub type Changes = Map<String, Value>;

/// Subscriber callback invoked after every successful `update_items`.
/// The UI registers one that broadcasts `storage:changed`; the SettingsService
/// registers one for side effects.
pub type ChangeListener = Arc<dyn Fn(&Changes) + Send + Sync>;

/// Opaque handle returned by [`StorageManager::subscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

/// Errors surfaced to callers. Only directory creation and write errors are
/// rethrown (read/parse errors are swallowed, as in the original).
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("failed to create storage directory: {0}")]
    CreateDir(#[source] std::io::Error),
    #[error("failed to write storage file: {0}")]
    Write(#[source] std::io::Error),
}

/// Flat JSON object store with in-memory defaults.
pub struct StorageManager {
    path: PathBuf,
    defaults: BTreeMap<String, String>,
    inner: Mutex<Inner>,
    /// Serializes read-modify-write cycles in `update_items`.
    write_lock: Mutex<()>,
}

struct Inner {
    next_id: u64,
    listeners: Vec<(SubscriptionId, ChangeListener)>,
}

impl StorageManager {
    /// Create a manager over `path` with string-encoded `defaults`.
    pub fn new(path: impl Into<PathBuf>, defaults: BTreeMap<String, String>) -> Self {
        Self {
            path: path.into(),
            defaults,
            inner: Mutex::new(Inner { next_id: 1, listeners: Vec::new() }),
            write_lock: Mutex::new(()),
        }
    }

    /// Path of the backing file.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Default value for `key`, if any.
    #[allow(dead_code)]
    pub fn default_for(&self, key: &str) -> Option<&str> {
        self.defaults.get(key).map(String::as_str)
    }

    /// Stored items merged with defaults (a default fills a key only when absent).
    pub fn get_items(&self) -> Map<String, Value> {
        let mut items = read_raw(&self.path);
        for (k, v) in &self.defaults {
            items.entry(k.clone()).or_insert_with(|| Value::String(v.clone()));
        }
        items
    }

    /// Single item as a string (non-string JSON values are rendered with `to_string`).
    pub fn get_string(&self, key: &str) -> Option<String> {
        self.get_items().get(key).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }

    /// §6.1 update: merge `changes` into the stored object, persist it, and
    /// only then tell subscribers. Persist errors are returned and suppress
    /// the notification.
    pub fn update_items(&self, changes: &Changes) -> Result<(), StorageError> {
        {
            let _serialized = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
            let merged = merge_changes(read_raw(&self.path), changes);
            persist(&self.path, &merged)?;
        }
        self.notify(changes);
        Ok(())
    }

    fn notify(&self, changes: &Changes) {
        let snapshot: Vec<ChangeListener> = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .listeners
            .iter()
            .map(|(_, l)| Arc::clone(l))
            .collect();
        snapshot.iter().for_each(|l| l(changes));
    }

    /// Register a change listener.
    pub fn subscribe(&self, listener: ChangeListener) -> SubscriptionId {
        let mut inner = self.inner.lock().unwrap();
        let id = SubscriptionId(inner.next_id);
        inner.next_id += 1;
        inner.listeners.push((id, listener));
        id
    }

    /// Remove a change listener. Returns whether it existed.
    #[allow(dead_code)]
    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.listeners.len();
        inner.listeners.retain(|(i, _)| *i != id);
        before != inner.listeners.len()
    }
}

/// Pure merge: `null` removes a key, anything else replaces it.
fn merge_changes(mut base: Map<String, Value>, changes: &Changes) -> Map<String, Value> {
    for (key, value) in changes {
        match value {
            Value::Null => {
                base.remove(key);
            }
            v => {
                base.insert(key.clone(), v.clone());
            }
        }
    }
    base
}

/// Write the whole object as 2-space JSON, creating the parent dir if needed.
fn persist(path: &Path, items: &Map<String, Value>) -> Result<(), StorageError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty() && !p.is_dir()) {
        std::fs::create_dir_all(parent).map_err(StorageError::CreateDir)?;
    }
    let body = serde_json::to_vec_pretty(items).unwrap_or_else(|_| b"{}".to_vec());
    std::fs::write(path, body).map_err(StorageError::Write)
}

/// Read the raw file; missing/empty/unparseable/non-object → `{}`.
fn read_raw(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn defaults() -> BTreeMap<String, String> {
        [("runInBackground".to_string(), "false".to_string())].into_iter().collect()
    }

    #[test]
    fn defaults_merge_without_write() {
        let d = tempfile::tempdir().unwrap();
        let s = StorageManager::new(d.path().join("app_storage.json"), defaults());
        assert_eq!(s.get_string("runInBackground").as_deref(), Some("false"));
        assert!(!s.path().exists());
    }

    #[test]
    fn update_null_deletes_and_notifies() {
        let d = tempfile::tempdir().unwrap();
        let s = StorageManager::new(d.path().join("sub").join("app_storage.json"), defaults());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        s.subscribe(Arc::new(move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        }));
        let mut c = Changes::new();
        c.insert("a".into(), Value::String("1".into()));
        s.update_items(&c).unwrap();
        assert_eq!(s.get_string("a").as_deref(), Some("1"));
        let mut c = Changes::new();
        c.insert("a".into(), Value::Null);
        s.update_items(&c).unwrap();
        assert_eq!(s.get_string("a"), None);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn corrupt_file_reads_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("app_storage.json");
        std::fs::write(&p, "{not json").unwrap();
        let s = StorageManager::new(&p, BTreeMap::new());
        assert!(s.get_items().is_empty());
    }

    #[test]
    fn non_string_values_and_indent() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("app_storage.json");
        let s = StorageManager::new(&p, defaults());
        let mut c = Changes::new();
        c.insert("n".into(), Value::from(3));
        c.insert("b".into(), Value::Bool(true));
        s.update_items(&c).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("\n  \"b\": true"));
        assert!(!text.contains("runInBackground"), "defaults must not be written");
        assert_eq!(s.get_items().get("n"), Some(&Value::from(3)));
        assert_eq!(s.get_string("n").as_deref(), Some("3"));
    }

    #[test]
    fn stored_value_overrides_default_and_unsubscribe() {
        let d = tempfile::tempdir().unwrap();
        let s = StorageManager::new(d.path().join("app_storage.json"), defaults());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let id = s.subscribe(Arc::new(move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(s.unsubscribe(id));
        assert!(!s.unsubscribe(id));
        let mut c = Changes::new();
        c.insert("runInBackground".into(), Value::String("true".into()));
        s.update_items(&c).unwrap();
        assert_eq!(s.get_string("runInBackground").as_deref(), Some("true"));
        assert_eq!(s.default_for("runInBackground"), Some("false"));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn corrupt_file_overwritten_by_update() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("app_storage.json");
        std::fs::write(&p, "garbage").unwrap();
        let s = StorageManager::new(&p, BTreeMap::new());
        let mut c = Changes::new();
        c.insert("k".into(), Value::String("v".into()));
        s.update_items(&c).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({"k": "v"}));
    }
}
