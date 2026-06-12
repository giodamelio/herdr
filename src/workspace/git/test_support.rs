use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};

static JJ_IDENTITY: Once = Once::new();

/// Ensure jj has an author identity for non-interactive use, and that it reads
/// no personal configuration.
///
/// Set in the process environment so jj subprocesses spawned by *production*
/// code under test (e.g. `jj workspace add`) inherit it too, not just the jj
/// calls made directly by these helpers. The values are constant, so concurrent
/// tests setting them is harmless.
///
/// `JJ_CONFIG` points at an empty file so the developer's own settings cannot
/// reach these repos. Without it a personal `immutable_heads()` revset makes
/// freshly created test commits immutable and every `jj describe` fails.
pub(crate) fn ensure_jj_identity() {
    JJ_IDENTITY.call_once(|| {
        std::env::set_var("JJ_USER", "Herdr Test");
        std::env::set_var("JJ_EMAIL", "herdr@example.invalid");

        let config =
            std::env::temp_dir().join(format!("herdr-jj-test-config-{}.toml", std::process::id()));
        if std::fs::write(&config, "").is_ok() {
            std::env::set_var("JJ_CONFIG", &config);
        }
    });
}

pub(crate) fn temp_test_dir(name: &str) -> PathBuf {
    let unique = format!(
        "herdr-workspace-tests-{}-{}-{}",
        name,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let path = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn init_repo_with_commit(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    run_git(repo, &["init", "--quiet"]);
    run_git(repo, &["config", "user.email", "herdr@example.invalid"]);
    run_git(repo, &["config", "user.name", "Herdr Test"]);
    run_git(
        repo,
        &["commit", "--quiet", "--allow-empty", "-m", "initial"],
    );
}

pub(crate) fn create_repo_with_linked_worktree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = temp_test_dir(name);
    let repo = base.join("herdr");
    let checkout = base.join("testr56");
    init_repo_with_commit(&repo);
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "testr56",
            checkout.to_str().unwrap(),
            "HEAD",
        ],
    );
    (base, repo, checkout)
}

pub(crate) fn create_bare_repo_with_linked_worktree(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = temp_test_dir(name);
    let seed = base.join("seed");
    let bare = base.join(".bare");
    let checkout = base.join("feature");
    init_repo_with_commit(&seed);
    run_git(
        &base,
        &[
            "clone",
            "--quiet",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    run_git(
        &bare,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature",
            checkout.to_str().unwrap(),
            "HEAD",
        ],
    );
    (base, bare, checkout)
}

pub(super) fn write_fake_tracked_repo(root: &Path) {
    let head_oid = "1111111111111111111111111111111111111111";
    let upstream_oid = "2222222222222222222222222222222222222222";
    std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
    std::fs::create_dir_all(root.join(".git/refs/remotes/origin")).unwrap();
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(root.join(".git/refs/heads/main"), format!("{head_oid}\n")).unwrap();
    std::fs::write(
        root.join(".git/refs/remotes/origin/main"),
        format!("{upstream_oid}\n"),
    )
    .unwrap();
    std::fs::write(
        root.join(".git/config"),
        "[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n",
    )
    .unwrap();
}

pub(super) fn run_git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Initialise a colocated jj repo at `root` with a single `init` commit.
pub(crate) fn init_colocated_repo(root: &Path) {
    ensure_jj_identity();
    let output = Command::new("jj")
        .args(["git", "init", "--colocate"])
        .arg(root)
        .output()
        .expect("failed to spawn jj");
    assert!(
        output.status.success(),
        "jj git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(root.join("README.md"), "init\n").unwrap();
    jj(root, &["describe", "-m", "init"]);
}

/// Run a jj command rooted at `dir`, asserting success and returning trimmed stdout.
pub(crate) fn jj(dir: &Path, args: &[&str]) -> String {
    ensure_jj_identity();
    let output = Command::new("jj")
        .arg("-R")
        .arg(dir)
        .args(args)
        .output()
        .expect("failed to spawn jj");
    assert!(
        output.status.success(),
        "jj {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
