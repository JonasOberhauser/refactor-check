//! Real-subprocess tests for the opencode agent provider: API-key passing
//! and fail-fast (ungated) requests for preflight.

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{mpsc, Arc};

use deductive_check::provider::{AgentRequest, CliAgentProvider};
use servyi_ioprovider::IOProvider;

fn agent_request(prompt: &str, dir: &std::path::Path, recoverable: bool) -> AgentRequest {
    AgentRequest {
        prompt: prompt.to_string(),
        working_directory: dir.to_path_buf(),
        files_to_read: vec![],
        recoverable,
    }
}

#[tokio::test]
async fn passes_the_api_key_to_the_agent_binary_via_env() {
    // /bin/sh -c "<script> --format json --dir ..." — the script writes the
    // child's env var to a file we then inspect (the provider extracts
    // opencode-style JSON events from stdout, so plain echo is filtered).
    let dir = tempfile::tempdir().unwrap();
    let keyfile = dir.path().join("key.txt");
    let provider = CliAgentProvider::new(
        "/bin/sh".to_string(),
        vec!["-c".to_string()],
    )
    .with_api_key("OPENROUTER_API_KEY", "sk-test-42".to_string());

    let script = format!("printenv OPENROUTER_API_KEY > {}", keyfile.display());
    let resp = provider
        .invoke(agent_request(&script, dir.path(), true))
        .await
        .unwrap();
    assert!(resp.success, "shell ran: {}", resp.stdout);

    let seen = std::fs::read_to_string(&keyfile).unwrap_or_default();
    assert_eq!(
        seen.trim(),
        "sk-test-42",
        "the key must reach the agent binary's environment"
    );
}

#[tokio::test]
async fn unrecoverable_requests_fail_fast_without_the_error_gate() {
    let dir = tempfile::tempdir().unwrap();
    // A real error gate: entering report_and_wait would block this test
    // forever — recoverable=false must never reach it.
    let (tx, _rx) = mpsc::channel();
    let gate = Arc::new(refactor_check_core::error_gate::ErrorGate::new(
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicBool::new(false)),
        tx,
        Arc::new(std::sync::Mutex::new(Vec::new())),
    ));

    // Binary exists but fails: must come back as an Err (preflight bails),
    // not Ok(success=false) behind a blocked gate.
    let failing = CliAgentProvider::new("/bin/false".to_string(), vec![])
        .with_error_gate(gate.clone());
    let resp = failing
        .invoke(agent_request("Say OK", dir.path(), false))
        .await;
    assert!(resp.is_err(), "unrecoverable failure must be an Err, got {resp:?}");

    // Binary missing entirely: same.
    let missing = CliAgentProvider::new("/nonexistent-agent-binary".to_string(), vec![])
        .with_error_gate(gate);
    let resp = missing
        .invoke(agent_request("Say OK", dir.path(), false))
        .await;
    assert!(resp.is_err(), "unrecoverable spawn failure must be an Err, got {resp:?}");
}
