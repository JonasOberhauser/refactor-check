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

#[tokio::test]
async fn nonzero_exit_with_error_events_is_a_recoverable_gate_stop() {
    // The exact production failure: opencode exits nonzero AND prints an
    // error event ("Invalid API key"). This must park in the error gate
    // (recoverable) — or be an Err — never silently Ok(success=false).
    let dir = tempfile::tempdir().unwrap();
    let provider = CliAgentProvider::new("/bin/sh".to_string(), vec!["-c".to_string()]);

    let script = r#"echo '{"type":"text","part":{"type":"text","text":"Invalid API key."}}'; exit 1"#;
    let resp = provider
        .invoke(agent_request(script, dir.path(), true))
        .await;
    assert!(
        resp.is_err(),
        "nonzero exit with error text must be an error without a gate, got {resp:?}"
    );
}

#[tokio::test]
async fn invalid_key_failure_parks_in_gate_and_resumes_after_fix() {
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("fixed.marker");
    let parked: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let epoch = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (tx, _rx) = mpsc::channel();
    let gate = Arc::new(refactor_check_core::error_gate::ErrorGate::new(
        epoch.clone(),
        shutdown.clone(),
        tx,
        parked.clone(),
    ));

    // Fails with "Invalid API key." until the marker exists, then succeeds.
    let script = format!(
        r#"if [ -f '{}' ]; then echo recovered; else echo '{{"type":"text","part":{{"type":"text","text":"Invalid API key."}}}}'; exit 1; fi"#,
        marker.display()
    );
    let provider = Arc::new(
        CliAgentProvider::new("/bin/sh".to_string(), vec!["-c".to_string()])
            .with_error_gate(gate),
    );

    let p = provider.clone();
    let handle = tokio::spawn(async move { p.invoke(agent_request(&script, dir.path(), true)).await });

    // The failure parks in the gate ...
    for _ in 0..200 {
        if !parked.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(parked.lock().unwrap().len(), 1, "failure must park in the gate");

    // User fixes the cause and hits RETRY (epoch bump): the retried call
    // succeeds.
    std::fs::write(&marker, "fixed").unwrap();
    epoch.store(1, Ordering::SeqCst);
    let resp = handle.await.unwrap().unwrap();
    assert!(resp.success, "retry after the fix must succeed: {}", resp.stdout);
    assert!(parked.lock().unwrap().is_empty(), "gate must clear on resume");
}

#[tokio::test]
async fn live_api_key_is_reread_on_every_call() {
    use refactor_check_core::live_config::LiveConfig;
    use refactor_check_core::config_update::AppConfig;

    let dir = tempfile::tempdir().unwrap();
    let keyfile = dir.path().join("key.txt");
    let config = Arc::new(LiveConfig::new(AppConfig::default()));
    let provider = Arc::new(
        CliAgentProvider::new("/bin/sh".to_string(), vec!["-c".to_string()])
            .with_live_api_key("OPENROUTER_API_KEY", config.clone()),
    );

    let script = format!("printenv OPENROUTER_API_KEY > {}", keyfile.display());
    let probe = |script: String| {
        let provider = provider.clone();
        let dir = dir.path().to_path_buf();
        async move {
            provider
                .invoke(agent_request(&script, &dir, true))
                .await
                .unwrap();
        }
    };
    tokio_test_probe(probe, script.clone()).await;
    assert_eq!(std::fs::read_to_string(&keyfile).unwrap().trim(), "");

    // The user fixes the key via the shell's live config; the next call
    // must pick it up WITHOUT restarting.
    config.update(|cfg| cfg.llm.api_key = "sk-fixed".to_string());
    tokio_test_probe(probe, script).await;
    assert_eq!(
        std::fs::read_to_string(&keyfile).unwrap().trim(),
        "sk-fixed",
        "retry must use the updated key"
    );
}

async fn tokio_test_probe<F, Fut>(probe: F, script: String)
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    probe(script).await;
}
