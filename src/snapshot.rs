//! Shadow-git snapshot system for per-turn undo.
//!
//! Each working directory gets its own shadow git repo at
//! `~/.fast-rlm-agent/snapshots/<dir-slug>/`. Before every agent turn, we
//! commit the current state into that shadow repo. `/undo` checks out the
//! previous commit and truncates the conversation.
//!
//! The user's own git history is never touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A point-in-time snapshot of both filesystem and conversation state.
pub struct Checkpoint {
    /// Commit SHA in the shadow repo.
    pub commit: String,
    /// `App::items.len()` at capture time (before user message was pushed).
    pub items_len: usize,
    /// `App::history.len()` at capture time (before user message was pushed).
    pub history_len: usize,
}

/// Manages a shadow git repo for one working directory.
pub struct Snapshotter {
    /// `~/.fast-rlm-agent/snapshots/<slug>/` — the outer directory.
    dir: PathBuf,
    /// The working directory being snapshotted.
    worktree: PathBuf,
    /// Stack of snapshots, newest at the back.
    checkpoints: Vec<Checkpoint>,
}

impl Snapshotter {
    /// Open (or create) a shadow repo for `cwd`. Returns `None` if git is
    /// unavailable or initialization fails.
    pub fn new(cwd: PathBuf) -> Option<Self> {
        if !git_available() {
            return None;
        }
        let cwd = cwd.canonicalize().unwrap_or(cwd);
        let dir = shadow_dir(&cwd);
        let s = Self {
            dir,
            worktree: cwd,
            checkpoints: Vec::new(),
        };
        s.init().ok()?;
        Some(s)
    }

    /// Snapshot the current state of the worktree. Call this before an agent
    /// turn begins, recording the pre-turn lengths of `items` and `history`.
    ///
    /// Returns `true` on success. Failures are soft — the caller can proceed
    /// without a checkpoint; `/undo` will simply report nothing to undo.
    pub fn capture(&mut self, items_len: usize, history_len: usize) -> bool {
        let ok = (|| -> Result<(), String> {
            self.git(&["add", "-A"])?;
            self.git(&[
                "commit",
                "-m",
                "snapshot",
                "--allow-empty",
                "--author=fast-rlm-agent <agent@local>",
                "--no-gpg-sign",
            ])?;
            Ok(())
        })();
        if ok.is_err() {
            return false;
        }
        match self.git_output(&["rev-parse", "HEAD"]) {
            Ok(sha) => {
                self.checkpoints.push(Checkpoint {
                    commit: sha.trim().to_string(),
                    items_len,
                    history_len,
                });
                true
            }
            Err(_) => false,
        }
    }

    /// Restore the worktree to the most recent checkpoint. Returns the
    /// checkpoint so the caller can truncate conversation state, or `None`
    /// if there is nothing to undo or restoration fails.
    #[allow(dead_code)]
    pub fn restore(&mut self) -> Option<Checkpoint> {
        let cp = self.checkpoints.pop()?;
        if self.restore_commit(&cp.commit).is_err() {
            // Put it back so the user can retry.
            self.checkpoints.push(cp);
            return None;
        }
        Some(cp)
    }

    /// `true` if there is at least one checkpoint available.
    #[allow(dead_code)]
    pub fn can_undo(&self) -> bool {
        !self.checkpoints.is_empty()
    }

    /// Read-only view of the checkpoint stack (oldest first).
    pub fn checkpoints(&self) -> &[Checkpoint] {
        &self.checkpoints
    }

    /// Restore to the checkpoint at `idx` (oldest = 0) and drop that
    /// checkpoint plus all newer ones. Returns the restored checkpoint, or
    /// `None` if `idx` is out of range or the git restore fails.
    pub fn restore_to(&mut self, idx: usize) -> Option<Checkpoint> {
        if idx >= self.checkpoints.len() {
            return None;
        }
        let commit = self.checkpoints[idx].commit.clone();
        let items_len = self.checkpoints[idx].items_len;
        let history_len = self.checkpoints[idx].history_len;

        self.restore_commit(&commit).ok()?;

        // Drop this checkpoint and everything newer.
        self.checkpoints.truncate(idx);
        Some(Checkpoint {
            commit,
            items_len,
            history_len,
        })
    }

