use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_WORKTREE_PREFIX: &str = "worktree";

/// A single jj invocation (program + args) used to manage worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// A jj workspace discovered for a repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExistingWorktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// jj workspaces are never bare; kept for API/display parity.
    pub is_bare: bool,
    /// True when the workspace has no associated bookmark.
    pub is_detached: bool,
    /// True when the workspace's checkout directory no longer exists on disk.
    pub is_prunable: bool,
    /// jj workspace name (the handle used by `jj workspace add/forget`).
    pub workspace_name: String,
}

pub(crate) fn generated_branch_slug(seed: u64) -> String {
    let adjectives = [
        "brave", "calm", "clear", "green", "lucky", "quiet", "rapid", "silver",
    ];
    let nouns = [
        "river", "cloud", "field", "forest", "harbor", "meadow", "stone", "valley",
    ];
    let adjective = adjectives[(seed as usize) % adjectives.len()];
    let noun = nouns[((seed / adjectives.len() as u64) as usize) % nouns.len()];
    let suffix = seed & 0xffff;
    format!("{DEFAULT_WORKTREE_PREFIX}/{adjective}-{noun}-{suffix:04x}")
}

/// Derive a jj workspace name from a bookmark name.
///
/// Bookmark names may contain `/` (e.g. `worktree/brave-river-0000`); jj
/// workspace names may not, so the path-safe slug doubles as the workspace
/// handle (`worktree-brave-river-0000`).
pub(crate) fn workspace_name_for_branch(branch: &str) -> String {
    branch_to_path_slug(branch)
}

pub(crate) fn branch_to_path_slug(branch: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in branch.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        DEFAULT_WORKTREE_PREFIX.to_string()
    } else {
        trimmed
    }
}

pub(crate) fn expand_tilde_path(path: &str) -> PathBuf {
    expand_tilde_path_from_env(path, cfg!(windows), |key| std::env::var_os(key))
}

fn expand_tilde_path_from_env(
    path: &str,
    is_windows: bool,
    env: impl Fn(&str) -> Option<OsString> + Copy,
) -> PathBuf {
    if path == "~" {
        return home_dir_from_env(is_windows, env).unwrap_or_else(|_| PathBuf::from(path));
    }

    let tilde_rest = path.strip_prefix("~/").or_else(|| {
        if is_windows {
            path.strip_prefix("~\\")
        } else {
            None
        }
    });
    if let Some(rest) = tilde_rest {
        return home_dir_from_env(is_windows, env)
            .map(|home| join_tilde_rest(home, rest, is_windows))
            .unwrap_or_else(|_| PathBuf::from(path));
    }

    PathBuf::from(path)
}

fn join_tilde_rest(home: PathBuf, rest: &str, is_windows: bool) -> PathBuf {
    if is_windows {
        rest.split(['/', '\\'])
            .filter(|component| !component.is_empty())
            .fold(home, |path, component| path.join(component))
    } else {
        home.join(rest)
    }
}

