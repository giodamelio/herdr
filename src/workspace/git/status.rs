use std::path::{Path, PathBuf};

use crate::workspace::WorkspaceGitStatusSnapshot;

use super::discovery::{
    canonicalize_best_effort_path, git_branch, git_space_metadata, jj_read, jj_read_stdout,
    jj_workspace_root,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusCacheEntry {
    pub fingerprint: JjStatusFingerprint,
    pub snapshot: WorkspaceGitStatusSnapshot,
}

/// Cheap identity used to decide whether cached ahead/behind counts are still
/// valid. Recomputed counts are only needed when the working-copy commit, the
/// bookmark, or its tracked remote target moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JjStatusFingerprint {
    pub workspace_root: PathBuf,
    pub branch: Option<String>,
    /// Commit the local bookmark points at. Ahead/behind is measured from the
    /// bookmark (not `@`, which is an empty commit on top of it), so the cache
    /// must invalidate when the bookmark moves.
    pub branch_commit: Option<String>,
    pub remote: Option<String>,
    pub upstream_commit: Option<String>,
}

pub fn git_status_cache_key(cwd: &Path) -> Option<PathBuf> {
    jj_workspace_root(cwd).map(|root| canonicalize_best_effort_path(&root))
}

pub fn git_status_snapshot_for_cwd(
    cwd: &Path,
    cached: Option<&GitStatusCacheEntry>,
) -> (WorkspaceGitStatusSnapshot, Option<GitStatusCacheEntry>) {
    let space = git_space_metadata(cwd);
    let Some(root) = jj_workspace_root(cwd) else {
        return (
            WorkspaceGitStatusSnapshot {
                branch: git_branch(cwd),
                ahead_behind: None,
                space,
            },
            None,
        );
    };

    let branch = git_branch(&root);
    let fingerprint = JjStatusFingerprint::compute(&root, branch.clone());

    if let Some(cached) = cached.filter(|entry| entry.fingerprint == fingerprint) {
        let snapshot = WorkspaceGitStatusSnapshot {
            branch,
            ahead_behind: cached.snapshot.ahead_behind,
            space,
        };
        return (
            snapshot.clone(),
            Some(GitStatusCacheEntry {
                fingerprint,
                snapshot,
            }),
        );
    }

    let ahead_behind = fingerprint.compute_ahead_behind(&root);
    let snapshot = WorkspaceGitStatusSnapshot {
        branch,
        ahead_behind,
        space,
    };
    (
        snapshot.clone(),
        Some(GitStatusCacheEntry {
            fingerprint,
            snapshot,
        }),
    )
}

impl JjStatusFingerprint {
    fn compute(root: &Path, branch: Option<String>) -> Self {
        let workspace_root = canonicalize_best_effort_path(root);
        let branch_commit = branch
            .as_deref()
            .and_then(|branch| jj_local_bookmark_commit(root, branch));
        let (remote, upstream_commit) = match branch.as_deref() {
            Some(branch) => match jj_tracked_remote(root, branch) {
                Some(remote) => {
                    let target = jj_remote_target(root, branch, &remote);
                    (Some(remote), target)
                }
                None => (None, None),
            },
            None => (None, None),
        };
        JjStatusFingerprint {
            workspace_root,
            branch,
            branch_commit,
            remote,
            upstream_commit,
        }
    }

    fn compute_ahead_behind(&self, root: &Path) -> Option<(usize, usize)> {
        let branch = self.branch.as_deref()?;
        let remote = self.remote.as_deref()?;
        // Only meaningful when the tracked remote actually points somewhere.
        self.upstream_commit.as_ref()?;
        jj_ahead_behind(root, branch, remote)
    }
}

fn jj_local_bookmark_commit(root: &Path, branch: &str) -> Option<String> {
    jj_read_stdout(
        root,
        &["log", "-r", branch, "--no-graph", "-T", "commit_id"],
    )
}

/// The remote that the local bookmark tracks. Prefers `origin`; ignores the
/// colocated `git` backing remote.
fn jj_tracked_remote(root: &Path, branch: &str) -> Option<String> {
    let output = jj_read_stdout(
        root,
        &[
            "bookmark",
            "list",
            "--all-remotes",
            branch,
            "-T",
            "if(remote && tracked && remote != \"git\", remote ++ \"\\n\", \"\")",
        ],
    )?;
    let remotes: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    remotes
        .iter()
        .find(|remote| **remote == "origin")
        .or_else(|| remotes.first())
        .map(|remote| remote.to_string())
}

fn jj_remote_target(root: &Path, branch: &str, remote: &str) -> Option<String> {
    jj_read_stdout(
        root,
        &[
            "log",
            "-r",
            &format!("{branch}@{remote}"),
            "--no-graph",
            "-T",
            "commit_id",
        ],
    )
}