    // ---- private -----------------------------------------------------------

    fn git_dir(&self) -> PathBuf {
        self.dir.join(".git")
    }

    /// Build a `Command` pre-loaded with the shadow `--git-dir` and worktree.
    fn cmd(&self) -> Command {
        let mut c = Command::new("git");
        c.arg("--git-dir").arg(self.git_dir());
        c.arg("--work-tree").arg(&self.worktree);
        c
    }

    fn git(&self, args: &[&str]) -> Result<(), String> {
        let out = self.cmd().args(args).output().map_err(|e| e.to_string())?;
        check(out)
    }

    fn git_output(&self, args: &[&str]) -> Result<String, String> {
        let out = self.cmd().args(args).output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn init(&self) -> Result<(), String> {
        if self.git_dir().exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;

        // `git init <dir>` creates <dir>/.git
        let out = Command::new("git")
            .args(["init", "-q"])
            .arg(&self.dir)
            .output()
            .map_err(|e| e.to_string())?;
        check(out)?;

        // Point the shadow repo's worktree at our cwd so git doesn't complain
        // about operating outside it.
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(self.git_dir())
            .args(["config", "core.worktree"])
            .arg(&self.worktree)
            .output()
            .map_err(|e| e.to_string())?;
        check(out)?;

        // Git refuses commits without user identity.
        for (key, val) in [
            ("user.name", "fast-rlm-agent"),
            ("user.email", "agent@local"),
        ] {
            let out = Command::new("git")
                .arg("--git-dir")
                .arg(self.git_dir())
                .args(["config", key, val])
                .output()
                .map_err(|e| e.to_string())?;
            check(out)?;
        }

        Ok(())
    }

    fn restore_commit(&self, commit: &str) -> Result<(), String> {
        // Stage everything so the index reflects current disk state.
        self.git(&["add", "-A"])?;

        // Files currently tracked (after the add above).
        let current_raw = self.git_output(&["ls-files", "--cached"])?;
        let current: std::collections::HashSet<&str> =
            current_raw.lines().filter(|l| !l.is_empty()).collect();

        // Files that exist in the target snapshot commit.
        let snapshot_raw = self.git_output(&["ls-tree", "-r", "--name-only", commit])?;
        let snapshot: std::collections::HashSet<&str> =
            snapshot_raw.lines().filter(|l| !l.is_empty()).collect();

        // Delete files added after the snapshot.
        for file in current.difference(&snapshot) {
            let _ = std::fs::remove_file(self.worktree.join(file));
        }

        // Load the snapshot tree into the index, then write it to disk.
        self.git(&["read-tree", commit])?;
        self.git(&["checkout-index", "-f", "-a"])?;

        Ok(())
    }
}

// ---- helpers ---------------------------------------------------------------

fn check(out: Output) -> Result<(), String> {
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `~/.fast-rlm-agent/snapshots/<slug>` — one directory per working path.
/// The slug is the canonicalized path with `/` replaced by `_`.
fn shadow_dir(cwd: &Path) -> PathBuf {
    let slug = cwd
        .to_string_lossy()
        .replace(['/', '\\', ':', ' '], "_")
        .trim_start_matches('_')
        .to_string();
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home)
        .join(".fast-rlm-agent")
        .join("snapshots")
        .join(slug)
}

// ---- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("fra-snap-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_snapshotter(worktree: &Path) -> Snapshotter {
        // Use a separate temp dir for the shadow repo so the worktree stays clean.
        let shadow_parent = std::env::temp_dir().join(format!("fra-shadow-{}", std::process::id()));
        let slug = worktree
            .to_string_lossy()
            .replace(['/', '\\', ':', ' '], "_")
            .trim_start_matches('_')
            .to_string();
        let dir = shadow_parent.join(slug);
        let s = Snapshotter {
            dir,
            worktree: worktree
                .canonicalize()
                .unwrap_or_else(|_| worktree.to_path_buf()),
            checkpoints: Vec::new(),
        };
        s.init().expect("init failed");
        s
    }

    #[test]
    fn capture_and_restore_modified_file() {
        let wt = TempDir::new("modify");
        fs::write(wt.path().join("hello.txt"), "before\n").unwrap();

        let mut snap = make_snapshotter(wt.path());
        assert!(snap.capture(0, 1));

        fs::write(wt.path().join("hello.txt"), "after\n").unwrap();

        let cp = snap.restore().expect("restore returned None");
        assert_eq!(cp.items_len, 0);
        assert_eq!(cp.history_len, 1);

        let content = fs::read_to_string(wt.path().join("hello.txt")).unwrap();
        assert_eq!(content, "before\n");
    }

    #[test]
    fn restore_deletes_files_added_after_snapshot() {
        let wt = TempDir::new("delete");
        fs::write(wt.path().join("existing.txt"), "keep\n").unwrap();

        let mut snap = make_snapshotter(wt.path());
        assert!(snap.capture(0, 1));

        // Add a new file after the snapshot.
        fs::write(wt.path().join("new.txt"), "should disappear\n").unwrap();

        snap.restore().expect("restore failed");

        assert!(
            !wt.path().join("new.txt").exists(),
            "new.txt should be deleted on restore"
        );
        assert!(wt.path().join("existing.txt").exists());
        let content = fs::read_to_string(wt.path().join("existing.txt")).unwrap();
        assert_eq!(content, "keep\n");
    }

    #[test]
    fn restore_recreates_deleted_files() {
        let wt = TempDir::new("recreate");
        fs::write(wt.path().join("file.txt"), "was here\n").unwrap();

        let mut snap = make_snapshotter(wt.path());
        assert!(snap.capture(0, 1));

        fs::remove_file(wt.path().join("file.txt")).unwrap();

        snap.restore().expect("restore failed");

        let content = fs::read_to_string(wt.path().join("file.txt")).unwrap();
        assert_eq!(content, "was here\n");
    }

    #[test]
    fn can_undo_tracks_checkpoint_stack() {
        let wt = TempDir::new("stack");
        let mut snap = make_snapshotter(wt.path());

        assert!(!snap.can_undo());
        snap.capture(5, 2);
        assert!(snap.can_undo());
        snap.restore();
        assert!(!snap.can_undo());
    }

    #[test]
    fn multiple_captures_undo_in_order() {
        let wt = TempDir::new("multi");
        fs::write(wt.path().join("v.txt"), "v1\n").unwrap();

        let mut snap = make_snapshotter(wt.path());
        snap.capture(0, 1); // checkpoint A: v1

        fs::write(wt.path().join("v.txt"), "v2\n").unwrap();
        snap.capture(2, 3); // checkpoint B: v2

        fs::write(wt.path().join("v.txt"), "v3\n").unwrap();

        // First undo restores v2.
        snap.restore().unwrap();
        assert_eq!(fs::read_to_string(wt.path().join("v.txt")).unwrap(), "v2\n");

        // Second undo restores v1.
        snap.restore().unwrap();
        assert_eq!(fs::read_to_string(wt.path().join("v.txt")).unwrap(), "v1\n");

        assert!(!snap.can_undo());
    }

    #[test]
    fn shadow_dir_is_deterministic() {
        let p = Path::new("/home/user/myproject");
        assert_eq!(shadow_dir(p), shadow_dir(p));
        assert_ne!(shadow_dir(p), shadow_dir(Path::new("/home/user/other")));
    }
}
