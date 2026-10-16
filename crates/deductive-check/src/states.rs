use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use refactor_check_core::context_id::ContextId;
use servyi_states::checkpoint::Handle;
use tracing::{debug, info, warn};

use crate::code_piece::{ArcCodePiece, FunctionId};
use crate::formula::FormulaSource;
use crate::phase::{CodePiecePhase, FormulaPhase};
use crate::prompts;
use crate::provider::{FileSystemRequest, GitRequest, PythonRequest, RustAnalyzerRequest, RustAnalyzerResponse, Providers, FunctionInfo, AgentRequest};
use crate::result::{BugReport, ClosedPiece, UnverifiedPiece, VerificationResult};
use refactor_check_core::provider::{LlmRequest, LlmRole, SolverRequest};
use refactor_check_core::smt::SolverOutcome;

struct PieceContext {
    called_functions: String,
    docs_section: String,
}

fn solver_outcome_to_formula_phase(outcome: &SolverOutcome) -> FormulaPhase {
    match outcome {
        SolverOutcome::Unsat => FormulaPhase::ClosedUnsat,
        SolverOutcome::Sat => FormulaPhase::ClosedSat,
        SolverOutcome::Unknown => FormulaPhase::ClosedUnknown,
        SolverOutcome::Error(_) => FormulaPhase::Fix,
    }
}

fn unverified_from_piece(
    piece: &crate::code_piece::DeductiveCodePiece,
    sat_models: Vec<String>,
    unknown_formulas: Vec<String>,
    elaboration: String,
) -> UnverifiedPiece {
    UnverifiedPiece {
        file: piece.file().clone(),
        function_id: piece.function_id().clone(),
        start_line: piece.start_line(),
        end_line: piece.end_line(),
        sat_models,
        unknown_formulas,
        elaboration,
    }
}



// ── The machine state ───────────────────────────────────────────────────────
//
// The machine's state is the checkpoint pair's node type — `Handle<MachineState>`
// IS the running machine. Every transition reads its data through the handle
// and installs the next state through the handle. Two disciplines keep the
// state checkpointable at all times:
//
//   1. IO runs with data borrowed (or cheaply cloned) from the state; big
//      work (pieces, function bodies) lives in the state's slot vectors and
//      fans out directly off them — it never leaves the tree.
//   2. A transition computes the next state from `&mut` of the current one
//      and installs it in one window that contains no safepoint, so a save
//      can never observe an emptied intermediate.

/// One piece verification: a fan-out child. The piece and its ctx live in
/// the state for the child's whole life — the child accesses them through
/// its handle; they are never moved out. `outcome` is written once, at
/// completion.
pub struct PieceSlot {
    pub piece: ArcCodePiece,
    pub ctx: ContextId,
    pub outcome: Option<Result<PieceOutcome>>,
}

/// One splitter invocation: a fan-out child. Same rule: data lives in the
/// state, accessed through the handle.
pub struct SplitSlot {
    pub file: PathBuf,
    pub fid: FunctionId,
    pub body: String,
    pub outcome: Option<Result<Vec<(ArcCodePiece, ContextId)>>>,
}

/// The machine state: which transition runs next, plus its data.
pub enum MachineState {
    PreflightGit { project_path: PathBuf },
    PreflightSolver { project_path: PathBuf },
    PreflightPython { project_path: PathBuf },
    PreflightAgent { project_path: PathBuf },
    PreflightProject { project_path: PathBuf },
    Initializing { project_path: PathBuf },
    ListingFunctions {
        project_path: PathBuf,
        verification_dir: PathBuf,
        branch_name: String,
        files: Vec<PathBuf>,
        accumulated_bugs: Vec<BugReport>,
    },
    AnalyzingFunctions {
        project_path: PathBuf,
        verification_dir: PathBuf,
        branch_name: String,
        functions: HashMap<PathBuf, Vec<FunctionId>>,
        function_docs: HashMap<FunctionId, String>,
        accumulated_bugs: Vec<BugReport>,
    },
    Splitting {
        project_path: PathBuf,
        verification_dir: PathBuf,
        branch_name: String,
        function_bodies: Vec<SplitSlot>,
        function_docs: HashMap<FunctionId, String>,
        accumulated_bugs: Vec<BugReport>,
    },
    Formalizing {
        project_path: PathBuf,
        verification_dir: PathBuf,
        branch_name: String,
        pieces: Vec<PieceSlot>,
        accumulated_bugs: Vec<BugReport>,
    },
    AnalyzingProblems {
        project_path: PathBuf,
        verification_dir: PathBuf,
        branch_name: String,
        closed: Vec<ClosedPiece>,
        unverified: Vec<UnverifiedPiece>,
        recheck_files: Vec<PathBuf>,
        base_commit: String,
        accumulated_bugs: Vec<BugReport>,
        total_pieces: usize,
        total_functions: usize,
    },
    Restarting {
        project_path: PathBuf,
        verification_dir: PathBuf,
        branch_name: String,
        base_commit: String,
        recheck_files: Vec<PathBuf>,
        accumulated_bugs: Vec<BugReport>,
    },
}

impl MachineState {
    /// A fresh machine starting at the git preflight.
    #[must_use]
    pub fn start(project_path: PathBuf) -> Self {
        Self::PreflightGit { project_path }
    }
}

/// What a machine step decided.
pub enum Flow {
    /// Transition installed; keep stepping.
    Continue,
    /// Verification finished.
    Done(VerificationResult),
}

