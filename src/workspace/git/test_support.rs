use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};

static JJ_IDENTITY: Once = Once::new();

/// Ensure jj has an author identity for non-interactive use.
///
/// Set in the process environment so jj subprocesses spawned by *production*
/// code under test (e.g. `jj workspace add`) inherit it too, not just the jj
/// calls made directly by these helpers. The values are constant, so concurrent
/// tests setting them is harmless.
pub(crate) fn ensure_jj_identity() {
    JJ_IDENTITY.call_once(|| {
        std::env::set_var("JJ_USER", "Herdr Test");
        std::env::set_var("JJ_EMAIL", "herdr@example.invalid");
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
