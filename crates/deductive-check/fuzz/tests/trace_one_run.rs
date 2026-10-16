//! Writes a trace of one state-machine run (see `trace_one_run`).

use deductive_check_fuzz::run_state_machine;

/// Pick the largest corpus input (richest execution) for a representative trace.
fn pick_corpus_input() -> Option<Vec<u8>> {
    let dir = std::path::PathBuf::from("corpus/state_machine");
    let mut entries: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|e| Some((e.metadata().ok()?.len(), e.path())))
        .collect();
    entries.sort_by_key(|(len, _)| *len);
    entries.last().map(|(_, p)| std::fs::read(p).ok())?
}

/// Runs one state-machine execution with a tracing subscriber writing the
/// full `deductive_check` event flow to a file. Run with:
///   cargo +nightly test trace_one_run -- --nocapture
///
/// Its own test binary: the subscriber is process-wide (fan-out children run
/// on their own threads), so other tests' runs would land in the trace.
#[test]
fn trace_one_run() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let trace_path = std::path::PathBuf::from(
        std::env::var("FUZZ_TRACE_PATH")
            .unwrap_or_else(|_| "/tmp/opencode/fuzz_trace.log".to_string()),
    );
    if let Some(parent) = trace_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = std::fs::File::create(&trace_path).expect("create trace file");

    tracing_subscriber::registry()
        .with(EnvFilter::new(
            "deductive_check=info,deductive_check_fuzz=info,refactor_check_core=warn,warn",
        ))
        .with(fmt::layer().with_writer(file).with_ansi(false).with_target(false))
        .try_init()
        .expect("trace_one_run needs the process-wide tracing subscriber");

    let data = match std::env::var("FUZZ_INPUT_PATH") {
        Ok(p) => std::fs::read(&p)
            .unwrap_or_else(|e| panic!("FUZZ_INPUT_PATH read failed ({p}): {e}")),
        Err(_) => pick_corpus_input().unwrap_or_else(|| vec![123u8; 200]),
    };
    eprintln!("tracing input of {} bytes -> {}", data.len(), trace_path.display());
    run_state_machine(&data);
    eprintln!("trace written to {}", trace_path.display());
}
