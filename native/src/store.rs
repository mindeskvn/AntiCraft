//! Conversation persistence: one JSON file per conversation under
//! `%APPDATA%\AntiCraft\native\conversations\{id}.json`, written atomically.
//!
//! **Auto-backup**: every `save` copies the previous file to
//! `.backups/{id}/{timestamp}_{seq}.json` before overwriting. Backups are
//! silent best-effort — failure is logged but never returned as an error.
//!
//! **Checkpoints**: `save_checkpoint` writes a snapshot of the history into
//! `.checkpoints/{id}/{index}.json`. The index is the number of user messages
//! already in the history when the snapshot was taken, so loading back is
//! `load_checkpoint(id, n)`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::artifacts::{ArtifactMeta, ArtifactStore};
use crate::api::Content;

/// Max chars of the auto-generated title.
pub const TITLE_MAX_CHARS: usize = 60;
/// Maximum backup files kept per conversation (oldest pruned).
const MAX_BACKUPS: usize = 50;
/// Maximum checkpoints kept per conversation (oldest pruned).
pub const MAX_CHECKPOINTS: usize = 50;

/// Write `bytes` to `path` via a sibling temp file + rename. Creates parent dirs.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        Uuid::new_v4().simple()
    ));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        let res = f.write_all(bytes).and_then(|()| f.sync_all());
        if let Err(e) = res {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    // std::fs::rename replaces an existing destination on Windows (MoveFileExW REPLACE_EXISTING).
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Copy the existing file at `src` into `backup_dir/{prefix}_{seq}.json`, then prune so the dir
/// holds at most `max_keep` files. Silent best-effort: errors are traced but never propagated.
pub(crate) fn backup_file(src: &Path, backup_dir: &Path, prefix: &str, max_keep: usize) {
    let Ok(data) = std::fs::read(src) else { return };
    if let Err(e) = std::fs::create_dir_all(backup_dir) {
        tracing::debug!("backup mkdir: {e}");
        return;
    }
    // Millisecond timestamp + random suffix to avoid collisions.
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("{prefix}_{ts}_{}.json", &Uuid::new_v4().simple().to_string()[..8]);
    let dest = backup_dir.join(&name);
    if let Err(e) = atomic_write(&dest, &data) {
        tracing::debug!("backup write {}: {e}", dest.display());
        return;
    }
    prune_dir(backup_dir, max_keep);
}

/// Keep at most `max_keep` files in `dir`, removing the oldest by file name (alphabetical, which
/// works for timestamp-prefixed names). Does nothing when the dir does not exist or is empty.
fn prune_dir(dir: &Path, max_keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_owned()))
        .collect();
    if names.len() <= max_keep {
        return;
    }
    names.sort();
    for name in names.iter().take(names.len() - max_keep) {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// A checkpoint snapshot — the messages stored before a particular user turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Position: number of user messages already in the history when this was taken. Used as
    /// the display label ("Before message 3") and as the restore key.
    pub user_index: usize,
    /// The history at the point of the snapshot (no thought parts, same as [`Conversation::messages`]).
    pub messages: Vec<crate::api::Content>,
}

/// Full persisted conversation. `messages` is the API history (no thought parts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub title: String,
    pub workspace: Option<PathBuf>,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub messages: Vec<Content>,
    /// Artifact metadata (content lives in `{id}/artifacts/`, see [`ArtifactStore`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactMeta>,
}

