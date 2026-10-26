//! Real-subprocess tests for the opencode agent provider: API-key passing
//! and fail-fast (ungated) requests for preflight.
#![allow(unknown_lints)] // item-level inline_literal allows below

#![allow(clippy::unwrap_used, clippy::panic, unused_results)]

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{mpsc, Arc};

use deductive_check::provider::{AgentRequest, CliAgentProvider};
use servyi_ioprovider::IOProvider;

/// Shell used as a stand-in agent binary and its script flag.
const SHELL_BIN: &str = "/bin/sh";
const SHELL_FLAG_C: &str = "-c";
/// Environment variable the API key is passed through.
const API_KEY_ENV: &str = "OPENROUTER_API_KEY";
/// Fake API keys injected before and after the live-config update.
const TEST_API_KEY: &str = "sk-test-42";
const UPDATED_API_KEY: &str = "sk-fixed";
/// File the shell script writes the key to.
const KEYFILE_NAME: &str = "key.txt";
/// Agent binaries that fail: exists-but-fails, and cannot-exist.
const FAILING_BIN: &str = "/bin/false";
const MISSING_BIN: &str = "/nonexistent-agent-binary";
/// Prompt for requests whose content is irrelevant.
const PLAIN_PROMPT: &str = "Say OK";
/// Shell script whose stdout is a JSON error event; the quotes are part of
/// the fixture under test.
#[allow(inline_literal)]
const ERROR_EVENT_SCRIPT: &str =
    r#"echo '{"type":"text","part":{"type":"text","text":"Invalid API key."}}'; exit 1"#;
/// Marker file whose existence tells the script the user fixed the cause.
const FIXED_MARKER_FILE: &str = "fixed.marker";
const MARKER_CONTENT: &str = "fixed";
/// Epoch value after the user's RETRY bump.
const RETRY_EPOCH: u64 = 1;
/// Gate-park polling: iteration cap and sleep between polls.
const GATE_POLL_MAX: u32 = 200;
const GATE_POLL_INTERVAL_MS: u64 = 10;
/// Failures that must be parked in the gate.
const EXPECTED_PARKED: usize = 1;

fn agent_request(prompt: &str, dir: &std::path::Path, recoverable: bool) -> AgentRequest {
    AgentRequest {
        prompt: prompt.to_string(),
        working_directory: dir.to_path_buf(),
        files_to_read: vec![],
        recoverable,
    }
}

#[test]
fn passes_the_api_key_to_the_agent_binary_via_env() {
    // /bin/sh -c "<script> --format json --dir ..." — the script writes the
    // child's env var to a file we then inspect (the provider extracts
    // opencode-style JSON events from stdout, so plain echo is filtered).
    let dir = tempfile::tempdir().unwrap();
    let keyfile = dir.path().join(KEYFILE_NAME);
    let provider = CliAgentProvider::new(SHELL_BIN.to_string(), vec![SHELL_FLAG_C.to_string()])
        .with_api_key(API_KEY_ENV, TEST_API_KEY.to_string());

    let script = format!("printenv OPENROUTER_API_KEY > {}", keyfile.display());
    let resp = provider
        .invoke(agent_request(&script, dir.path(), true))
        
        .unwrap();
    assert!(resp.success, "shell ran: {}", resp.stdout);

    let seen = std::fs::read_to_string(&keyfile).unwrap_or_default();
    assert_eq!(
        seen.trim(),
        TEST_API_KEY,
        "the key must reach the agent binary's environment"
    );
}

#[test]
fn unrecoverable_requests_fail_fast_without_the_error_gate() {
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
    let failing = CliAgentProvider::new(FAILING_BIN.to_string(), vec![])
        .with_error_gate(gate.clone());
    let resp = failing
        .invoke(agent_request(PLAIN_PROMPT, dir.path(), false))
        ;
    assert!(resp.is_err(), "unrecoverable failure must be an Err, got {resp:?}");

    // Binary missing entirely: same.
    let missing = CliAgentProvider::new(MISSING_BIN.to_string(), vec![])
        .with_error_gate(gate);
    let resp = missing
        .invoke(agent_request(PLAIN_PROMPT, dir.path(), false))
        ;
    assert!(resp.is_err(), "unrecoverable spawn failure must be an Err, got {resp:?}");
}

#[test]
fn nonzero_exit_with_error_events_is_a_recoverable_gate_stop() {
    // The exact production failure: opencode exits nonzero AND prints an
    // error event ("Invalid API key"). This must park in the error gate
    // (recoverable) — or be an Err — never silently Ok(success=false).
    let dir = tempfile::tempdir().unwrap();
    let provider = CliAgentProvider::new(SHELL_BIN.to_string(), vec![SHELL_FLAG_C.to_string()]);

    let script = ERROR_EVENT_SCRIPT;
    let resp = provider
        .invoke(agent_request(script, dir.path(), true))
        ;
    assert!(
        resp.is_err(),
        "nonzero exit with error text must be an error without a gate, got {resp:?}"
    );
}

#[test]
fn invalid_key_failure_parks_in_gate_and_resumes_after_fix() {
    use std::sync::atomic::Ordering;

    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join(FIXED_MARKER_FILE);
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
        CliAgentProvider::new(SHELL_BIN.to_string(), vec![SHELL_FLAG_C.to_string()])
            .with_error_gate(gate),
    );

    let p = provider.clone();
    // The invoke runs on a worker thread; it parks in the gate on failure.
    let handle = std::thread::spawn(move || p.invoke(agent_request(&script, dir.path(), true)));

    // The failure parks in the gate ...
    for _ in 0..GATE_POLL_MAX {
        if !parked.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(GATE_POLL_INTERVAL_MS));
    }
    assert_eq!(parked.lock().unwrap().len(), EXPECTED_PARKED, "failure must park in the gate");

    // User fixes the cause and hits RETRY (epoch bump): the retried call
    // succeeds.
    std::fs::write(&marker, MARKER_CONTENT).unwrap();
    epoch.store(RETRY_EPOCH, Ordering::SeqCst);
    let resp = handle.join().unwrap().unwrap();
    assert!(resp.success, "retry after the fix must succeed: {}", resp.stdout);
    assert!(parked.lock().unwrap().is_empty(), "gate must clear on resume");
}

#[test]
fn live_api_key_is_reread_on_every_call() {
    use refactor_check_core::live_config::LiveConfig;
    use refactor_check_core::config_update::AppConfig;

    let dir = tempfile::tempdir().unwrap();
    let keyfile = dir.path().join(KEYFILE_NAME);
    let config = Arc::new(LiveConfig::new(AppConfig::default()));
    let provider = Arc::new(
        CliAgentProvider::new(SHELL_BIN.to_string(), vec![SHELL_FLAG_C.to_string()])
            .with_live_api_key(API_KEY_ENV, config.clone()),
    );

    let script = format!("printenv OPENROUTER_API_KEY > {}", keyfile.display());
    let probe = |script: String| {
        let provider = provider.clone();
        let dir = dir.path().to_path_buf();
        {
            provider
                .invoke(agent_request(&script, &dir, true))
                
                .unwrap();
        }
    };
    probe(script.clone());
    assert_eq!(std::fs::read_to_string(&keyfile).unwrap().trim(), "");

    // The user fixes the key via the shell's live config; the next call
    // must pick it up WITHOUT restarting.
    config.update(|cfg| cfg.llm.api_key = UPDATED_API_KEY.to_string());
    probe(script);
    assert_eq!(
        std::fs::read_to_string(&keyfile).unwrap().trim(),
        UPDATED_API_KEY,
        "retry must use the updated key"
    );
}

