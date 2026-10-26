//! Real-git tests for the git provider's repo detection and the
//! init/add flow the Initializer relies on.
#![allow(clippy::unwrap_used, clippy::panic, unused_results)]

use deductive_check::provider::{CliGitProvider, GitRequest, GitResponse};
use servyi_ioprovider::IOProvider;

/// Git binary and the inspection subcommands used below.
const GIT_BIN: &str = "git";
const INIT_ARGS: &[&str] = &["init"];
const STAGED_FILES_ARGS: &[&str] = &["diff", "--cached", "--name-only"];
/// Files created for the AddAll flow and their contents.
const MAIN_RS_PATH: &str = "src/main.rs";
const MAIN_RS_CONTENT: &str = "fn main() {}";
const NESTED_SRC_DIR: &str = "src/nested";
const NESTED_LIB_RS_PATH: &str = "src/nested/lib.rs";
const LIB_RS_CONTENT: &str = "pub fn f() {}";
const NON_RUST_FILE: &str = "notes.md";
const NON_RUST_CONTENT: &str = "hi";
/// Files AddAll is expected to stage.
const EXPECTED_STAGED_COUNT: usize = 2;
/// Subcrate path inside an existing work tree.
const CRATES_DIR: &str = "crates";
const SUBCRATE_NAME: &str = "subcrate";

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new(GIT_BIN)
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

#[test]
fn add_all_tolerates_unmatched_globs() {
    // A fresh repo with a .rs file but no .toml/.lock anywhere: the old
    // literal `git add '*.rs' '*.toml' '*.lock'` died on the unmatched
    // pathspec ('fatal: pathspec '*.lock' did not match any files').
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), INIT_ARGS);
    std::fs::create_dir_all(dir.path().join(NESTED_SRC_DIR)).unwrap();
    std::fs::write(dir.path().join(MAIN_RS_PATH), MAIN_RS_CONTENT).unwrap();
    std::fs::write(dir.path().join(NESTED_LIB_RS_PATH), LIB_RS_CONTENT).unwrap();

    let provider = CliGitProvider::new(dir.path().to_path_buf());
    let resp: GitResponse = provider
        .invoke(GitRequest::AddAll { path: dir.path().to_path_buf() })
        
        .unwrap();
    assert!(resp.success, "AddAll must not fail on unmatched globs: {}", resp.output);

    // Everything matching was staged — at any depth.
    let staged = git(dir.path(), STAGED_FILES_ARGS);
    assert!(staged.contains(MAIN_RS_PATH), "staged: {staged}");
    assert!(staged.contains(NESTED_LIB_RS_PATH), "staged: {staged}");
    assert_eq!(staged.lines().count(), EXPECTED_STAGED_COUNT, "only matching files: {staged}");
}

#[test]
fn add_all_with_no_matches_is_a_noop_success() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), INIT_ARGS);
    std::fs::write(dir.path().join(NON_RUST_FILE), NON_RUST_CONTENT).unwrap();

    let provider = CliGitProvider::new(dir.path().to_path_buf());
    let resp: GitResponse = provider
        .invoke(GitRequest::AddAll { path: dir.path().to_path_buf() })
        
        .unwrap();
    assert!(resp.success, "AddAll with nothing to add must succeed: {}", resp.output);
}

#[test]
fn repo_root_detects_enclosing_worktree_from_subdirectory() {
    // A subcrate (nested dir) of an existing repo must be recognized as
    // inside the work tree — the Initializer must not `git init` a new
    // nested repo there.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), INIT_ARGS);
    let sub = dir.path().join(CRATES_DIR).join(SUBCRATE_NAME);
    std::fs::create_dir_all(&sub).unwrap();

    let provider = CliGitProvider::new(sub.clone());
    let resp: GitResponse = provider.invoke(GitRequest::RepoRoot).unwrap();
    assert!(resp.success, "inside a work tree: {}", resp.output);
    let expected = std::fs::canonicalize(dir.path()).unwrap();
    let reported = std::fs::canonicalize(resp.output.trim()).unwrap();
    assert_eq!(reported, expected, "RepoRoot must report the enclosing toplevel");
}

#[test]
fn repo_root_fails_outside_any_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let provider = CliGitProvider::new(dir.path().to_path_buf());
    let resp: GitResponse = provider.invoke(GitRequest::RepoRoot).unwrap();
    assert!(!resp.success, "no work tree here: {}", resp.output);
}