impl Conversation {
    pub fn new(workspace: Option<PathBuf>, model: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            title: String::new(),
            workspace,
            model: model.into(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    /// Set the title from the first prompt if it is still empty.
    pub fn ensure_title(&mut self, first_prompt: &str) {
        if self.title.is_empty() {
            self.title = auto_title(first_prompt);
        }
    }

    pub fn summary(&self) -> ConversationSummary {
        ConversationSummary {
            id: self.id,
            title: self.title.clone(),
            workspace: self.workspace.clone(),
            model: self.model.clone(),
            updated_at: self.updated_at,
        }
    }
}

/// Lightweight listing entry for the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub id: Uuid,
    pub title: String,
    pub workspace: Option<PathBuf>,
    pub model: String,
    pub updated_at: DateTime<Utc>,
}

/// First line of the prompt, whitespace-collapsed, cut at [`TITLE_MAX_CHARS`] with an ellipsis.
pub fn auto_title(prompt: &str) -> String {
    let line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return "New chat".into();
    }
    if collapsed.chars().count() > TITLE_MAX_CHARS {
        let cut: String = collapsed.chars().take(TITLE_MAX_CHARS - 1).collect();
        format!("{}\u{2026}", cut.trim_end())
    } else {
        collapsed
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("conversation not found")]
    NotFound,
    #[error("storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt conversation file: {0}")]
    Json(#[from] serde_json::Error),
}

/// Directory-backed conversation store.
#[derive(Debug, Clone)]
pub struct ConversationStore {
    dir: PathBuf,
}

impl ConversationStore {
    /// `{data_dir}/conversations`.
    pub fn new(data_dir: &Path) -> Self {
        Self { dir: data_dir.join("conversations") }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// All conversations, newest first. Unreadable files are skipped with a warning.
    pub fn list(&self) -> Result<Vec<ConversationSummary>, StoreError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&p).map_err(StoreError::from).and_then(|b| Ok(serde_json::from_slice::<Conversation>(&b)?)) {
                Ok(c) => out.push(c.summary()),
                Err(e) => tracing::warn!("skipping conversation file {}: {e}", p.display()),
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(out)
    }

    pub fn load(&self, id: Uuid) -> Result<Conversation, StoreError> {
        match std::fs::read(self.path(id)) {
            Ok(b) => Ok(serde_json::from_slice(&b)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::NotFound),
            Err(e) => Err(e.into()),
        }
    }

    /// Save, bumping `updated_at`.
    /// Save, bumping `updated_at`. Silently backs up the previous file first.
    pub fn save(&self, conv: &mut Conversation) -> Result<(), StoreError> {
        // Auto-backup: copy the existing file before overwriting.
        let path = self.path(conv.id);
        backup_file(&path, &self.backup_dir(conv.id), "auto", MAX_BACKUPS);
        conv.updated_at = Utc::now();
        let json = serde_json::to_vec_pretty(conv)?;
        atomic_write(&path, &json)?;
        Ok(())
    }

    /// Save a checkpoint of `messages` before turn number `user_index` starts.
    ///
    /// `user_index` is the count of user messages already in the history — it serves as the
    /// label ("Before message 3") and the restore key. A checkpoint with the same key is
    /// silently overwritten, so re-sends don't pile up. Old checkpoints are pruned to
    /// [`MAX_CHECKPOINTS`].
    pub fn save_checkpoint(&self, id: Uuid, user_index: usize, messages: &[crate::api::Content]) -> std::io::Result<()> {
        let cp = Checkpoint { user_index, messages: messages.to_vec() };
        let json = serde_json::to_vec_pretty(&cp).map_err(std::io::Error::other)?;
        atomic_write(&self.checkpoint_path(id, user_index), &json)?;
        prune_dir(&self.checkpoint_dir(id), MAX_CHECKPOINTS);
        Ok(())
    }

    /// Load a checkpoint, or `None` when it does not exist.
    pub fn load_checkpoint(&self, id: Uuid, user_index: usize) -> Option<Checkpoint> {
        let bytes = std::fs::read(self.checkpoint_path(id, user_index)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// All checkpoints for `id`, sorted oldest first.
    pub fn list_checkpoints(&self, id: Uuid) -> Vec<Checkpoint> {
        let dir = self.checkpoint_dir(id);
        let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
        let mut out: Vec<Checkpoint> = entries
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .filter_map(|e| std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice(&b).ok()))
            .collect();
        out.sort_by_key(|c| c.user_index);
        out
    }

    /// Directories for backup and checkpoint data of `id`.
    fn backup_dir(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!(".backups/{id}"))
    }
    fn checkpoint_dir(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!(".checkpoints/{id}"))
    }
    fn checkpoint_path(&self, id: Uuid, user_index: usize) -> PathBuf {
        self.checkpoint_dir(id).join(format!("{user_index:05}.json"))
    }

