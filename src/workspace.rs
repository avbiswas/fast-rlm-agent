//! Workspace-root path validation for model-requested filesystem operations.

use std::path::{Component, Path, PathBuf};

/// Resolve an existing path and ensure its canonical target remains inside the
/// canonical workspace root. Canonicalization makes symlink escapes visible.
pub fn resolve_existing(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let candidate = candidate(root, requested)?;
    let resolved = candidate
        .canonicalize()
        .map_err(|e| format!("cannot access {requested}: {e}"))?;
    ensure_inside(root, &resolved, requested)?;
    Ok(resolved)
}

/// Resolve a path that may not exist yet. Its closest existing ancestor is
/// canonicalized so a symlinked parent cannot redirect a write outside the
/// workspace.
pub fn resolve_for_write(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let candidate = candidate(root, requested)?;

    if candidate.exists() {
        return resolve_existing(root, requested);
    }

    let mut ancestor = candidate.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| outside_error(requested, root))?;
    }
    let resolved_ancestor = ancestor
        .canonicalize()
        .map_err(|e| format!("cannot access parent of {requested}: {e}"))?;
    ensure_inside(root, &resolved_ancestor, requested)?;
    Ok(candidate)
}

fn candidate(root: &Path, requested: &str) -> Result<PathBuf, String> {
    if requested.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }

    let path = Path::new(requested);
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!(
            "path escapes workspace: {requested} contains a parent-directory component"
        ));
    }

    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("cannot access workspace {}: {e}", root.display()))?;
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        canonical_root.join(path)
    };
    Ok(candidate)
}

fn ensure_inside(root: &Path, path: &Path, requested: &str) -> Result<(), String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("cannot access workspace {}: {e}", root.display()))?;
    if path.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(outside_error(requested, &canonical_root))
    }
}

fn outside_error(requested: &str, root: &Path) -> String {
    format!(
        "path escapes workspace: {requested} is outside {}",
        root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("fra-workspace-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_existing_relative_and_absolute_paths_inside_root() {
        let root = TempDir::new("inside");
        fs::write(root.0.join("file.txt"), "ok").unwrap();

        assert_eq!(
            resolve_existing(&root.0, "file.txt").unwrap(),
            root.0.join("file.txt")
        );
        assert_eq!(
            resolve_existing(&root.0, root.0.join("file.txt").to_str().unwrap()).unwrap(),
            root.0.join("file.txt")
        );
    }

    #[test]
    fn allows_a_new_file_under_existing_or_new_directories() {
        let root = TempDir::new("new-file");
        assert_eq!(
            resolve_for_write(&root.0, "new/nested/file.txt").unwrap(),
            root.0.join("new/nested/file.txt")
        );
    }

    #[test]
    fn rejects_parent_traversal_and_absolute_paths_outside_root() {
        let root = TempDir::new("escapes");
        let outside = root.0.parent().unwrap().join("outside.txt");

        assert!(resolve_for_write(&root.0, "../outside.txt").is_err());
        assert!(resolve_for_write(&root.0, outside.to_str().unwrap()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_existing_files_and_new_files_through_outside_symlinks() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new("symlink-root");
        let outside = TempDir::new("symlink-outside");
        fs::write(outside.0.join("secret.txt"), "secret").unwrap();
        symlink(&outside.0, root.0.join("escape")).unwrap();

        assert!(resolve_existing(&root.0, "escape/secret.txt").is_err());
        assert!(resolve_for_write(&root.0, "escape/new.txt").is_err());
    }
}