/// Advance the machine by one transition. Parks at the step boundary.
pub fn step(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> Result<Flow> {
    handle.safepoint();
    match &**handle {
        MachineState::PreflightGit { .. } => step_preflight_git(handle, providers),
        MachineState::PreflightSolver { .. } => step_preflight_solver(handle, providers),
        MachineState::PreflightPython { .. } => step_preflight_python(handle, providers),
        MachineState::PreflightAgent { .. } => step_preflight_agent(handle, providers),
        MachineState::PreflightProject { .. } => step_preflight_project(handle, providers),
        MachineState::Initializing { .. } => step_initializing(handle, providers),
        MachineState::ListingFunctions { .. } => step_listing_functions(handle, providers),
        MachineState::AnalyzingFunctions { .. } => step_analyzing_functions(handle, providers),
        MachineState::Splitting { .. } => step_splitting(handle, providers, pm),
        MachineState::Formalizing { .. } => step_formalizing(handle, providers, pm),
        MachineState::AnalyzingProblems { .. } => step_analyzing_problems(handle, providers),
        MachineState::Restarting { .. } => step_restarting(handle, providers),
    }
}

fn step_preflight_git(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
) -> Result<Flow> {
    info!("preflight: checking git");
    let _resp = providers.git.invoke(GitRequest::CurrentCommitHash)?;
    info!("preflight: git OK");
    let next = match &mut **handle {
        MachineState::PreflightGit { project_path } => MachineState::PreflightSolver {
                project_path: std::mem::take(project_path),
        },
        _ => anyhow::bail!("phase mismatch: expected PreflightGit"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn step_preflight_solver(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
) -> Result<Flow> {
    info!("preflight: checking z3 solver");
    let root = ContextId::root();
    let resp = providers.solver.invoke(SolverRequest {
        formula: "(check-sat)".to_string(),
        context_id: Box::new(root.new_child()),
    })?;
    info!(ctx = %resp.context_id, "preflight: solver OK");
    let next = match &mut **handle {
        MachineState::PreflightSolver { project_path } => MachineState::PreflightPython {
                project_path: std::mem::take(project_path),
        },
        _ => anyhow::bail!("phase mismatch: expected PreflightSolver"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn step_preflight_python(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
) -> Result<Flow> {
    info!("preflight: checking python3");
    let _resp = providers.python.invoke(PythonRequest {
        script: r#"print("(check-sat)")"#.to_string(),
    })?;
    info!("preflight: python3 OK");
    let next = match &mut **handle {
        MachineState::PreflightPython { project_path } => MachineState::PreflightAgent {
                project_path: std::mem::take(project_path),
        },
        _ => anyhow::bail!("phase mismatch: expected PreflightPython"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn step_preflight_agent(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
) -> Result<Flow> {
    info!("preflight: checking opencode agent");
    let working_directory = match &**handle {
        MachineState::PreflightAgent { project_path } => project_path.clone(),
        _ => anyhow::bail!("phase mismatch: expected PreflightAgent"),
    };
    let resp = providers.agent.invoke(AgentRequest {
        prompt: "Say OK".to_string(),
        working_directory,
        files_to_read: vec![],
        // Recoverable: a bad key or missing binary parks in the error
        // gate, the user fixes it and hits RETRY (the agent re-reads
        // the live key on every call).
        recoverable: true,
    })?;
    if !resp.success {
        anyhow::bail!("opencode agent check failed: {}", resp.stdout);
    }
    info!("preflight: opencode OK");
    let next = match &mut **handle {
        MachineState::PreflightAgent { project_path } => MachineState::PreflightProject {
                project_path: std::mem::take(project_path),
        },
        _ => anyhow::bail!("phase mismatch: expected PreflightAgent"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn step_preflight_project(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
) -> Result<Flow> {
    info!("preflight: checking project files");
    let project_path = match &**handle {
        MachineState::PreflightProject { project_path } => project_path.clone(),
        _ => anyhow::bail!("phase mismatch: expected PreflightProject"),
    };
    let resp = providers.git.invoke(GitRequest::WalkRustFiles { path: project_path })?;
    let count = resp.output.lines().filter(|l| !l.is_empty()).count();
    if count == 0 {
        anyhow::bail!("no .rs files found in project");
    }
    info!("preflight: project OK ({count} rust files)");
    info!("preflight: all checks passed");
    let next = match &mut **handle {
        MachineState::PreflightProject { project_path } => MachineState::Initializing {
                project_path: std::mem::take(project_path),
        },
        _ => anyhow::bail!("phase mismatch: expected PreflightProject"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn step_initializing(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
) -> Result<Flow> {
    let project_path = match &**handle {
        MachineState::Initializing { project_path } => project_path.clone(),
        _ => anyhow::bail!("phase mismatch: expected Initializing"),
    };
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let branch_name = format!("verification/{timestamp}");

    // Ensure the project is inside a git work tree. Ask git itself — a
    // subcrate of a bigger repo has no .git of its own, and git-init'ing
    // a nested repo there would be wrong; we branch in the enclosing
    // repo instead.
    let repo_root = providers.git.invoke(GitRequest::RepoRoot)?;
    if !repo_root.success {
        info!(project = %project_path.display(), "not inside a git work tree, initializing git repo");
        let resp = providers.git.invoke(GitRequest::Init { path: project_path.clone() })?;
        if !resp.success {
            anyhow::bail!("Failed to init git repo: {}", resp.output);
        }
        info!("adding rust files to git");
        let resp = providers.git.invoke(GitRequest::AddAll { path: project_path.clone() })?;
        if !resp.success {
            anyhow::bail!("Failed to add files: {}", resp.output);
        }
        let resp = providers.git.invoke(GitRequest::Commit { message: "Initial commit".to_string() })?;
        if !resp.success {
            anyhow::bail!("Failed to create initial commit: {}", resp.output);
        }
        info!("git repo initialized with initial commit");
    } else {
        info!(root = %repo_root.output.trim(), "using existing git work tree");
    }

    info!("IO: git CreateBranch");
    let resp = providers.git.invoke(GitRequest::CreateBranch { name: branch_name.clone() })?;
    info!("IO response: git CreateBranch");
    if !resp.success {
        anyhow::bail!("Failed to create verification branch: {}", resp.output);
    }

    let verification_dir = project_path.join("verification").join(&timestamp);
    info!("IO: git CreateDirectory");
    let resp = providers.git.invoke(GitRequest::CreateDirectory { path: verification_dir.clone() })?;
    info!("IO response: git CreateDirectory");
    if !resp.success {
        anyhow::bail!("Failed to create verification directory: {}", resp.output);
    }

    info!("IO: git WalkRustFiles");
    let resp = providers.git.invoke(GitRequest::WalkRustFiles { path: project_path.clone() })?;
    let mut all_rust_files: Vec<PathBuf> = resp
        .output
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect();
    let mut seen = std::collections::HashSet::new();
    all_rust_files.retain(|f| seen.insert(f.clone()));
    info!("IO response: {} rust files", all_rust_files.len());

    let next = match &mut **handle {
        MachineState::Initializing { .. } => MachineState::ListingFunctions {
                project_path,
                verification_dir,
                branch_name,
                files: all_rust_files,
                accumulated_bugs: Vec::new(),
        },
        _ => anyhow::bail!("phase mismatch: expected Initializing"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn step_listing_functions(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
) -> Result<Flow> {
    let files = match &**handle {
        MachineState::ListingFunctions { files, .. } => files.clone(),
        _ => anyhow::bail!("phase mismatch: expected ListingFunctions"),
    };
    info!("IO: rust-analyzer ListFunctions");
    let resp = providers
        .rust_analyzer
        .invoke(RustAnalyzerRequest::ListFunctions {
            files: files.clone(),
            cfg_verification: true,
        })?;
    info!("IO response: rust-analyzer ListFunctions");

    let functions = match resp {
        RustAnalyzerResponse::FunctionList(fns) => fns,
        _ => anyhow::bail!("Expected FunctionList from rust-analyzer"),
    };

    let target_file_set: HashSet<PathBuf> = files.iter().cloned().collect();
    let filtered = filter_connected_functions(functions, &target_file_set);

    let mut seen_fns = std::collections::HashSet::new();
    let guaranteed: Vec<FunctionInfo> = filtered
        .into_iter()
        .filter(|fi| fi.has_guarantees)
        .filter(|fi| seen_fns.insert((fi.id.file.clone(), fi.id.name.clone(), fi.id.line)))
        .collect();

    let function_map: std::collections::HashMap<PathBuf, Vec<FunctionId>> = {
        let mut map: std::collections::HashMap<PathBuf, Vec<FunctionId>> = std::collections::HashMap::new();
        for fi in &guaranteed {
            map.entry(fi.id.file.clone()).or_default().push(fi.id.clone());
        }
        map
    };
    drop(seen_fns);

    let mut function_docs: HashMap<FunctionId, String> = HashMap::new();
    for fi in &guaranteed {
        if !fi.docs.is_empty() {
            let _prev = function_docs.insert(fi.id.clone(), fi.docs.clone());
        }
    }

    let next = match &mut **handle {
        MachineState::ListingFunctions { project_path, verification_dir, branch_name, accumulated_bugs, .. } => MachineState::AnalyzingFunctions {
                project_path: std::mem::take(project_path),
                verification_dir: std::mem::take(verification_dir),
                branch_name: std::mem::take(branch_name),
                functions: function_map,
                function_docs,
                accumulated_bugs: std::mem::take(accumulated_bugs),
        },
        _ => anyhow::bail!("phase mismatch: expected ListingFunctions"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn step_analyzing_functions(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
) -> Result<Flow> {
    let functions = match &**handle {
        MachineState::AnalyzingFunctions { functions, .. } => functions,
        _ => anyhow::bail!("phase mismatch: expected AnalyzingFunctions"),
    };
    let mut function_bodies = Vec::new();
    for (file, function_ids) in functions {
        for fid in function_ids {
            debug!("IO: rust-analyzer GetFunctionCode");
            let resp = providers
                .rust_analyzer
                .invoke(RustAnalyzerRequest::GetFunctionCode {
                    function_id: fid.clone(),
                })?;
            debug!("IO response: rust-analyzer GetFunctionCode");
            let body = match resp {
                RustAnalyzerResponse::FunctionCode(code) => code,
                _ => anyhow::bail!("Expected FunctionCode from rust-analyzer"),
            };
            if !body.trim().is_empty() {
                function_bodies.push(SplitSlot {
                    file: file.clone(),
                    fid: fid.clone(),
                    body,
                    outcome: None,
                });
            }
        }
    }

    let next = match &mut **handle {
        MachineState::AnalyzingFunctions { project_path, verification_dir, branch_name, function_docs, accumulated_bugs, .. } => MachineState::Splitting {
                project_path: std::mem::take(project_path),
                verification_dir: std::mem::take(verification_dir),
                branch_name: std::mem::take(branch_name),
                function_bodies,
                function_docs: std::mem::take(function_docs),
                accumulated_bugs: std::mem::take(accumulated_bugs),
        },
        _ => anyhow::bail!("phase mismatch: expected AnalyzingFunctions"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn step_splitting(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> Result<Flow> {
    // The function bodies live in the state's slot vector; the fan-out
    // splits them off it directly, so a mid-fanout save sees everything.
    let docs: Vec<(FunctionId, String)> = match &mut **handle {
        MachineState::Splitting { function_docs, .. } => std::mem::take(function_docs).into_iter().collect(),
        _ => anyhow::bail!("phase mismatch: expected Splitting"),
    };
    let root_ctx = ContextId::root();
    handle.fanout(
        &|s| match s {
            MachineState::Splitting { function_bodies, .. } => function_bodies.iter_mut().collect(),
            _ => Vec::new(),
        },
        |mut child: Handle<'_, SplitSlot>| {
            child.safepoint();
            child.outcome = Some(split_function(&mut child, providers, pm, root_ctx));
            child.safepoint();
        },
    );

    let results: Vec<_> = match &mut **handle {
        MachineState::Splitting { function_bodies, .. } => std::mem::take(function_bodies)
            .into_iter()
            .map(|slot| slot.outcome.expect("fan-out child stores its outcome"))
            .collect::<Result<Vec<_>>>()?,
        _ => anyhow::bail!("phase mismatch: expected Splitting"),
    };

    for (fid, docs) in docs {
        pm.store_function_docs(fid, docs);
    }

    let code_pieces: Vec<(ArcCodePiece, ContextId)> = results.into_iter().flatten().collect();

    let next = match &mut **handle {
        MachineState::Splitting { project_path, verification_dir, branch_name, accumulated_bugs, .. } => MachineState::Formalizing {
            project_path: std::mem::take(project_path),
            verification_dir: std::mem::take(verification_dir),
            branch_name: std::mem::take(branch_name),
            pieces: code_pieces
                .into_iter()
                .map(|(piece, ctx)| PieceSlot { piece, ctx, outcome: None })
                .collect(),
            accumulated_bugs: std::mem::take(accumulated_bugs),
        },
        _ => anyhow::bail!("phase mismatch: expected Splitting"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

/// Distinct function names among the pieces — the result's function total.
fn functions_count(slots: &[PieceSlot]) -> usize {
    let mut seen = HashSet::new();
    for slot in slots {
        let _new = seen.insert(slot.piece.function_id().name.clone());
    }
    seen.len()
}

fn step_formalizing(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> Result<Flow> {
    // The pieces live in the state's slot vector; the fan-out splits them
    // off it directly, so a mid-fanout save sees everything.
    let verification_dir = match &**handle {
        MachineState::Formalizing { verification_dir, .. } => verification_dir.clone(),
        _ => anyhow::bail!("phase mismatch: expected Formalizing"),
    };
    let total_pieces = match &**handle {
        MachineState::Formalizing { pieces, .. } => pieces.len(),
        _ => anyhow::bail!("phase mismatch: expected Formalizing"),
    };
    let ctx_labels: Vec<String> = match &**handle {
        MachineState::Formalizing { pieces, .. } => pieces.iter().map(|s| s.ctx.to_string()).collect(),
        _ => anyhow::bail!("phase mismatch: expected Formalizing"),
    };

    handle.fanout(
        &|s| match s {
            MachineState::Formalizing { pieces, .. } => pieces.iter_mut().collect(),
            _ => Vec::new(),
        },
        |mut child: Handle<'_, PieceSlot>| {
            child.safepoint();
            child.outcome = Some(process_piece(&mut child, providers, pm, &verification_dir));
            child.safepoint();
        },
    );

    let slots = match &mut **handle {
        MachineState::Formalizing { pieces, .. } => std::mem::take(pieces),
        _ => anyhow::bail!("phase mismatch: expected Formalizing"),
    };
    let functions_count = functions_count(&slots);

    let mut closed_pieces = Vec::new();
    let mut unverified_pieces = Vec::new();

    for (i, slot) in slots.into_iter().enumerate() {
        let piece = slot.piece;
        match slot.outcome.expect("fan-out child stores its outcome") {
            Ok(outcome) => {
                if let Some(cp) = outcome.closed {
                    closed_pieces.push(cp);
                }
                if let Some(up) = outcome.unverified {
                    unverified_pieces.push(up);
                }
            }
            Err(e) => {
                warn!(ctx = %ctx_labels[i], error = %e, "Failed to process piece");
                unverified_pieces.push(unverified_from_piece(
                    &piece,
                    Vec::new(),
                    Vec::new(),
                    format!("Processing error: {}", e),
                ));
            }
        }
    }

    if unverified_pieces.is_empty() {
        let bug_reports = match &mut **handle {
            MachineState::Formalizing { accumulated_bugs, .. } => std::mem::take(accumulated_bugs),
            _ => anyhow::bail!("phase mismatch: expected Formalizing"),
        };
        return Ok(Flow::Done(VerificationResult {
            closed_pieces,
            unverified_pieces,
            bug_reports,
            total_functions: functions_count,
            total_pieces,
        }));
    }
    let next = match &mut **handle {
        MachineState::Formalizing { project_path, verification_dir, branch_name, accumulated_bugs, .. } => MachineState::AnalyzingProblems {
            project_path: std::mem::take(project_path),
            verification_dir: std::mem::take(verification_dir),
            branch_name: std::mem::take(branch_name),
            closed: closed_pieces,
            unverified: unverified_pieces,
            recheck_files: Vec::new(),
            base_commit: String::new(),
            accumulated_bugs: std::mem::take(accumulated_bugs),
            total_pieces,
            total_functions: functions_count,
        },
        _ => anyhow::bail!("phase mismatch: expected Formalizing"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn step_analyzing_problems(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
) -> Result<Flow> {
    let (project_path, unverified) = match &**handle {
        MachineState::AnalyzingProblems { project_path, unverified, .. } => {
            (project_path.clone(), unverified.clone())
        }
        _ => anyhow::bail!("phase mismatch: expected AnalyzingProblems"),
    };
    info!(
        unverified_count = unverified.len(),
        "ProblemAnalyzer: processing unverified pieces",
    );

    let mut bug_reports = Vec::new();
    let mut remaining_unverified = Vec::new();
    let mut files_to_recheck = Vec::new();

    let base_commit = match &**handle {
        MachineState::AnalyzingProblems { base_commit, .. } if !base_commit.is_empty() => base_commit.clone(),
        _ => {
            info!("IO: git CurrentCommitHash");
            let resp = providers.git.invoke(GitRequest::CurrentCommitHash)?;
            resp.output.trim().to_string()
        }
    };
    info!(%base_commit, "IO response: git CurrentCommitHash");

    for piece in &unverified {
        info!(
            file = %piece.file.display(),
            function = %piece.function_id.display_name(),
            "ProblemAnalyzer: processing unverified piece",
        );
        info!("IO: rust-analyzer GetFileContent");
        let source = match providers
            .rust_analyzer
            .invoke(RustAnalyzerRequest::GetFileContent {
                path: piece.file.clone(),
            })
        {
            Ok(RustAnalyzerResponse::FileContent(s)) if !s.is_empty() => s,
            Ok(_) => {
                warn!(file = %piece.file.display(), "Source file not found in VFS (empty content)");
                bug_reports.push(BugReport {
                    file: piece.file.clone(),
                    function_id: piece.function_id.clone(),
                    description: format!(
                        "{} at {}:{}-{}: could not read source file (not in VFS)",
                        piece.function_id.display_name(),
                        piece.file.display(),
                        piece.start_line,
                        piece.end_line,
                    ),
                });
                continue;
            }
            Err(e) => {
                warn!(file = %piece.file.display(), error = %e, "Failed to get source file from rust-analyzer");
                bug_reports.push(BugReport {
                    file: piece.file.clone(),
                    function_id: piece.function_id.clone(),
                    description: format!(
                        "{} at {}:{}-{}: rust-analyzer error: {}",
                        piece.function_id.display_name(),
                        piece.file.display(),
                        piece.start_line,
                        piece.end_line,
                        e,
                    ),
                });
                continue;
            }
        };

        let sat = if piece.sat_models.is_empty() {
            "None".to_string()
        } else {
            piece.sat_models.iter().map(|m| format!("```\n{}\n```", m)).collect::<Vec<_>>().join("\n\n")
        };
        let unknown = if piece.unknown_formulas.is_empty() {
            "None".to_string()
        } else {
            piece.unknown_formulas.iter().map(|f| format!("```\n{}\n```", f)).collect::<Vec<_>>().join("\n\n")
        };

        let prompt = format!(
            r#"You are a verification problem analyzer. A verification attempt for a Rust function has failed. Your job is to fix the problem by editing the source code.

## Failed verification

Function: {} at {}:{}
SAT models (counterexamples): {}
Unknown formulas: {}
Previous elaboration: {}

## Source file ({})

```
{}
```

## Instructions

Analyze why verification failed and fix the problem. Common patterns:
- Function f fails because it cannot satisfy the precondition of callee g, but that precondition stems from an overly strict precondition in g's callee h. Fix: relax h's preconditions.
- A loop invariant is too strong because a helper has unnecessarily strict preconditions. Fix: weaken the helper's preconditions.
- An assertion fails because a caller passes values violating an undocumented assumption. Fix: add a precondition to the caller.
- The formula is too complex. Fix: add `/* VERIFICATION HINT: ... */` comments explaining how to simplify.

You may edit ANY function in this file, not just the failing one. After fixing, respond with exactly one of:
- "RETRY" if you made code changes that should be re-verified
- A bug description if you found a genuine bug in the code (not just a verification issue)

If you respond RETRY, make sure you commit your changes first."#,
            piece.function_id.display_name(),
            piece.file.display(),
            piece.start_line,
            sat,
            unknown,
            piece.elaboration,
            piece.file.display(),
            source,
        );

        info!("IO: agent problem analysis");
        let agent_resp = providers
            .agent
            .invoke(AgentRequest {
                prompt,
                working_directory: project_path.clone(),
                files_to_read: vec![piece.file.clone()],
                recoverable: true,
            })?;
        info!("IO response: agent responded");

        let response = agent_resp.stdout.trim().to_string();
        info!(response_preview = %response.chars().take(500).collect::<String>(), success = agent_resp.success, "agent response");

        if response.to_uppercase().starts_with("RETRY") {
            info!("IO: git AddFiles");
            let _ = providers
                .git
                .invoke(GitRequest::AddFiles {
                    paths: vec![piece.file.clone()],
                });
            info!("IO response: files added");
            info!("IO: git Commit");
            let _ = providers
                .git
                .invoke(GitRequest::Commit {
                    message: format!("Agent fix for {} at {}:{}", piece.function_id.display_name(), piece.file.display(), piece.start_line),
                });
            info!("IO response: committed");

            remaining_unverified.push(piece.clone());
            files_to_recheck.push(piece.file.clone());
        } else {
            bug_reports.push(BugReport {
                file: piece.file.clone(),
                function_id: piece.function_id.clone(),
                description: format!(
                    "{} at {}:{}-{}: {}",
                    piece.function_id.display_name(),
                    piece.file.display(),
                    piece.start_line,
                    piece.end_line,
                    response
                ),
            });
        }
    }

    if remaining_unverified.is_empty() {
        match &mut **handle {
            MachineState::AnalyzingProblems { closed, total_functions, total_pieces, accumulated_bugs, .. } => {
                let mut all_bugs = std::mem::take(accumulated_bugs);
                all_bugs.extend(bug_reports);
                return Ok(Flow::Done(VerificationResult {
                    closed_pieces: std::mem::take(closed),
                    unverified_pieces: Vec::new(),
                    bug_reports: all_bugs,
                    total_functions: *total_functions,
                    total_pieces: *total_pieces,
                }));
            }
            _ => anyhow::bail!("phase mismatch: expected AnalyzingProblems"),
        }
    }
    let next = match &mut **handle {
        MachineState::AnalyzingProblems { project_path, verification_dir, branch_name, accumulated_bugs, .. } => {
            let mut all_bugs = std::mem::take(accumulated_bugs);
            all_bugs.extend(bug_reports);
            MachineState::Restarting {
                project_path: std::mem::take(project_path),
                verification_dir: std::mem::take(verification_dir),
                branch_name: std::mem::take(branch_name),
                base_commit,
                recheck_files: files_to_recheck,
                accumulated_bugs: all_bugs,
            }
        }
        _ => anyhow::bail!("phase mismatch: expected AnalyzingProblems"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn step_restarting(
    handle: &mut Handle<'_, MachineState>,
    providers: &Providers<'_>,
) -> Result<Flow> {
    let (base_commit, recheck_files) = match &**handle {
        MachineState::Restarting { base_commit, recheck_files, .. } => {
            (base_commit.clone(), recheck_files.clone())
        }
        _ => anyhow::bail!("phase mismatch: expected Restarting"),
    };
    info!("IO: git FindChangedRustFiles");
    let resp = providers
        .git
        .invoke(GitRequest::FindChangedRustFiles {
            base_commit: base_commit.clone(),
        })?;

    let changed_files: Vec<PathBuf> = if resp.success {
        resp.output
            .lines()
            .filter(|l| l.ends_with(".rs"))
            .map(PathBuf::from)
            .collect()
    } else {
        Vec::new()
    };
    info!("IO response: {} changed files", changed_files.len());

    let mut all_files: Vec<PathBuf> = recheck_files
        .iter()
        .chain(changed_files.iter())
        .cloned()
        .collect();
    let mut seen = HashSet::new();
    all_files.retain(|f| seen.insert(f.clone()));

    let next = match &mut **handle {
        MachineState::Restarting { project_path, verification_dir, branch_name, accumulated_bugs, .. } => MachineState::ListingFunctions {
                project_path: std::mem::take(project_path),
                verification_dir: std::mem::take(verification_dir),
                branch_name: std::mem::take(branch_name),
                files: all_files,
                accumulated_bugs: std::mem::take(accumulated_bugs),
        },
        _ => anyhow::bail!("phase mismatch: expected Restarting"),
    };
    **handle = next;
    Ok(Flow::Continue)
}

fn filter_connected_functions(
    functions: Vec<FunctionInfo>,
    target_files: &HashSet<PathBuf>,
) -> Vec<FunctionInfo> {
    let mut keep: HashSet<(PathBuf, String, u32)> = HashSet::new();

    for fi in &functions {
        if target_files.contains(&fi.id.file) {
            let _new = keep.insert((fi.id.file.clone(), fi.id.name.clone(), fi.id.line));
        }
    }

    let name_to_files: std::collections::HashMap<String, HashSet<PathBuf>> = {
        let mut map = std::collections::HashMap::new();
        for fi in &functions {
            let key = match &fi.id.impl_for {
                Some(imp) => format!("{}::{}", imp, fi.id.name),
                None => fi.id.name.clone(),
            };
            let _new = map.entry(key).or_insert_with(HashSet::new).insert(fi.id.file.clone());
        }
        map
    };

    let mut changed = true;
    while changed {
        changed = false;
        for fi in &functions {
            let key = (fi.id.file.clone(), fi.id.name.clone(), fi.id.line);
            if keep.contains(&key) {
                continue;
            }

            let in_target = target_files.contains(&fi.id.file);
            let called_by_target = {
                let lookup = match &fi.id.impl_for {
                    Some(imp) => format!("{}::{}", imp, fi.id.name),
                    None => fi.id.name.clone(),
                };
                name_to_files.get(&lookup).is_some_and(|files| {
                    files.iter().any(|f| target_files.contains(f))
                })
            };

            if in_target || called_by_target {
                let _new = keep.insert(key);
                changed = true;
            }
        }
    }

    functions
        .into_iter()
        .filter(|fi| keep.contains(&(fi.id.file.clone(), fi.id.name.clone(), fi.id.line)))
        .collect()
}


fn split_function(
    child: &mut Handle<'_, SplitSlot>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    root_ctx: &ContextId,
) -> Result<Vec<(ArcCodePiece, ContextId)>> {
    if child.body.trim().is_empty() {
        warn!(file = %child.file.display(), function = %child.fid.display_name(), "skipping function with empty body");
        return Ok(Vec::new());
    }

    let mut func_ctx = root_ctx.new_child();

    let max_attempts = 3;
    for attempt in 0..max_attempts {
        child.safepoint();
        let messages = vec![
            crate::llm::system_message(&prompts::splitter_system()),
            crate::llm::user_message(&prompts::splitter_user(
                &child.file.display().to_string(),
                &child.fid.display_name(),
                &child.body,
                child.fid.line,
            )),
        ];

        info!(%func_ctx, "IO: splitter");
        let resp = providers
            .llm
            .invoke(LlmRequest {
                role: LlmRole::Splitter,
                messages,
                context_id: Box::new(func_ctx),
            })?;
        func_ctx = *resp.context_id;
        info!(%func_ctx, "IO response: splitter");
        let response = resp.value;

        let pieces = parse_split_pieces(response, &func_ctx, &child.file, &child.fid, &child.body, pm);

        if !pieces.is_empty() {
            return Ok(pieces);
        }
        warn!(attempt = attempt + 1, %func_ctx, function = %child.fid.display_name(), "splitter produced no valid pieces, retrying");
    }

    let (piece, ctx) = pm.new_piece(
        &func_ctx,
        child.file.clone(),
        child.fid.clone(),
        child.fid.line,
        child.fid.line + child.body.lines().count() as u32,
        child.body.clone(),
    );
    Ok(vec![(Arc::new(piece), ctx)])
}


fn parse_split_pieces(
    response: String,
    func_ctx: &ContextId,
    file: &Path,
    fid: &FunctionId,
    original_body: &str,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> Vec<(ArcCodePiece, ContextId)> {
    let blocks = crate::formula::extract_fenced_blocks(&response);

    let mut pieces = Vec::new();
    for (_, content) in blocks {
        let code = content.trim().to_string();
        let end_line = fid.line + code.lines().count() as u32;
        let (piece, ctx) = pm.new_piece(
            func_ctx,
            file.to_path_buf(),
            fid.clone(),
            fid.line,
            end_line,
            code,
        );
        pieces.push((Arc::new(piece), ctx));
    }

    if pieces.is_empty() {
        let (piece, ctx) = pm.new_piece(
            func_ctx,
            file.to_path_buf(),
            fid.clone(),
            fid.line,
            fid.line + original_body.lines().count() as u32,
            original_body.to_string(),
        );
        pieces.push((Arc::new(piece), ctx));
    }

    pieces
}


pub struct PieceOutcome {
    pub(crate) closed: Option<ClosedPiece>,
    pub(crate) unverified: Option<UnverifiedPiece>,
}


fn gather_context(
    child: &mut Handle<'_, PieceSlot>,
    providers: &Providers<'_>,
) -> Result<String> {
    child.safepoint();
    info!(ctx = %child.ctx, "IO: rust-analyzer GetCalledFunctions");
    let resp = providers
        .rust_analyzer
        .invoke(RustAnalyzerRequest::GetCalledFunctions {
            function_id: child.piece.function_id().clone(),
        })?;

    let called_functions = match resp {
        RustAnalyzerResponse::CalledFunctionList(fns) => fns,
        _ => Vec::new(),
    };
    info!(ctx = %child.ctx, "IO response: {} called functions", called_functions.len());

    let mut context_parts = Vec::new();
    for cf in called_functions {
        let name = cf.name.clone();
        child.safepoint();
        info!(ctx = %child.ctx, "IO: rust-analyzer GetCalledFunctionCode");
        let resp = providers
            .rust_analyzer
            .invoke(RustAnalyzerRequest::GetCalledFunctionCode { called: cf })?;
        info!(ctx = %child.ctx, "IO response: rust-analyzer GetCalledFunctionCode");
        let result = match resp {
            RustAnalyzerResponse::CalledFunctionCode(r) => r,
            _ => continue,
        };
        if result.code.starts_with("// Could not find") {
            continue;
        }
        let docs_section = if result.docs.is_empty() {
            String::new()
        } else {
            format!("\nDocumentation:\n{}", result.docs)
        };
        context_parts.push(format!("--- {} ---\n{}{}", name, result.code, docs_section));
    }

    if context_parts.is_empty() {
        Ok("(no called functions found)".to_string())
    } else {
        Ok(context_parts.join("\n\n"))
    }
}


fn formalize_piece(
    child: &mut Handle<'_, PieceSlot>,
    context: &str,
    elaboration: &str,
    docs_section: &str,
    providers: &Providers<'_>,
) -> Result<String> {
    let messages = vec![
        crate::llm::system_message(&prompts::formalizer_system()),
        crate::llm::user_message(&prompts::formalizer_user(
            child.piece.code(),
            context,
            None,
            elaboration,
            docs_section,
        )),
    ];

    child.safepoint();
    info!(ctx = %child.ctx, "IO: formalizer");
    let resp = providers
        .llm
        .invoke(LlmRequest {
                role: LlmRole::Formalizer,
                messages,
                context_id: Box::new(child.ctx.new_child()),
            })?;
    info!(ctx = %child.ctx, "IO response: formalizer");

    Ok(resp.value)
}


fn check_formula(
    formula_content: &str,
    formula_source: &FormulaSource,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    iteration: u32,
    child: &mut Handle<'_, PieceSlot>,
) -> Result<(crate::formula::Formula, ContextId, SolverOutcome, String)> {
    let (formula, fctx) = pm.new_formula(&child.ctx, formula_content.to_string(), formula_source.clone(), iteration);

    pm.advance_formula(&fctx, Some(FormulaPhase::Open), FormulaPhase::Check);

    let smt_content = match translate_formula(formula_content, formula_source, &fctx, providers, child) {
        Ok(smt) => smt,
        Err(e) => {
            warn!(%fctx, error = %e, "Formula translation failed; routing to fixer");
            pm.advance_formula(&fctx, Some(FormulaPhase::Check), FormulaPhase::Fix);
            return Ok((formula, fctx, SolverOutcome::Error(format!("python3 translation failed: {e}")), formula_content.to_string()));
        }
    };

    child.safepoint();
    info!(%fctx, "IO: solver");
    let resp = providers
        .solver
            .invoke(SolverRequest {
                formula: smt_content.clone(),
                context_id: Box::new(fctx),
            })?;
    let fctx = *resp.context_id;
    let result = resp.value;
    info!(%fctx, outcome = ?result.outcome, "IO response: solver");

    let outcome = result.outcome.clone();

    pm.advance_formula(&fctx, Some(FormulaPhase::Check), solver_outcome_to_formula_phase(&outcome));

    Ok((formula, fctx, outcome, smt_content))
}


fn fix_formula(
    formula_content: &str,
    error: &str,
    fctx: &ContextId,
    context: &str,
    docs_section: &str,
    providers: &Providers<'_>,
    child: &mut Handle<'_, PieceSlot>,
) -> Result<String> {
    let messages = vec![
        crate::llm::system_message(&prompts::fixer_system()),
        crate::llm::user_message(&prompts::fixer_user(formula_content, error, context, docs_section)),
    ];

    child.safepoint();
    info!(%fctx, "IO: fixer");
    let resp = providers
        .llm
        .invoke(LlmRequest {
                role: LlmRole::Fixer,
                messages,
                context_id: Box::new(fctx.new_child()),
            })?;
    info!(%fctx, "IO response: fixer");

    Ok(resp.value)
}


fn translate_formula(
    content: &str,
    source: &FormulaSource,
    formula_ctx: &ContextId,
    providers: &Providers<'_>,
    child: &mut Handle<'_, PieceSlot>,
) -> Result<String> {
    match source {
        FormulaSource::SmtLib => Ok(content.to_string()),
        FormulaSource::PyZ3 => {
            child.safepoint();
            info!(%formula_ctx, "IO: python translate");
            let resp = providers
                .python
                .invoke(PythonRequest {
                    script: content.to_string(),
                })?;
            info!(%formula_ctx, "IO response: python translated");

            if resp.smtlib.is_empty() {
                anyhow::bail!("Python script produced no SMT output for ctx {}", formula_ctx);
            }
            Ok(resp.smtlib)
        }
    }
}

/// Knobs of the check-and-fix loop, grouped to keep the call sites flat.
pub(crate) struct FixLoop {
    pub(crate) iteration: u32,
    pub(crate) max_fix_attempts: usize,
}


fn check_and_fix_formula(
    ef: &crate::formula::ExtractedFormula,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    loop_knobs: &FixLoop,
    piece_ctx_info: &PieceContext,
    child: &mut Handle<'_, PieceSlot>,
) -> Result<(crate::formula::Formula, SolverOutcome, ContextId)> {
    child.safepoint();
    let FixLoop { iteration, max_fix_attempts } = *loop_knobs;

    let (mut formula, mut fctx, mut outcome, mut current_smt) = check_formula(
        &ef.content, &ef.source, providers, pm, iteration, child,
    )?;
    let mut fix_attempts = 0;
    let mut error_msg = match &outcome {
        SolverOutcome::Error(e) => e.clone(),
        _ => String::new(),
    };
    while matches!(outcome, SolverOutcome::Error(_)) && fix_attempts < max_fix_attempts {
        info!(%fctx, fix_attempts, "Attempting to fix formula");

        let fixed_response = fix_formula(&current_smt, &error_msg, &fctx, &piece_ctx_info.called_functions, &piece_ctx_info.docs_section, providers, child)?;
        let fixed_extracted = crate::formula::extract_formulas_from_response(&fixed_response);

        if fixed_extracted.is_empty() {
            fix_attempts += 1;
            continue;
        }

        let fixed_ef = &fixed_extracted[0];

        let translated = match translate_formula(&fixed_ef.content, &fixed_ef.source, &fctx, providers, child) {
            Ok(smt) => smt,
            Err(e) => {
                warn!(%fctx, fix_attempts, error = %e, "Failed to translate fixed formula; feeding error back to fixer");
                error_msg = format!("python3 translation failed: {e}");
                current_smt = fixed_ef.content.clone();
                fix_attempts += 1;
                continue;
            }
        };

        pm.expect_formula_phase_and_set(
            &fctx,
            &[FormulaPhase::Fix],
            FormulaPhase::Check,
        );

        info!(%fctx, "IO: solver retry");
        let resp = providers
            .solver
            .invoke(SolverRequest {
                formula: translated.clone(),
                context_id: Box::new(fctx),
            })?;
        fctx = *resp.context_id;
        let fixed_result = resp.value;
        info!(%fctx, outcome = ?fixed_result.outcome, "IO response: solver");

        outcome = fixed_result.outcome.clone();
        if let SolverOutcome::Error(e) = &outcome {
            error_msg = e.clone();
        }
        current_smt = translated;
        formula = crate::formula::Formula::new(
            fixed_ef.content.clone(),
            fixed_ef.source.clone(),
            formula.iteration(),
        );
        fix_attempts += 1;

        pm.advance_formula(&fctx, Some(FormulaPhase::Check), solver_outcome_to_formula_phase(&outcome));
        if !matches!(outcome, SolverOutcome::Error(_)) {
            break;
        }
    }

    Ok((formula, outcome, fctx))
}


fn judge_piece(
    child: &mut Handle<'_, PieceSlot>,
    formulas_summary: &str,
    piece_ctx_info: &PieceContext,
    providers: &Providers<'_>,
) -> Result<String> {
    let mut messages = vec![
        crate::llm::system_message(&prompts::judge_system()),
        crate::llm::user_message(&prompts::judge_user(
            child.piece.code(),
            formulas_summary,
            &piece_ctx_info.called_functions,
            &piece_ctx_info.docs_section,
        )),
    ];

    child.safepoint();
    info!(ctx = %child.ctx, "IO: judge");
    let resp = providers
        .llm
        .invoke(LlmRequest {
                role: LlmRole::Judge,
                messages: messages.clone(),
                context_id: Box::new(child.ctx.new_child()),
            })?;
    info!(ctx = %child.ctx, "IO response: judge");

    let mut response = resp.value;

    for retry in 0..crate::consts::MAX_JUDGE_CLARIFICATION_RETRIES {
        let trimmed_upper = response.trim().to_uppercase();
        if trimmed_upper == crate::consts::JUDGE_REASONABLE {
            break;
        }
        if !trimmed_upper.contains(crate::consts::JUDGE_REASONABLE) {
            break;
        }

        info!(ctx = %child.ctx, retry, "Judge response contained REASONABLE with extra text, retrying for clarification");

        messages.push(crate::llm::assistant_message(&response));
        messages.push(crate::llm::user_message(&prompts::judge_retry(&response)));

        child.safepoint();
        info!(ctx = %child.ctx, "IO: judge retry");
        let resp = providers
            .llm
            .invoke(LlmRequest {
                role: LlmRole::Judge,
                messages: messages.clone(),
                context_id: Box::new(child.ctx.new_child()),
            })?;

        response = resp.value;
        info!(ctx = %child.ctx, "IO response: judge retry");
    }

    Ok(response)
}


fn process_piece(
    child: &mut Handle<'_, PieceSlot>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    verification_dir: &Path,
) -> Result<PieceOutcome> {
    assert!(child.piece.type_invariant());

    child.safepoint();
    pm.expect_piece_phase_and_set(&child.ctx, &[CodePiecePhase::Open], CodePiecePhase::GetContext);

    let context = gather_context(child, providers)?;
    info!(ctx = %child.ctx, "Gathered context for piece");

    let own_docs = pm.get_function_docs(child.piece.function_id());
    let own_docs_section = if own_docs.is_empty() {
        String::new()
    } else {
        format!("\nFunction documentation:\n{}\nNote: Safety comments describe preconditions that must hold when the function is called. Treat them as verification preconditions.", own_docs)
    };

    let mut elaboration = String::new();
    let max_judge_attempts = crate::consts::MAX_JUDGE_ATTEMPTS;

    for judge_attempt in 0..max_judge_attempts {
        pm.advance_piece(&child.ctx, Some(CodePiecePhase::GetContext), CodePiecePhase::Formalizer);

        let formalizer_response = formalize_piece(child, &context, &elaboration, &own_docs_section, providers)?;
        info!(ctx = %child.ctx, judge_attempt, "formalizer step done");

        let extracted = crate::formula::extract_formulas_from_response(&formalizer_response);
        if extracted.is_empty() {
            warn!(ctx = %child.ctx, judge_attempt, "No formulas extracted from formalizer response");
            elaboration = format!("Attempt {}: No formulas were extracted. Please produce SMT-LIB2 formulas in ```smt2 code blocks.\n{}", judge_attempt + 1, elaboration);
            pm.advance_piece(&child.ctx, Some(CodePiecePhase::Formalizer), CodePiecePhase::GetContext);
            continue;
        }

        pm.advance_piece(&child.ctx, Some(CodePiecePhase::Formalizer), CodePiecePhase::Check);

        let max_fix_attempts = crate::consts::MAX_INSIST_ATTEMPTS;
        let iteration = judge_attempt as u32;

        let piece_ctx_info = PieceContext {
            called_functions: context.clone(),
            docs_section: own_docs_section.clone(),
        };

        let formula_results: Vec<_> = extracted.iter().map(|ef| {
            check_and_fix_formula(
                ef, providers, pm,
                &FixLoop { iteration, max_fix_attempts },
                &piece_ctx_info, child,
            )
        }).collect();

        let mut unsat_count = 0usize;
        let mut sat_models = Vec::new();
        let mut unknown_formulas = Vec::new();
        let mut formula_summaries = Vec::new();

        for (ef, result) in extracted.iter().zip(formula_results) {
            let (formula, outcome, fctx) = result?;
            let summary = format!(
                "Formula {} (iteration {}): {:?}\n```\n{}\n```",
                fctx,
                formula.iteration(),
                outcome,
                formula.content(),
            );
            formula_summaries.push(summary);

            match &outcome {
                SolverOutcome::Unsat => {
                    unsat_count += 1;
                }
                SolverOutcome::Sat => {
                    sat_models.push(formula.content().to_string());
                }
                SolverOutcome::Unknown => {
                    unknown_formulas.push(formula.content().to_string());
                }
                SolverOutcome::Error(e) => {
                    unknown_formulas.push(format!("(solver error: {})", e));
                }
            }

            let ext = match ef.source {
                FormulaSource::SmtLib => "smt2",
                FormulaSource::PyZ3 => "py",
            };
            let filename = format!("ctx{}_iter{}_attempt{}.{}", fctx, iteration, judge_attempt, ext);
            info!(ctx = %child.ctx, "IO: filesystem write formula");
            let _ = providers
                .filesystem
                .invoke(FileSystemRequest {
                    dir: verification_dir.to_path_buf(),
                    filename,
                    content: ef.content.clone(),
                })
                ;
            info!(ctx = %child.ctx, "IO response: formula written");
        }

        info!(ctx = %child.ctx, "IO: git AddFiles");
        let _ = providers
            .git
            .invoke(GitRequest::AddFiles {
                paths: vec![verification_dir.to_path_buf()],
            })
            ;
        info!(ctx = %child.ctx, "IO response: files added");
        info!(ctx = %child.ctx, "IO: git Commit");
        let _ = providers
            .git
            .invoke(GitRequest::Commit {
                message: format!("Add formulas for ctx {} (iteration {})", child.ctx, judge_attempt),
            })
            ;
        info!(ctx = %child.ctx, "IO response: committed");

        pm.advance_piece(&child.ctx, Some(CodePiecePhase::Check), CodePiecePhase::Judge);

        let formulas_summary = formula_summaries.join("\n");
        let judge_response = judge_piece(child, &formulas_summary, &piece_ctx_info, providers)?;
        info!(ctx = %child.ctx, judge_attempt, "judge step done");

        let trimmed_upper = judge_response.trim().to_uppercase();
        let is_exact = trimmed_upper == crate::consts::JUDGE_REASONABLE;
        let is_reasonable = is_exact || trimmed_upper.contains(crate::consts::JUDGE_REASONABLE);
        if is_reasonable && !is_exact {
            warn!(ctx = %child.ctx, "Judge response accepted via contains fallback (not exact match)");
        }

        if is_reasonable {
            if sat_models.is_empty() && unknown_formulas.is_empty() {
                pm.advance_piece(&child.ctx, Some(CodePiecePhase::Judge), CodePiecePhase::Closed);
                let piece = &child.piece;
                return Ok(PieceOutcome {
                    closed: Some(ClosedPiece {
                        file: piece.file().clone(),
                        function_id: piece.function_id().clone(),
                        start_line: piece.start_line(),
                        end_line: piece.end_line(),
                        unsat_formulas: unsat_count,
                        sat_formulas: sat_models.len(),
                        unknown_formulas: unknown_formulas.len(),
                    }),
                    unverified: None,
                });
            }
            pm.advance_piece(&child.ctx, Some(CodePiecePhase::Judge), CodePiecePhase::Unverified);
            return Ok(PieceOutcome {
                closed: None,
                unverified: Some(unverified_from_piece(
                    &child.piece,
                    sat_models,
                    unknown_formulas,
                    judge_response,
                )),
            });
        }

        elaboration = format!(
            "Judge attempt {} feedback: {}\n\nPrevious problems to avoid when reformalizing:\n{}",
            judge_attempt + 1,
            judge_response,
            elaboration
        );
        if judge_attempt + 1 < max_judge_attempts {
            pm.advance_piece(&child.ctx, Some(CodePiecePhase::Judge), CodePiecePhase::GetContext);
        } else {
            break;
        }
    }

    // The piece exhausted its judge attempts without being accepted. Its phase
    // is Judge if the last attempt reached the judge, or GetContext if the
    // last attempt hit the "no formulas extracted" branch (Formalizer ->
    // GetContext -> continue). Accept either.
    pm.expect_piece_phase_and_set(
        &child.ctx,
        &[CodePiecePhase::Judge, CodePiecePhase::GetContext],
        CodePiecePhase::Unverified,
    );
    Ok(PieceOutcome {
        closed: None,
        unverified: Some(unverified_from_piece(
            &child.piece,
            Vec::new(),
            Vec::new(),
            format!("Exhausted {} judge attempts. Last elaboration: {}", max_judge_attempts, elaboration),
        )),
    })
}


