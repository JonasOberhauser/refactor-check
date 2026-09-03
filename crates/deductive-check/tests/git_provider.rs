//! Real-git tests for the git provider's repo detection and the
//! init/add flow the Initializer relies on.

use async_trait::async_trait;
use deductive_check::provider::{CliGitProvider, GitRequest, GitResponse};
use servyi_ioprovider::IOProvider;

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[tokio::test]
async fn add_all_tolerates_unmatched_globs() {
    // A fresh repo with a .rs file but no .toml/.lock anywhere: the old
    // literal `git add '*.rs' '*.toml' '*.lock'` died on the unmatched
    // pathspec ('fatal: pathspec '*.lock' did not match any files').
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init"]);
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    std::fs::write(dir.path().join("src/nested/lib.rs"), "pub fn f() {}").unwrap();

    let provider = CliGitProvider::new(dir.path().to_path_buf());
    let resp: GitResponse = provider
        .invoke(GitRequest::AddAll { path: dir.path().to_path_buf() })
        .await
        .unwrap();
    assert!(resp.success, "AddAll must not fail on unmatched globs: {}", resp.output);

    // Everything matching was staged — at any depth.
    let staged = git(dir.path(), &["diff", "--cached", "--name-only"]);
    assert!(staged.contains("src/main.rs"), "staged: {staged}");
    assert!(staged.contains("src/nested/lib.rs"), "staged: {staged}");
    assert_eq!(staged.lines().count(), 2, "only matching files: {staged}");
}

#[tokio::test]
async fn add_all_with_no_matches_is_a_noop_success() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init"]);
    std::fs::write(dir.path().join("notes.md"), "hi").unwrap();

    let provider = CliGitProvider::new(dir.path().to_path_buf());
    let resp: GitResponse = provider
        .invoke(GitRequest::AddAll { path: dir.path().to_path_buf() })
        .await
        .unwrap();
    assert!(resp.success, "AddAll with nothing to add must succeed: {}", resp.output);
}

#[tokio::test]
async fn repo_root_detects_enclosing_worktree_from_subdirectory() {
    // A subcrate (nested dir) of an existing repo must be recognized as
    // inside the work tree — the Initializer must not `git init` a new
    // nested repo there.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init"]);
    let sub = dir.path().join("crates").join("subcrate");
    std::fs::create_dir_all(&sub).unwrap();

    let provider = CliGitProvider::new(sub.clone());
    let resp: GitResponse = provider.invoke(GitRequest::RepoRoot).await.unwrap();
    assert!(resp.success, "inside a work tree: {}", resp.output);
    let expected = std::fs::canonicalize(dir.path()).unwrap();
    let reported = std::fs::canonicalize(resp.output.trim()).unwrap();
    assert_eq!(reported, expected, "RepoRoot must report the enclosing toplevel");
}

#[tokio::test]
async fn repo_root_fails_outside_any_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let provider = CliGitProvider::new(dir.path().to_path_buf());
    let resp: GitResponse = provider.invoke(GitRequest::RepoRoot).await.unwrap();
    assert!(!resp.success, "no work tree here: {}", resp.output);
}
