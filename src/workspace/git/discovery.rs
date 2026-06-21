use std::path::{Path, PathBuf};
use std::process::Command;

/// VCS-derived workspace metadata.
///
/// Herdr is built on Jujutsu (jj) workspaces. The field names keep the `git`
/// vocabulary used across the codebase, but the values are derived from the jj
/// repository: every herdr "worktree" is a jj workspace, backed in colocated
/// repos by a shared `.jj/repo` store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSpaceMetadata {
    /// Stable identity shared by every workspace of the same repo: the resolved
    /// `.jj/repo` store directory.
    pub key: String,
    /// Identity of this specific checkout: the canonical workspace root.
    pub checkout_key: String,
    /// Display label for the repo (the default workspace directory name).
    pub label: String,
    /// Root of this workspace checkout.
    pub repo_root: PathBuf,
    /// True when this is an added jj workspace (not the default workspace).
    pub is_linked_worktree: bool,
}

pub fn derive_label_from_cwd(cwd: &Path) -> String {
    if let Some(metadata) = git_space_metadata(cwd) {
        return metadata.label;
    }

    if let Ok(home) = std::env::var("HOME") {
        let home = Path::new(&home);
        if cwd == home {
            return "~".to_string();
        }
    }

    cwd.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| cwd.display().to_string())
}

/// Run a read-only jj command rooted at `dir`.
///
/// `--ignore-working-copy` keeps these queries side-effect free (no snapshot,
/// safe while another process holds the working copy) and `--color=never`
/// guards against a user `ui.color = "always"` setting leaking escape codes.
pub(super) fn jj_read(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("jj")
        .arg("--ignore-working-copy")
        .arg("--color=never")
        .arg("-R")
        .arg(dir)
        .args(args)
        .output()
        .ok()
}

pub(super) fn jj_read_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let output = jj_read(dir, args)?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let stdout = stdout.trim();
    (!stdout.is_empty()).then(|| stdout.to_string())
}