fn home_dir_from_env(
    is_windows: bool,
    env: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, ()> {
    if !is_windows {
        return env("HOME").map(PathBuf::from).ok_or(());
    }

    if let Some(path) = usable_home_path(env("USERPROFILE")) {
        return Ok(path);
    }
    if let (Some(drive), Some(path)) = (
        usable_home_component(env("HOMEDRIVE")),
        usable_home_component(env("HOMEPATH")),
    ) {
        let path = path.to_string_lossy();
        if !path.starts_with(['\\', '/']) {
            return usable_home_path(env("HOME")).ok_or(());
        }
        let combined = format!("{}{}", drive.to_string_lossy(), path);
        if let Some(path) = usable_home_path(Some(OsString::from(combined))) {
            return Ok(path);
        }
    }

    usable_home_path(env("HOME")).ok_or(())
}

fn usable_home_path(value: Option<OsString>) -> Option<PathBuf> {
    let value = value?;
    if value.is_empty() || value == "~" {
        return None;
    }
    Some(PathBuf::from(value))
}

fn usable_home_component(value: Option<OsString>) -> Option<OsString> {
    let value = value?;
    if value.is_empty() || value == "~" {
        return None;
    }
    Some(value)
}

pub(crate) fn expand_tilde_absolute_path(path: &str) -> PathBuf {
    let path = expand_tilde_path(path);
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

pub(crate) fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn default_checkout_path(root: &Path, repo_name: &str, branch: &str) -> PathBuf {
    root.join(repo_name).join(branch_to_path_slug(branch))
}

/// Commands to create a new jj workspace at `path`, based on `base`, with a new
/// bookmark `branch` pointing at the new working copy.
///
/// `base` is the revision the new workspace starts from; the git-era sentinel
/// `"HEAD"` maps to the source workspace's `@`.
pub(crate) fn build_worktree_add_commands(
    source_checkout: &Path,
    path: &Path,
    branch: &str,
    base: &str,
) -> Vec<WorktreeCommand> {
    let workspace_name = workspace_name_for_branch(branch);
    let base_rev = if base == "HEAD" { "@" } else { base };
    vec![
        WorktreeCommand {
            program: "jj".to_string(),
            args: vec![
                "-R".to_string(),
                source_checkout.display().to_string(),
                "workspace".to_string(),
                "add".to_string(),
                "--name".to_string(),
                workspace_name,
                "-r".to_string(),
                base_rev.to_string(),
                path.display().to_string(),
            ],
        },
        WorktreeCommand {
            program: "jj".to_string(),
            args: vec![
                "-R".to_string(),
                path.display().to_string(),
                "bookmark".to_string(),
                "create".to_string(),
                branch.to_string(),
                "-r".to_string(),
                "@".to_string(),
            ],
        },
    ]
}

/// Command to stop tracking a jj workspace. This leaves the checkout directory
/// and the workspace's bookmark in place; the caller deletes the directory.
pub(crate) fn build_worktree_forget_command(
    repo_root: &Path,
    workspace_name: &str,
) -> WorktreeCommand {
    WorktreeCommand {
        program: "jj".to_string(),
        args: vec![
            "-R".to_string(),
            repo_root.display().to_string(),
            "workspace".to_string(),
            "forget".to_string(),
            workspace_name.to_string(),
        ],
    }
}

/// Forget a jj workspace and remove its checkout directory.
///
/// `jj workspace forget` only drops tracking; the bookmark is preserved so the
/// branch survives, matching the git-era "remove worktree, keep branch"
/// behaviour. The directory is deleted separately.
pub(crate) fn remove_worktree_checkout(
    repo_root: &Path,
    workspace_name: &str,
    path: &Path,
) -> Result<(), String> {
    run_worktree_command(&build_worktree_forget_command(repo_root, workspace_name))?;
    if path.exists() {
        std::fs::remove_dir_all(path).map_err(|err| err.to_string())?;
    }
    Ok(())
}

pub(crate) fn run_worktree_command(command: &WorktreeCommand) -> Result<(), String> {
    let output = Command::new(&command.program)
        .args(&command.args)
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(if message.is_empty() {
        format!("{} failed with status {}", command.program, output.status)
    } else {
        message
    })
}

pub(crate) fn run_worktree_commands(commands: &[WorktreeCommand]) -> Result<(), String> {
    for command in commands {
        run_worktree_command(command)?;
    }
    Ok(())
}

/// Whether the workspace at `path` has uncommitted changes.
///
/// jj auto-snapshots the working copy, so `@` is empty exactly when there are no
/// changes (tracked or untracked) relative to its parent. This intentionally
/// snapshots (no `--ignore-working-copy`).
pub(crate) fn checkout_has_dirty_files(path: &Path) -> Result<bool, String> {
    let output = Command::new("jj")
        .arg("--color=never")
        .arg("-R")
        .arg(path)
        .args([
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "if(empty, \"clean\", \"dirty\")",
        ])
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(stdout.trim() == "dirty");
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        Err(stderr)
    } else if !stdout.is_empty() {
        Err(stdout)
    } else {
        Err(format!("jj status failed with status {}", output.status))
    }
}

/// Run a read-only jj command rooted at `repo_root`, capturing stdout.
fn jj_capture(repo_root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("jj")
        .arg("--ignore-working-copy")
        .arg("--color=never")
        .arg("-R")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("jj {} failed with status {}", args.join(" "), output.status)
    } else {
        stderr
    })
}

pub(crate) fn list_existing_worktrees(repo_root: &Path) -> Result<Vec<ExistingWorktree>, String> {
    // Pull name + working-copy commit together so the branch can be resolved
    // from the repo even when a workspace's checkout directory is gone.
    let listing = jj_capture(
        repo_root,
        &[
            "workspace",
            "list",
            "-T",
            "name ++ \"\\t\" ++ target.commit_id() ++ \"\\n\"",
        ],
    )?;

    let mut entries = Vec::new();
    for line in listing.lines() {
        let Some((name, target)) = line.split_once('\t') else {
            continue;
        };
        let name = name.trim();
        let target = target.trim();
        if name.is_empty() {
            continue;
        }
        // jj can't resolve the path of a workspace whose checkout was deleted
        // outside herdr. That's an edge case (herdr removes via forget + rmdir
        // together), so skip such a workspace rather than fail the whole list.
        let Ok(path_output) = jj_capture(repo_root, &["workspace", "root", "--name", name]) else {
            continue;
        };
        let path = PathBuf::from(path_output.trim());
        let branch = jj_bookmark_at_commit(repo_root, target);
        entries.push(ExistingWorktree {
            is_detached: branch.is_none(),
            is_prunable: !path.exists(),
            branch,
            path,
            is_bare: false,
            workspace_name: name.to_string(),
        });
    }
    Ok(entries)
}

