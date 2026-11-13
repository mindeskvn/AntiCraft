//! `@` mention popup (ORCHESTRATION.md section D.5).
//!
//! When the caret follows `@query` (no whitespace), the composer opens a popup with workspace
//! files and folders ranked by [`crate::agent::context::fuzzy_score`]. Choosing an entry
//! replaces `@query` with an `@[label](path)` token that the agent resolves. The file index is
//! built off the UI thread ([`IndexJob`], a plain `std::thread` running walkdir) and is bounded
//! by [`MAX_INDEX_ENTRIES`] and [`MAX_WALK_VISITS`]. A backend index (`AppEvent::WorkspaceIndex`)
//! can replace it through `composer::set_workspace_index`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::agent::context::{fuzzy_score, Mention};

/// Max rows shown.
pub const MAX_ROWS: usize = 12;
/// Max entries kept in the index (files and folders).
pub const MAX_INDEX_ENTRIES: usize = 20_000;
/// Max directory entries visited by one walk (skipped ones included), so a huge tree cannot
/// keep the worker busy for long.
pub const MAX_WALK_VISITS: usize = 200_000;
/// Max walk depth below the workspace root.
pub const MAX_DEPTH: usize = 16;
/// The index is rebuilt when the popup opens and it is older than this.
pub const INDEX_STALE_AFTER: Duration = Duration::from_secs(30);
/// Directory names never indexed (at any depth).
pub const SKIP_DIRS: &[&str] = &[".git", ".hg", ".svn", "target", "node_modules", ".venv", "__pycache__", ".idea", ".vs"];

/// Popup state kept by the composer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MentionPopup {
    /// Workspace-relative paths (`/` separators, folders end with `/`).
    pub index: Vec<String>,
    /// Byte range of `@query` in the input while the popup is open.
    pub span: Option<(usize, usize)>,
    pub selected: usize,
    /// `@` position the user dismissed with Esc; the popup stays closed for it.
    pub dismissed: Option<usize>,
    /// Query of the cached rows (`None`: nothing cached).
    cached_query: Option<String>,
    cached: Vec<String>,
}

impl MentionPopup {
    /// Replace the index (clears the ranking cache).
    pub fn set_index(&mut self, index: Vec<String>) {
        self.index = index;
        self.cached_query = None;
        self.cached.clear();
    }

    /// Ranked rows for `query`, recomputed only when the query or index changes.
    pub fn rows(&mut self, query: &str) -> &[String] {
        if self.cached_query.as_deref() != Some(query) {
            self.cached = matches(&self.index, query).into_iter().map(str::to_owned).collect();
            self.cached_query = Some(query.to_owned());
            self.selected = 0;
        }
        &self.cached
    }
}

/// Find an active `@query` ending at byte offset `caret`. Returns (start of `@`, query).
pub fn active_query(input: &str, caret: usize) -> Option<(usize, &str)> {
    let head = input.get(..caret)?;
    let at = head.rfind('@')?;
    let q = &head[at + 1..];
    let boundary = at == 0 || head[..at].ends_with(char::is_whitespace);
    (boundary && !q.contains(char::is_whitespace) && !q.starts_with('[')).then_some((at, q))
}

/// Ranked matches for `query` (empty query: shortest paths first). A match on the file name
/// scores above the same match spread over the path.
pub fn matches<'a>(index: &'a [String], query: &str) -> Vec<&'a str> {
    let mut v: Vec<(i32, &str)> = index
        .iter()
        .filter_map(|p| {
            let full = fuzzy_score(query, p)?;
            let name = p.trim_end_matches('/').rsplit('/').next().unwrap_or(p);
            let base = if query.is_empty() { None } else { fuzzy_score(query, name).map(|s| s + 8) };
            Some((full.max(base.unwrap_or(i32::MIN)), p.as_str()))
        })
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(b.1)));
    v.into_iter().take(MAX_ROWS).map(|(_, p)| p).collect()
}

/// Replace `input[start..end]` with the mention token plus a space; returns the new caret.
pub fn insert(input: &mut String, start: usize, end: usize, path: &str) -> usize {
    let label = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path).to_owned();
    let token = Mention { label, path: path.to_owned(), is_dir: path.ends_with('/') }.to_token() + " ";
    input.replace_range(start..end, &token);
    start + token.len()
}

/// Glob set built from the workspace root `.gitignore` (negations are ignored; a pattern
/// without `/` matches at any depth, a leading `/` anchors it to the root).
pub fn gitignore_globs(root: &Path) -> Option<GlobSet> {
    let text = std::fs::read_to_string(root.join(".gitignore")).ok()?;
    let mut b = GlobSetBuilder::new();
    let mut any = false;
    for line in text.lines().take(500) {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with('!') {
            continue;
        }
        let l = l.trim_end_matches('/');
        let pat = match l.strip_prefix('/') {
            Some(anchored) => anchored.to_owned(),
            None if l.contains('/') => l.to_owned(),
            None => format!("**/{l}"),
        };
        if let Ok(g) = Glob::new(&pat) {
            b.add(g);
            any = true;
        }
    }
    if any {
        b.build().ok()
    } else {
        None
    }
}

