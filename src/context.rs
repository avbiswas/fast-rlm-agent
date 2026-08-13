//! Preprocess a user's free-form prompt into FastRLM-style structured context.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::workspace;

/// The stable input contract passed across the agent/backend boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptContext {
    pub prompt: String,
    pub links: Vec<String>,
    pub files: Vec<ContextFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFile {
    /// Workspace-relative path, using the platform's normal display form.
    pub path: String,
    /// UTF-8 file contents loaded before the model turn starts.
    pub content: String,
}

impl PromptContext {
    pub fn from_prompt(prompt: String, workspace_root: &Path) -> Self {
        Self {
            links: extract_links(&prompt),
            files: extract_files(&prompt, workspace_root),
            prompt,
        }
    }

    /// Chat Completions only accepts textual user content. Keeping this
    /// serialization at the backend seam lets a future FastRLM adapter pass the
    /// same value as a real dictionary without changing preprocessing.
    pub fn to_user_content(&self) -> String {
        serde_json::to_string(self).expect("PromptContext is always JSON serializable")
    }
}

fn extract_links(prompt: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut seen = HashSet::new();
    let mut offset = 0;

    while let Some(relative_start) = find_url_start(&prompt[offset..]) {
        let start = offset + relative_start;
        let rest = &prompt[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`'))
            .unwrap_or(rest.len());
        let link = rest[..end].trim_end_matches(|c: char| {
            matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
        });
        if !link.is_empty() && seen.insert(link.to_string()) {
            links.push(link.to_string());
        }
        offset = start + end.max(1);
    }

    links
}

fn find_url_start(value: &str) -> Option<usize> {
    [value.find("https://"), value.find("http://")]
        .into_iter()
        .flatten()
        .min()
}

fn extract_files(prompt: &str, workspace_root: &Path) -> Vec<ContextFile> {
    let canonical_root = match workspace_root.canonicalize() {
        Ok(root) => root,
        Err(_) => return Vec::new(),
    };
    let mut files = Vec::new();
    let mut seen = HashSet::new();

    for raw in file_candidates(prompt) {
        if raw.starts_with("http://") || raw.starts_with("https://") {
            continue;
        }
        let Ok(resolved) = workspace::resolve_existing(&canonical_root, &raw) else {
            continue;
        };
        if !resolved.is_file() || !seen.insert(resolved.clone()) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&resolved) else {
            continue;
        };
        let path = resolved
            .strip_prefix(&canonical_root)
            .unwrap_or(&resolved)
            .display()
            .to_string();
        files.push(ContextFile { path, content });
    }

    files
}

fn file_candidates(prompt: &str) -> Vec<String> {
    let mut candidates: Vec<String> = prompt
        .split_whitespace()
        .filter_map(clean_file_candidate)
        .collect();

    // Markdown file links such as [design](docs/design.md).
    let mut rest = prompt;
    while let Some(start) = rest.find("](") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find(')') else {
            break;
        };
        if let Some(candidate) = clean_file_candidate(&rest[..end]) {
            candidates.push(candidate);
        }
        rest = &rest[end + 1..];
    }

    candidates
}

fn clean_file_candidate(raw: &str) -> Option<String> {
    let candidate = raw
        .trim_matches(|c: char| {
            matches!(
                c,
                '@' | '`'
                    | '"'
                    | '\''
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '<'
                    | '>'
                    | ','
                    | ';'
                    | ':'
                    | '!'
                    | '?'
            )
        })
        .trim_end_matches('.');
    if candidate.is_empty() {
        None
    } else {
        Some(candidate.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("fra-context-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join("docs")).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn extracts_and_deduplicates_urls_in_first_seen_order() {
        let links = extract_links(
            "Compare https://example.com/a, then [B](http://example.org/b). Again: https://example.com/a",
        );
        assert_eq!(links, vec!["https://example.com/a", "http://example.org/b"]);
    }

    #[test]
    fn loads_referenced_workspace_files_with_relative_paths() {
        let root = TempDir::new("files");
        fs::write(root.0.join("a.txt"), "alpha").unwrap();
        fs::write(root.0.join("docs/b.md"), "bravo").unwrap();

        let context = PromptContext::from_prompt(
            "Use @a.txt, `docs/b.md`, and [the notes](docs/b.md).".to_string(),
            &root.0,
        );
        assert_eq!(
            context.files,
            vec![
                ContextFile {
                    path: "a.txt".to_string(),
                    content: "alpha".to_string(),
                },
                ContextFile {
                    path: "docs/b.md".to_string(),
                    content: "bravo".to_string(),
                },
            ]
        );
    }

    #[test]
    fn ignores_missing_binary_and_outside_files() {
        let root = TempDir::new("ignored");
        fs::write(root.0.join("binary.dat"), [0xff, 0xfe]).unwrap();
        let context = PromptContext::from_prompt(
            "Read missing.txt binary.dat and ../outside.txt".to_string(),
            &root.0,
        );
        assert!(context.files.is_empty());
    }

    #[test]
    fn serializes_the_fast_rlm_context_shape() {
        let root = TempDir::new("json");
        fs::write(root.0.join("notes.txt"), "hello").unwrap();
        let context = PromptContext::from_prompt(
            "Summarize notes.txt and https://example.com".to_string(),
            &root.0,
        );
        let value: serde_json::Value = serde_json::from_str(&context.to_user_content()).unwrap();
        assert_eq!(value["prompt"], context.prompt);
        assert_eq!(value["links"][0], "https://example.com");
        assert_eq!(value["files"][0]["path"], "notes.txt");
        assert_eq!(value["files"][0]["content"], "hello");
    }
}