/// The bookmark associated with a workspace's working-copy commit, queried from
/// the repo store (works even when the checkout directory is gone).
fn jj_bookmark_at_commit(repo_root: &Path, commit: &str) -> Option<String> {
    if commit.is_empty() {
        return None;
    }
    let revset = format!("heads(::{commit} & bookmarks())");
    let output = jj_capture(
        repo_root,
        &[
            "log",
            "-r",
            &revset,
            "--no-graph",
            "-T",
            "local_bookmarks.map(|b| b.name()).join(\"\\n\")",
        ],
    )
    .ok()?;
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
    fn generated_branch_slug_is_worktree_namespaced_and_stable() {
        assert_eq!(generated_branch_slug(0), "worktree/brave-river-0000");
        assert_eq!(generated_branch_slug(9), "worktree/calm-cloud-0009");
    }

    #[test]
    fn workspace_name_strips_slash_from_branch() {
        assert_eq!(
            workspace_name_for_branch("worktree/brave-river-0000"),
            "worktree-brave-river-0000"
        );
        assert_eq!(workspace_name_for_branch("issue/137"), "issue-137");
    }

    #[test]
    fn branch_to_path_slug_makes_branch_safe_folder_name() {
        assert_eq!(
            branch_to_path_slug("worktree/brave-river"),
            "worktree-brave-river"
        );
        assert_eq!(
            branch_to_path_slug("issue/137 Worktree Spaces"),
            "issue-137-worktree-spaces"
        );
        assert_eq!(branch_to_path_slug("///"), "worktree");
    }

    #[test]
    fn add_commands_create_workspace_and_bookmark() {
        let commands = build_worktree_add_commands(
            Path::new("/repo/herdr"),
            Path::new("/w/herdr/worktree-brave-river"),
            "worktree/brave-river",
            "HEAD",
        );
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].program, "jj");
        assert_eq!(
            commands[0].args,
            vec![
                "-R",
                "/repo/herdr",
                "workspace",
                "add",
                "--name",
                "worktree-brave-river",
                "-r",
                "@",
                "/w/herdr/worktree-brave-river",
            ]
        );
        assert_eq!(
            commands[1].args,
            vec![
                "-R",
                "/w/herdr/worktree-brave-river",
                "bookmark",
                "create",
                "worktree/brave-river",
                "-r",
                "@",
            ]
        );
    }

    #[test]
    fn forget_command_targets_workspace_by_name() {
        let command =
            build_worktree_forget_command(Path::new("/repo/herdr"), "worktree-brave-river");
        assert_eq!(command.program, "jj");
        assert_eq!(
            command.args,
            vec![
                "-R",
                "/repo/herdr",
                "workspace",
                "forget",
                "worktree-brave-river",
            ]
        );
    }

    #[test]
    fn default_checkout_path_appends_repo_and_branch_slug() {
        assert_eq!(
            default_checkout_path(
                Path::new("/home/me/.herdr/worktrees"),
                "herdr",
                "worktree/brave-river",
            ),
            PathBuf::from("/home/me/.herdr/worktrees/herdr/worktree-brave-river")
        );
    }

    #[test]
    fn list_reports_default_and_added_workspaces() {
        let base = temp_test_dir("worktree-list");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_colocated_repo(&repo);
        jj(&repo, &["bookmark", "create", "main", "-r", "@"]);
        let added = base.join("added");
        jj(
            &repo,
            &[
                "workspace",
                "add",
                "--name",
                "worktree-added",
                added.to_str().unwrap(),
            ],
        );
        jj(&added, &["bookmark", "create", "worktree/added", "-r", "@"]);

        let entries = list_existing_worktrees(&repo).unwrap();
        let default = entries
            .iter()
            .find(|entry| entry.workspace_name == "default")
            .expect("default workspace listed");
        assert_eq!(default.branch.as_deref(), Some("main"));
        assert!(!default.is_prunable);

        let added_entry = entries
            .iter()
            .find(|entry| entry.workspace_name == "worktree-added")
            .expect("added workspace listed");
        assert_eq!(added_entry.branch.as_deref(), Some("worktree/added"));
        assert_eq!(
            canonical_or_original(&added_entry.path),
            canonical_or_original(&added)
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn add_then_remove_creates_and_deletes_checkout_keeping_bookmark() {
        let base = temp_test_dir("worktree-add-remove");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_colocated_repo(&repo);
        let checkout = base.join("checkout");
        let branch = "worktree/test-create-remove";

        let add = build_worktree_add_commands(&repo, &checkout, branch, "HEAD");
        run_worktree_commands(&add).unwrap();

        assert!(checkout.join("README.md").exists());
        assert_eq!(
            crate::workspace::git_branch(&checkout).as_deref(),
            Some(branch)
        );

        remove_worktree_checkout(&repo, &workspace_name_for_branch(branch), &checkout).unwrap();
        assert!(!checkout.exists());
        // Bookmark survives the removal.
        let bookmarks = jj(&repo, &["bookmark", "list", "-T", "name ++ \"\\n\""]);
        assert!(bookmarks.lines().any(|line| line.trim() == branch));

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn checkout_dirty_detection_reports_clean_and_dirty_workspaces() {
        let base = temp_test_dir("worktree-dirty");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_colocated_repo(&repo);
        let checkout = base.join("checkout");
        let add = build_worktree_add_commands(&repo, &checkout, "worktree/dirty", "HEAD");
        run_worktree_commands(&add).unwrap();

        assert_eq!(checkout_has_dirty_files(&checkout), Ok(false));
        std::fs::write(checkout.join("DIRTY.md"), "dirty\n").unwrap();
        assert_eq!(checkout_has_dirty_files(&checkout), Ok(true));

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn expand_tilde_path_uses_home_when_available() {
        assert_eq!(
            expand_tilde_path_from_env("~/.herdr/worktrees", false, |key| match key {
                "HOME" => Some("/home/me".into()),
                _ => None,
            }),
            PathBuf::from("/home/me/.herdr/worktrees")
        );
        assert_eq!(
            expand_tilde_path_from_env("/tmp/worktrees", false, |_| None),
            PathBuf::from("/tmp/worktrees")
        );
    }

    #[test]
    fn home_dir_uses_windows_profile_before_literal_home() {
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOME" => Some("~".into()),
                "USERPROFILE" => Some(r"C:\Users\herdr".into()),
                _ => None,
            }),
            Ok(PathBuf::from(r"C:\Users\herdr"))
        );
    }

    #[test]
    fn home_dir_uses_windows_drive_and_path_when_profile_is_missing() {
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOMEDRIVE" => Some("C:".into()),
                "HOMEPATH" => Some(r"\Users\herdr".into()),
                _ => None,
            }),
            Ok(PathBuf::from(r"C:\Users\herdr"))
        );
    }

    #[test]
    fn home_dir_rejects_incomplete_windows_drive_and_path() {
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOMEDRIVE" => Some("C:".into()),
                "HOMEPATH" => Some("".into()),
                _ => None,
            }),
            Err(())
        );
        assert_eq!(
            home_dir_from_env(true, |key| match key {
                "HOMEDRIVE" => Some("C:".into()),
                "HOMEPATH" => Some("Users\\herdr".into()),
                _ => None,
            }),
            Err(())
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_tilde_expansion_keeps_windows_separator_literal() {
        assert_eq!(
            expand_tilde_path_from_env(r"~\.herdr\worktrees", false, |key| match key {
                "HOME" => Some("/home/me".into()),
                _ => None,
            }),
            PathBuf::from(r"~\.herdr\worktrees")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_tilde_expansion_normalizes_separators() {
        fn env(key: &str) -> Option<OsString> {
            match key {
                "HOME" => Some("~".into()),
                "USERPROFILE" => Some(r"C:\Users\herdr".into()),
                _ => None,
            }
        }

        let default_path = expand_tilde_path_from_env("~/.herdr/worktrees", true, env);
        assert_eq!(
            default_path,
            PathBuf::from(r"C:\Users\herdr\.herdr\worktrees")
        );
        assert_eq!(
            default_path.display().to_string(),
            r"C:\Users\herdr\.herdr\worktrees"
        );
        assert_eq!(
            expand_tilde_path_from_env(r"~\.herdr\worktrees", true, env),
            PathBuf::from(r"C:\Users\herdr\.herdr\worktrees")
        );
    }
}