fn jj_ahead_behind(root: &Path, branch: &str, remote: &str) -> Option<(usize, usize)> {
    // Measure the local bookmark against its remote (not `@`, which is an empty
    // working-copy commit on top of the bookmark and would always read +1).
    let remote_ref = format!("{branch}@{remote}");
    let ahead = jj_count_revset(root, &format!("{remote_ref}..{branch}"))?;
    let behind = jj_count_revset(root, &format!("{branch}..{remote_ref}"))?;
    Some((ahead, behind))
}

fn jj_count_revset(root: &Path, revset: &str) -> Option<usize> {
    let output = jj_read(root, &["log", "-r", revset, "--no-graph", "-T", "\"x\\n\""])?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
    )
}

#[cfg(test)]
pub(crate) fn git_ahead_behind(cwd: &Path) -> Option<(usize, usize)> {
    let root = jj_workspace_root(cwd)?;
    let branch = git_branch(&root)?;
    let remote = jj_tracked_remote(&root, &branch)?;
    jj_ahead_behind(&root, &branch, &remote)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::git::test_support::{
        ensure_jj_identity, init_colocated_repo, jj, temp_test_dir,
    };
    use std::process::Command;

    /// Clone `src` into `dest` as a colocated repo and put `@` on a local `main`
    /// bookmark that tracks `origin` (mirrors a normal branch-tracks-remote
    /// checkout; the bare colocated source has no default HEAD for clone to
    /// auto-track).
    fn clone_tracking_main(src: &Path, dest: &Path) {
        ensure_jj_identity();
        let output = Command::new("jj")
            .args(["git", "clone", "--colocate"])
            .arg(src)
            .arg(dest)
            .output()
            .expect("failed to spawn jj");
        assert!(
            output.status.success(),
            "jj git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        jj(dest, &["bookmark", "track", "main", "--remote", "origin"]);
        jj(dest, &["new", "main"]);
    }

    #[test]
    fn cache_key_is_per_workspace_checkout() {
        let base = temp_test_dir("cache-key-per-ws");
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

        assert_ne!(git_status_cache_key(&root), git_status_cache_key(&added));

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn snapshot_reports_bookmark_without_remote() {
        let root = temp_test_dir("snapshot-branch");
        init_colocated_repo(&root);
        jj(&root, &["bookmark", "create", "main", "-r", "@"]);

        let (snapshot, entry) = git_status_snapshot_for_cwd(&root, None);
        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.ahead_behind, None);
        assert!(entry.is_some());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ahead_behind_tracks_remote_and_recomputes_when_head_moves() {
        let base = temp_test_dir("ahead-behind");
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        init_colocated_repo(&src);
        jj(&src, &["bookmark", "create", "main", "-r", "@"]);
        let clone = base.join("clone");
        clone_tracking_main(&src, &clone);

        let (initial, cache_entry) = git_status_snapshot_for_cwd(&clone, None);
        assert_eq!(initial.branch.as_deref(), Some("main"));
        assert_eq!(initial.ahead_behind, Some((0, 0)));

        // Advance the local bookmark one commit ahead of origin.
        jj(&clone, &["new", "main"]);
        std::fs::write(clone.join("change.txt"), "x\n").unwrap();
        jj(&clone, &["describe", "-m", "ahead"]);
        jj(
            &clone,
            &["bookmark", "set", "main", "-r", "@", "--allow-backwards"],
        );

        let (updated, _) = git_status_snapshot_for_cwd(&clone, cache_entry.as_ref());
        assert_eq!(updated.branch.as_deref(), Some("main"));
        assert_eq!(updated.ahead_behind, Some((1, 0)));

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn cache_reused_when_fingerprint_matches() {
        let base = temp_test_dir("cache-reuse");
        let src = base.join("src");
        std::fs::create_dir_all(&src).unwrap();
        init_colocated_repo(&src);
        jj(&src, &["bookmark", "create", "main", "-r", "@"]);
        let clone = base.join("clone");
        clone_tracking_main(&src, &clone);

        let (_, entry) = git_status_snapshot_for_cwd(&clone, None);
        let entry = entry.unwrap();
        // Inject a sentinel so we can tell a reused snapshot from a recomputed one.
        let mut cached = entry.clone();
        cached.snapshot.ahead_behind = Some((7, 7));

        let (snapshot, _) = git_status_snapshot_for_cwd(&clone, Some(&cached));
        assert_eq!(snapshot.ahead_behind, Some((7, 7)));

        std::fs::remove_dir_all(base).unwrap();
    }
}
