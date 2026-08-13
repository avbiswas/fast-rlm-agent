//! Session persistence: every chat is saved locally as JSON and can be
//! reloaded with `/resume`.
//!
//! A session file carries both layers of state:
//!  * `history` — a lightweight copy of the visible user/assistant turns.
//!  * `items` — the rendered transcript, so the UI redisplays faithfully.
//!
//! FastRLM owns the authoritative model and REPL state in its separate
//! persistent session directory; both layers share the same session id.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::agent::Message;
use crate::app::{Item, ToolRun};
use crate::tools::ToolStatus;

#[derive(Serialize, Deserialize)]
pub struct SavedSession {
    pub id: String,
    /// First user message, truncated — what the picker shows.
    pub title: String,
    pub cwd: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub history: Vec<Message>,
    pub items: Vec<Item>,
}

/// One row in the `/resume` picker.
pub struct Summary {
    pub path: PathBuf,
    pub title: String,
    pub updated_at: u64,
    /// Number of user turns in the session.
    pub turns: usize,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn new_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{millis}-{}", std::process::id())
}

/// `~/.fast-rlm-agent/sessions`
pub fn default_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".fast-rlm-agent").join("sessions")
}

pub fn save_in(dir: &Path, session: &SavedSession) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", session.id));
    let json = serde_json::to_string_pretty(session)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // Write-then-rename so a crash mid-save can't corrupt an existing file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// All sessions in `dir`, newest first.
pub fn list_in(dir: &Path) -> Vec<Summary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sessions: Vec<Summary> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let session = read_session(&path).ok()?;
            Some(Summary {
                path,
                title: session.title,
                updated_at: session.updated_at,
                turns: session
                    .items
                    .iter()
                    .filter(|i| matches!(i, Item::User(_)))
                    .count(),
            })
        })
        .collect();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions
}

/// Load a session for resuming. Tool calls frozen mid-flight (the app was
/// quit or crashed during a turn) are normalized to a terminal state.
pub fn load(path: &Path) -> Result<SavedSession, String> {
    let mut session = read_session(path).map_err(|e| e.to_string())?;
    for item in &mut session.items {
        if let Item::Tool(ToolRun {
            status, summary, ..
        }) = item
        {
            if matches!(status, ToolStatus::Pending | ToolStatus::Running) {
                *status = ToolStatus::Failed;
                if summary.is_none() {
                    *summary = Some("interrupted".to_string());
                }
            }
        }
    }
    Ok(session)
}

fn read_session(path: &Path) -> std::io::Result<SavedSession> {
    let data = std::fs::read_to_string(path)?;
    serde_json::from_str(&data).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ToolBody;

    fn message(role: &str, content: &str) -> Message {
        serde_json::json!({ "role": role, "content": content })
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fra-sess-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_session(id: &str, updated_at: u64) -> SavedSession {
        SavedSession {
            id: id.to_string(),
            title: "build me a thing".to_string(),
            cwd: "/tmp/project".to_string(),
            created_at: 100,
            updated_at,
            history: vec![
                message("system", "sys"),
                message("user", "build me a thing"),
            ],
            items: vec![
                Item::User("build me a thing".to_string()),
                Item::Assistant("On it. **Done.**".to_string()),
                Item::Tool(ToolRun {
                    id: 0,
                    verb: "Edit".to_string(),
                    arg: "src/x.rs".to_string(),
                    body: ToolBody::Diff {
                        path: "src/x.rs".to_string(),
                        old: "a\n".to_string(),
                        new: "b\n".to_string(),
                    },
                    status: ToolStatus::Done,
                    summary: Some("Replaced 1 occurrence(s)".to_string()),
                }),
            ],
        }
    }

    #[test]
    fn save_list_load_round_trip() {
        let dir = temp_dir("roundtrip");
        let session = sample_session("s1", 200);
        let path = save_in(&dir, &session).unwrap();

        let listed = list_in(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "build me a thing");
        assert_eq!(listed[0].turns, 1);

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.id, "s1");
        // Both layers survive byte-faithfully.
        assert_eq!(
            serde_json::to_string(&loaded.history).unwrap(),
            serde_json::to_string(&session.history).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&loaded.items).unwrap(),
            serde_json::to_string(&session.items).unwrap()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_is_newest_first() {
        let dir = temp_dir("order");
        save_in(&dir, &sample_session("old", 100)).unwrap();
        save_in(&dir, &sample_session("new", 999)).unwrap();
        let listed = list_in(&dir);
        assert_eq!(listed.len(), 2);
        assert!(listed[0].path.to_str().unwrap().contains("new"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_normalizes_interrupted_tool_calls() {
        let dir = temp_dir("interrupted");
        let mut session = sample_session("s2", 50);
        if let Item::Tool(run) = &mut session.items[2] {
            run.status = ToolStatus::Pending;
            run.summary = None;
        }
        let path = save_in(&dir, &session).unwrap();
        let loaded = load(&path).unwrap();
        let Item::Tool(run) = &loaded.items[2] else {
            panic!("expected tool item");
        };
        assert_eq!(run.status, ToolStatus::Failed);
        assert_eq!(run.summary.as_deref(), Some("interrupted"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