/// Walk `root` (no symlink following) and return workspace-relative paths, folders with a
/// trailing `/`, in file-name order. Bounded by `cap` entries and [`MAX_WALK_VISITS`].
pub fn build_index(root: &Path, cap: usize) -> Vec<String> {
    let ignore = gitignore_globs(root);
    let mut out = Vec::new();
    let mut visits = 0usize;
    let mut it = walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(MAX_DEPTH)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0 || !(e.file_type().is_dir() && SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
        });
    while let Some(entry) = it.next() {
        visits += 1;
        if visits > MAX_WALK_VISITS || out.len() >= cap {
            break;
        }
        let Ok(e) = entry else { continue };
        if e.depth() == 0 {
            continue;
        }
        let Ok(rel) = e.path().strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let is_dir = e.file_type().is_dir();
        if ignore.as_ref().is_some_and(|g| g.is_match(&rel)) {
            if is_dir {
                it.skip_current_dir();
            }
            continue;
        }
        out.push(if is_dir { format!("{rel}/") } else { rel });
    }
    out
}

/// A background index build. The worker never touches UI state; the composer polls
/// [`IndexJob::poll`] once per frame (a `try_lock`, never blocking).
#[derive(Debug, Clone)]
pub struct IndexJob {
    pub root: PathBuf,
    pub started: Instant,
    result: Arc<Mutex<Option<Vec<String>>>>,
}

impl IndexJob {
    /// Start indexing `root` on a new thread; `done` runs on the worker when finished
    /// (the composer passes `ctx.request_repaint`).
    pub fn spawn(root: PathBuf, done: impl FnOnce() + Send + 'static) -> Self {
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        let r = root.clone();
        let spawned = std::thread::Builder::new().name("mention-index".into()).spawn(move || {
            let idx = build_index(&r, MAX_INDEX_ENTRIES);
            if let Ok(mut g) = slot.lock() {
                *g = Some(idx);
            }
            done();
        });
        if spawned.is_err() {
            // No thread: deliver an empty index rather than waiting forever.
            if let Ok(mut g) = result.lock() {
                *g = Some(Vec::new());
            }
        }
        IndexJob { root, started: Instant::now(), result }
    }

    /// The finished index, once (`None` while running or already taken).
    pub fn poll(&self) -> Option<Vec<String>> {
        self.result.try_lock().ok().and_then(|mut g| g.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_detection_and_insert() {
        assert_eq!(active_query("see @src/ma", 11), Some((4, "src/ma")));
        assert_eq!(active_query("mail@host", 9), None);
        assert_eq!(active_query("@a b", 4), None);
        let mut s = "see @ma".to_owned();
        let caret = insert(&mut s, 4, 7, "src/main.rs");
        assert_eq!(s, "see @[main.rs](src/main.rs) ");
        assert_eq!(caret, s.len());
    }

    #[test]
    fn ranking() {
        let idx = vec!["src/main.rs".to_owned(), "docs/manual.md".to_owned(), "src/".to_owned()];
        assert_eq!(matches(&idx, "main")[0], "src/main.rs");
        let idx = vec!["a/composer/x.rs".to_owned(), "src/ui/composer.rs".to_owned()];
        assert_eq!(matches(&idx, "composer.rs")[0], "src/ui/composer.rs");
        let many: Vec<String> = (0..50).map(|i| format!("f{i:02}.txt")).collect();
        assert_eq!(matches(&many, "").len(), MAX_ROWS);
    }

    #[test]
    fn popup_caches_rows() {
        let mut p = MentionPopup::default();
        p.set_index(vec!["src/lib.rs".into(), "README.md".into()]);
        assert_eq!(p.rows("lib"), ["src/lib.rs".to_owned()]);
        p.selected = 1;
        assert_eq!(p.rows("lib").len(), 1);
        assert_eq!(p.selected, 1, "same query keeps the selection");
        p.rows("read");
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn index_skips_and_bounds() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        for dir in ["src/ui", "target/debug", "node_modules/x", "build"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for f in ["src/ui/a.rs", "target/debug/b", "build/out.o", "notes.log"] {
            std::fs::write(root.join(f), "").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "build/\n*.log\n# c\n!keep\n").unwrap();
        let idx = build_index(root, MAX_INDEX_ENTRIES);
        assert!(idx.contains(&"src/".to_owned()));
        assert!(idx.contains(&"src/ui/a.rs".to_owned()));
        assert!(idx.iter().all(|p| !p.starts_with("target") && !p.starts_with("node_modules")), "{idx:?}");
        assert!(idx.iter().all(|p| !p.starts_with("build") && !p.ends_with(".log")), "{idx:?}");
        assert_eq!(build_index(root, 2).len(), 2);
    }

    #[test]
    fn index_job_delivers_once() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x.txt"), "").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let job = IndexJob::spawn(d.path().to_owned(), move || tx.send(()).unwrap());
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(job.poll(), Some(vec!["x.txt".to_owned()]));
        assert_eq!(job.poll(), None);
    }
}
