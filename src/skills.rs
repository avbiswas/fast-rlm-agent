//! On-demand discovery and reading of agent instructions and skills.

use std::path::{Path, PathBuf};

use crate::workspace;

const MAX_DOCUMENT_BYTES: u64 = 64 * 1024;
const MAX_DOCUMENTS: usize = 200;
const DOCUMENT_NAMES: [&str; 4] = ["AGENTS.md", "AGENTS.override.md", "CLAUDE.md", "SKILL.md"];
const SKIPPED_DIRS: [&str; 5] = [".git", ".jj", ".venv", "node_modules", "target"];

pub fn list(workspace_root: &Path) -> Result<String, String> {
    let root = workspace_root
        .canonicalize()
        .map_err(|error| format!("cannot access workspace: {error}"))?;
    let mut documents = Vec::new();
    collect(&root, &root, &mut documents)?;
    documents.sort();
    documents.truncate(MAX_DOCUMENTS);

    if documents.is_empty() {
        return Ok(
            "No AGENTS.md, CLAUDE.md, or SKILL.md files were found in the workspace.".to_string(),
        );
    }

    let mut output = String::from("Available instruction and skill documents:\n");
    for document in documents {
        output.push_str("- ");
        output.push_str(&document);
        output.push('\n');
    }
    output.push_str("Call skill again with one of these paths to read it.");
    Ok(output)
}

pub fn read(workspace_root: &Path, requested: &str) -> Result<String, String> {
    let path = workspace::resolve_existing(workspace_root, requested)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if !DOCUMENT_NAMES.contains(&name) {
        return Err(
            "skill can only read AGENTS.md, AGENTS.override.md, CLAUDE.md, or SKILL.md files"
                .to_string(),
        );
    }
    let metadata =
        std::fs::metadata(&path).map_err(|error| format!("cannot inspect {requested}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("{requested} is not a file"));
    }
    if metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "{requested} is too large; instruction documents are limited to {MAX_DOCUMENT_BYTES} bytes"
        ));
    }
    std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {requested} as UTF-8: {error}"))
}

fn collect(root: &Path, directory: &Path, documents: &mut Vec<String>) -> Result<(), String> {
    if documents.len() >= MAX_DOCUMENTS {
        return Ok(());
    }
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
    let mut entries = entries
        .filter_map(Result::ok)
        .collect::<Vec<std::fs::DirEntry>>();
    entries.sort_by_key(|entry| entry.file_name());

    // Record documents in this directory before descending, so a root
    // AGENTS.md cannot be displaced by a very large nested skill collection.
    for entry in &entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_file() && DOCUMENT_NAMES.contains(&name.as_ref()) {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            documents.push(display_path(relative));
        }
        if documents.len() >= MAX_DOCUMENTS {
            return Ok(());
        }
    }

    for entry in entries {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() && !SKIPPED_DIRS.contains(&name.as_ref()) {
            // An unreadable nested directory should not hide usable skills
            // elsewhere in the workspace.
            let _ = collect(root, &path, documents);
        }
        if documents.len() >= MAX_DOCUMENTS {
            return Ok(());
        }
    }
    Ok(())
}

fn display_path(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("fra-skills-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_and_reads_instruction_and_skill_documents() {
        let root = TempDir::new("list");
        std::fs::write(root.0.join("AGENTS.md"), "# Instructions\n").unwrap();
        std::fs::create_dir_all(root.0.join(".agents/skills/testing")).unwrap();
        std::fs::write(
            root.0.join(".agents/skills/testing/SKILL.md"),
            "# Testing\n",
        )
        .unwrap();

        let available = list(&root.0).unwrap();
        assert!(available.contains("AGENTS.md"));
        assert!(available.contains(".agents/skills/testing/SKILL.md"));
        assert_eq!(read(&root.0, "AGENTS.md").unwrap(), "# Instructions\n");
    }

    #[test]
    fn ignores_build_directories_and_rejects_arbitrary_files() {
        let root = TempDir::new("guards");
        std::fs::create_dir_all(root.0.join("target/generated")).unwrap();
        std::fs::write(root.0.join("target/generated/SKILL.md"), "hidden").unwrap();
        std::fs::write(root.0.join("README.md"), "not a skill").unwrap();

        assert!(!list(&root.0).unwrap().contains("target/generated"));
        assert!(read(&root.0, "README.md").is_err());
    }
}
