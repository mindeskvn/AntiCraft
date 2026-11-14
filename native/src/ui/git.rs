//! The Git branch a project folder has checked out, read straight from `.git/HEAD` (no `git`
//! process), for the label beside the project picker. Read-only: AntiCraft never switches
//! branches itself.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a read is reused before `.git/HEAD` is read again.
const REFRESH: Duration = Duration::from_secs(3);

/// What `HEAD` points at: a branch name, or the commit of a detached `HEAD` (short hash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    Detached(String),
}

impl Head {
    pub fn label(&self) -> &str {
        match self {
            Head::Branch(b) | Head::Detached(b) => b,
        }
    }
}

/// Parse the contents of a `HEAD` file.
pub fn parse_head(contents: &str) -> Option<Head> {
    let line = contents.lines().next()?.trim();
    if let Some(r) = line.strip_prefix("ref:") {
        let r = r.trim();
        let name = r.strip_prefix("refs/heads/").unwrap_or(r);
        return (!name.is_empty()).then(|| Head::Branch(name.to_owned()));
    }
    let hex = line.len() >= 7 && line.chars().all(|c| c.is_ascii_hexdigit());
    hex.then(|| Head::Detached(line[..7].to_owned()))
}

/// The `.git` directory of the repository `dir` belongs to: `dir/.git`, or the folder a `.git`
/// file points at (worktrees and submodules), walking up to the drive root.
fn git_dir(dir: &Path) -> Option<PathBuf> {
    for d in dir.ancestors() {
        let dot = d.join(".git");
        if dot.is_dir() {
            return Some(dot);
        }
        if dot.is_file() {
            let text = std::fs::read_to_string(&dot).ok()?;
            let target = text.lines().find_map(|l| l.trim().strip_prefix("gitdir:"))?.trim();
            let p = PathBuf::from(target);
            return Some(if p.is_absolute() { p } else { d.join(p) });
        }
    }
    None
}

/// `HEAD` of the repository holding `dir`, if it is in one.
pub fn head(dir: &Path) -> Option<Head> {
    let text = std::fs::read_to_string(git_dir(dir)?.join("HEAD")).ok()?;
    parse_head(&text)
}

/// [`head`], read at most every [`REFRESH`] per folder (kept in egui memory), so a branch
/// switched in a terminal shows up within seconds without reading a file every frame.
pub fn head_cached(ctx: &egui::Context, dir: &Path) -> Option<Head> {
    let id = egui::Id::unique(("anticraft_git_head", dir));
    let now = Instant::now();
    if let Some((at, h)) = ctx.data(|d| d.get_temp::<(Instant, Option<Head>)>(id)) {
        if now.duration_since(at) < REFRESH {
            return h;
        }
    }
    let h = head(dir);
    ctx.data_mut(|d| d.insert_temp(id, (now, h.clone())));
    ctx.request_repaint_after(REFRESH);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_names_the_branch_or_the_commit() {
        assert_eq!(parse_head("ref: refs/heads/main\n"), Some(Head::Branch("main".into())));
        assert_eq!(parse_head("ref: refs/heads/feature/ui-2.31"), Some(Head::Branch("feature/ui-2.31".into())));
        assert_eq!(parse_head("3f2a9c1d0b8e7f6a5b4c3d2e1f0a9b8c7d6e5f4a\n"), Some(Head::Detached("3f2a9c1".into())));
        assert_eq!(parse_head(""), None);
        assert_eq!(parse_head("not a head"), None);
    }

    #[test]
    fn a_worktree_follows_its_git_file() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        std::fs::write(repo.path().join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let sub = repo.path().join("src").join("ui");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(head(&sub), Some(Head::Branch("main".into())), "a subfolder belongs to the repo");

        let wt = tempfile::tempdir().unwrap();
        let admin = repo.path().join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        std::fs::write(wt.path().join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        assert_eq!(head(wt.path()), Some(Head::Branch("topic".into())));
    }
}