pub(super) fn canonicalize_best_effort_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Walk up from `start` to the nearest ancestor that is a jj workspace root
/// (contains a `.jj` entry).
pub(super) fn jj_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };

    loop {
        if current.join(".jj").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Resolve the shared store directory (`<default-workspace>/.jj/repo`) for a
/// workspace root.
///
/// In the default workspace `.jj/repo` is a directory; in an added workspace it
/// is a file containing a path (relative to `.jj/`) to the default workspace's
/// store.
pub(super) fn jj_store_dir(workspace_root: &Path) -> Option<PathBuf> {
    let repo = workspace_root.join(".jj").join("repo");
    let meta = std::fs::symlink_metadata(&repo).ok()?;
    if meta.is_dir() {
        return Some(canonicalize_best_effort_path(&repo));
    }

    let target = std::fs::read_to_string(&repo).ok()?;
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    let target_path = Path::new(target);
    let resolved = if target_path.is_absolute() {
        target_path.to_path_buf()
    } else {
        workspace_root.join(".jj").join(target_path)
    };
    Some(canonicalize_best_effort_path(&resolved))
}

/// True when the workspace at `workspace_root` is an added jj workspace.
pub(super) fn jj_is_added_workspace(workspace_root: &Path) -> bool {
    std::fs::symlink_metadata(workspace_root.join(".jj").join("repo"))
        .map(|meta| !meta.is_dir())
        .unwrap_or(false)
}

pub fn git_space_metadata(cwd: &Path) -> Option<GitSpaceMetadata> {
    let root = jj_workspace_root(cwd)?;
    let store = jj_store_dir(&root)?;
    let key = store.display().to_string();
    let checkout_key = canonicalize_best_effort_path(&root).display().to_string();
    let is_linked_worktree = jj_is_added_workspace(&root);

    // `store` is `<default-workspace>/.jj/repo`; the repo label is the default
    // workspace directory name.
    let label = store
        .parent()
        .and_then(Path::parent)
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_string();

    Some(GitSpaceMetadata {
        key,
        checkout_key,
        label,
        repo_root: root,
        is_linked_worktree,
    })
}

/// The bookmark associated with this workspace's working copy: the nearest
/// ancestor bookmark of `@`. Returns `None` when no bookmark is reachable
/// (analogous to a detached checkout).
pub fn git_branch(cwd: &Path) -> Option<String> {
    let root = jj_workspace_root(cwd)?;
    let output = jj_read_stdout(
        &root,
        &[
            "log",
            "-r",
            "heads(::@ & bookmarks())",
            "--no-graph",
            "-T",
            "local_bookmarks.map(|b| b.name()).join(\"\\n\")",
        ],
    )?;
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::git::test_support::{init_colocated_repo, jj, temp_test_dir};

    #[test]
    fn git_branch_reads_bookmark_at_working_copy() {
        let root = temp_test_dir("branch-bookmark");
        init_colocated_repo(&root);
        jj(&root, &["bookmark", "create", "main", "-r", "@"]);

        assert_eq!(git_branch(&root).as_deref(), Some("main"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_branch_returns_none_without_bookmark() {
        let root = temp_test_dir("branch-none");
        init_colocated_repo(&root);

        assert_eq!(git_branch(&root), None);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_branch_reads_nearest_ancestor_bookmark_from_subdir() {
        let root = temp_test_dir("branch-ancestor");
        init_colocated_repo(&root);
        jj(&root, &["bookmark", "create", "main", "-r", "@"]);
        jj(&root, &["new"]);
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(git_branch(&nested).as_deref(), Some("main"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_space_metadata_ignores_empty_jj_marker() {
        let base = temp_test_dir("invalid-jj-root");
        let cwd = base.join("workspace");
        // A `.jj` directory without a resolvable store must not register as a repo.
        std::fs::create_dir_all(base.join(".jj")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        assert_eq!(git_space_metadata(&cwd), None);

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn git_space_metadata_identifies_default_workspace() {
        let root = temp_test_dir("space-default");
        init_colocated_repo(&root);
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        let metadata =
            git_space_metadata(&nested).expect("default workspace should map to a space");
        assert!(!metadata.is_linked_worktree);
        assert_eq!(
            canonicalize_best_effort_path(&metadata.repo_root),
            canonicalize_best_effort_path(&root)
        );
        assert_eq!(
            metadata.label,
            root.file_name().and_then(|name| name.to_str()).unwrap()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_space_metadata_groups_added_workspace_with_default() {
        let base = temp_test_dir("space-added");
        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        init_colocated_repo(&root);
        let added = base.join("added");
        jj(
            &root,
            &[
                "workspace",
                "add",
                "--name",
                "added",
                added.to_str().unwrap(),
            ],
        );

        let default_space = git_space_metadata(&root).unwrap();
        let added_space = git_space_metadata(&added).unwrap();

        // Same repo identity (shared store), different checkouts.
        assert_eq!(default_space.key, added_space.key);
        assert_ne!(default_space.checkout_key, added_space.checkout_key);
        assert!(!default_space.is_linked_worktree);
        assert!(added_space.is_linked_worktree);
        assert_eq!(default_space.label, added_space.label);
        assert_eq!(added_space.label, "repo");

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn derive_label_prefers_repo_name() {
        let base = temp_test_dir("label-repo-base");
        let root = base.join("named-repo");
        std::fs::create_dir_all(&root).unwrap();
        init_colocated_repo(&root);
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(derive_label_from_cwd(&nested), "named-repo");

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn derive_label_uses_path_name_outside_repo() {
        let root = temp_test_dir("label-plain");
        let label = root.file_name().and_then(|name| name.to_str()).unwrap();

        assert_eq!(derive_label_from_cwd(Path::new(&root)), label);

        std::fs::remove_dir_all(root).unwrap();
    }
}