    /// Artifact store of one conversation (`{dir}/{id}/artifacts`).
    pub fn artifacts(&self, id: Uuid) -> ArtifactStore {
        ArtifactStore::new(&self.dir, id)
    }

    /// Delete the conversation file and its artifact directory.
    pub fn delete(&self, id: Uuid) -> Result<(), StoreError> {
        let removed = std::fs::remove_file(self.path(id));
        if let Err(e) = self.artifacts(id).remove_all() {
            tracing::warn!("could not remove artifacts of conversation {id}: {e}");
        }
        match removed {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::NotFound),
            Err(e) => Err(e.into()),
        }
    }

    pub fn rename(&self, id: Uuid, title: &str) -> Result<(), StoreError> {
        let mut c = self.load(id)?;
        let t = title.split_whitespace().collect::<Vec<_>>().join(" ");
        c.title = if t.is_empty() { "New chat".into() } else { t };
        self.save(&mut c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(dir.path());
        assert!(store.list().unwrap().is_empty());
        let mut c = Conversation::new(None, "gemini-2.5-pro");
        c.ensure_title("  fix the   bug\nmore");
        c.messages.push(Content::user_text("fix the bug"));
        store.save(&mut c).unwrap();
        assert_eq!(store.load(c.id).unwrap(), c);
        store.rename(c.id, "Renamed").unwrap();
        assert_eq!(store.list().unwrap()[0].title, "Renamed");
        store.delete(c.id).unwrap();
        assert!(matches!(store.load(c.id), Err(StoreError::NotFound)));
    }

    #[test]
    fn list_sorted_skips_corrupt_and_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(dir.path());
        let mut a = Conversation::new(Some(PathBuf::from("C:/ws")), "m");
        store.save(&mut a).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut b = Conversation::new(None, "m");
        store.save(&mut b).unwrap();
        std::fs::write(store.dir().join("broken.json"), b"{not json").unwrap();
        std::fs::write(store.dir().join(".x.json.tmp"), b"{}").unwrap();
        let ids: Vec<_> = store.list().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![b.id, a.id]);
        assert_eq!(store.load(a.id).unwrap().workspace, Some(PathBuf::from("C:/ws")));
    }

    #[test]
    fn missing_and_rename_edge_cases() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(dir.path());
        let id = Uuid::new_v4();
        assert!(matches!(store.delete(id), Err(StoreError::NotFound)));
        assert!(matches!(store.rename(id, "x"), Err(StoreError::NotFound)));
        let mut c = Conversation::new(None, "m");
        c.ensure_title("first");
        c.ensure_title("second");
        assert_eq!(c.title, "first");
        store.save(&mut c).unwrap();
        store.rename(c.id, "   ").unwrap();
        assert_eq!(store.load(c.id).unwrap().title, "New chat");
        // Only one .json file directly in the conversations dir; backup dirs and other
        // hidden directories created by save/rename don't count.
        let leftovers = std::fs::read_dir(store.dir()).unwrap()
            .flatten()
            .filter(|e| {
                let p = e.path();
                p.is_file() && p.extension().and_then(|x| x.to_str()) == Some("json")
            })
            .count();
        assert_eq!(leftovers, 1, "no temp files left behind");
    }

    /// Checkpoints round-trip, keyed by user index, and a re-save at the same index replaces
    /// rather than piles up.
    #[test]
    fn checkpoints_round_trip_and_replace() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(dir.path());
        let id = Uuid::new_v4();
        let m1 = vec![Content::user_text("first")];
        let m2 = vec![Content::user_text("first"), Content::user_text("second")];
        store.save_checkpoint(id, 0, &m1).unwrap();
        store.save_checkpoint(id, 1, &m2).unwrap();
        // Same key: replaced, not duplicated.
        store.save_checkpoint(id, 1, &m1).unwrap();
        let all = store.list_checkpoints(id);
        assert_eq!(all.len(), 2, "two keys");
        assert_eq!(all[0].user_index, 0);
        assert_eq!(all[1].user_index, 1);
        assert_eq!(all[1].messages, m1, "the later save replaced the earlier one");
        assert_eq!(store.load_checkpoint(id, 0).unwrap().messages, m1);
        assert!(store.load_checkpoint(id, 9).is_none(), "missing key is None");
        // Another conversation sees none of it.
        assert!(store.list_checkpoints(Uuid::new_v4()).is_empty());
    }

    /// Every save keeps the previous file in `.backups/{id}/`, and the oldest are pruned.
    #[test]
    fn saves_back_up_the_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(dir.path());
        let mut c = Conversation::new(None, "m");
        store.save(&mut c).unwrap(); // first save: nothing to back up yet
        let backup_dir = store.dir().join(".backups").join(c.id.to_string());
        assert!(!backup_dir.exists() || std::fs::read_dir(&backup_dir).unwrap().count() == 0);

        c.messages.push(Content::user_text("one"));
        store.save(&mut c).unwrap();
        let backups: Vec<_> = std::fs::read_dir(&backup_dir).unwrap().flatten().collect();
        assert_eq!(backups.len(), 1, "the pre-save version is kept");
        // The backup holds the earlier state (no messages).
        let b: Conversation = serde_json::from_slice(&std::fs::read(backups[0].path()).unwrap()).unwrap();
        assert!(b.messages.is_empty());

        // The backup dir never shows up as a conversation.
        assert_eq!(store.list().unwrap().len(), 1);

        // Exceeding MAX_BACKUPS prunes the oldest.
        for i in 0..(MAX_BACKUPS + 3) {
            c.messages.push(Content::user_text(format!("m{i}")));
            store.save(&mut c).unwrap();
        }
        let n = std::fs::read_dir(&backup_dir).unwrap().flatten().count();
        assert_eq!(n, MAX_BACKUPS, "pruned to the cap");
    }

    #[test]
    fn delete_removes_artifacts_and_meta_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(dir.path());
        let mut c = Conversation::new(None, "m");
        let arts = store.artifacts(c.id);
        let meta = arts.write_versioned("task", "- [ ] a", Some("todo")).unwrap();
        c.artifacts.push(meta);
        store.save(&mut c).unwrap();
        assert_eq!(store.load(c.id).unwrap().artifacts, c.artifacts);
        assert_eq!(store.list().unwrap().len(), 1, "artifact dir is not a conversation");
        store.delete(c.id).unwrap();
        assert!(!store.dir().join(c.id.to_string()).exists());
        // Old files without the field still load.
        let mut old = Conversation::new(None, "m");
        store.save(&mut old).unwrap();
        let raw = std::fs::read_to_string(store.dir().join(format!("{}.json", old.id))).unwrap();
        assert!(!raw.contains("artifacts"));
        assert!(store.load(old.id).unwrap().artifacts.is_empty());
    }

    #[test]
    fn titles() {
        assert_eq!(auto_title("  fix the   bug\nmore"), "fix the bug");
        assert_eq!(auto_title("   "), "New chat");
        assert_eq!(auto_title(&"x".repeat(100)).chars().count(), TITLE_MAX_CHARS);
    }

    #[test]
    fn atomic_write_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b.json");
        atomic_write(&p, b"1").unwrap();
        atomic_write(&p, b"2").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"2");
    }
}
