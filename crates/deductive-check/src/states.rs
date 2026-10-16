use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result};
use refactor_check_core::checkpoint::{fanout, try_fanout, Cancellation, Handle};
use refactor_check_core::context_id::ContextId;
use tracing::{debug, info, warn};

use crate::code_piece::{ArcCodePiece, FunctionId};
use crate::formula::FormulaSource;
use crate::phase::{CodePiecePhase, FormulaPhase};
use crate::prompts;
use crate::provider::{CalledFunction, FileSystemRequest, GitRequest, PythonRequest, RustAnalyzerRequest, RustAnalyzerResponse, Providers, FunctionInfo, AgentRequest};
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

pub type Step = refactor_check_core::checkpoint::Step<Stage, VerificationResult>;

/// The machine between two steps, and the root node of its checkpoint tree.
/// Fan-out stages keep their children inside, and every child stores its own
/// progress and result, so a snapshot taken mid-fan-out records which
/// children already finished.
pub enum Stage {
    PreflightGit(PreflightGit),
    PreflightSolver(PreflightSolver),
    PreflightPython(PreflightPython),
    PreflightAgent(PreflightAgent),
    PreflightProject(PreflightProject),
    Initializer(Initializer),
    FunctionLister(FunctionLister),
    FunctionAnalyzer(FunctionAnalyzer),
    Splitter(Splitter),
    FullFormalizer(FullFormalizer),
    ProblemAnalyzer(ProblemAnalyzer),
    Restarter(Restarter),
}

/// A stage's transition. Fan-out stages run their children first (see
/// [`fan_out`]), so `execute` only collects what the children stored.
pub trait AlgorithmState: Send + Sync + 'static {
    fn execute(
        self,
        providers: &Providers<'_>,
        pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step>;
}

impl AlgorithmState for Stage {
    fn execute(
        self,
        providers: &Providers<'_>,
        pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        match self {
            Stage::PreflightGit(s) => s.execute(providers, pm),
            Stage::PreflightSolver(s) => s.execute(providers, pm),
            Stage::PreflightPython(s) => s.execute(providers, pm),
            Stage::PreflightAgent(s) => s.execute(providers, pm),
            Stage::PreflightProject(s) => s.execute(providers, pm),
            Stage::Initializer(s) => s.execute(providers, pm),
            Stage::FunctionLister(s) => s.execute(providers, pm),
            Stage::FunctionAnalyzer(s) => s.execute(providers, pm),
            Stage::Splitter(s) => s.execute(providers, pm),
            Stage::FullFormalizer(s) => s.execute(providers, pm),
            Stage::ProblemAnalyzer(s) => s.execute(providers, pm),
            Stage::Restarter(s) => s.execute(providers, pm),
        }
    }
}

/// Runs the parallel part of a fan-out stage: every child works on its own
/// node, on its own scoped thread, and stores its result there.
pub fn fan_out(
    h: &mut Handle<'_, Option<Stage>>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> Result<()> {
    match h.as_ref() {
        Some(Stage::FunctionAnalyzer(_)) => try_fanout(
            h,
            &|stage| match stage {
                Some(Stage::FunctionAnalyzer(state)) => state.functions.iter_mut().collect(),
                _ => Vec::new(),
            },
            |task, _| fetch_code(task, providers),
        ),
        Some(Stage::Splitter(_)) => try_fanout(
            h,
            &|stage| match stage {
                Some(Stage::Splitter(state)) => state.functions.iter_mut().collect(),
                _ => Vec::new(),
            },
            |task, cancellation| run_split(task, providers, pm, cancellation),
        ),
        Some(Stage::FullFormalizer(state)) => {
            let verification_dir = state.verification_dir.clone();
            let git_lock = Mutex::new(());
            fanout(
                h,
                &|stage| match stage {
                    Some(Stage::FullFormalizer(state)) => state.pieces.iter_mut().collect(),
                    _ => Vec::new(),
                },
                |task| run_piece(task, providers, pm, &verification_dir, &git_lock),
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

pub struct Initializer {
    project_path: PathBuf,
}

impl Initializer {
    pub fn new(project_path: PathBuf) -> Self {
        Self { project_path }
    }
}

/// Preflight: verify all external tools are installed and working before
/// starting the verification pipeline. Each check is a separate state using
/// the appropriate IOProvider, so failures are individually recoverable via
/// the error gate.
pub struct PreflightGit {
    project_path: PathBuf,
}
impl PreflightGit {
    pub fn new(project_path: PathBuf) -> Self {
        Self { project_path }
    }
}
impl AlgorithmState for PreflightGit {
    fn execute(self, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        info!("preflight: checking git");
        let _resp = providers.git.invoke(GitRequest::CurrentCommitHash)?;
        info!("preflight: git OK");
        Ok(Step::State(Stage::PreflightSolver(PreflightSolver { project_path: self.project_path })))
    }
}

pub struct PreflightSolver {
    project_path: PathBuf,
}
impl AlgorithmState for PreflightSolver {
    fn execute(self, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        info!("preflight: checking z3 solver");
        let root = ContextId::root();
        let resp = providers.solver.invoke(SolverRequest {
            formula: "(check-sat)".to_string(),
            context_id: Box::new(root.new_child()),
        })?;
        info!(ctx = %resp.context_id, "preflight: solver OK");
        Ok(Step::State(Stage::PreflightPython(PreflightPython { project_path: self.project_path })))
    }
}

pub struct PreflightPython {
    project_path: PathBuf,
}
impl AlgorithmState for PreflightPython {
    fn execute(self, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        info!("preflight: checking python3");
        let _resp = providers.python.invoke(PythonRequest {
            script: r#"print("(check-sat)")"#.to_string(),
        })?;
        info!("preflight: python3 OK");
        Ok(Step::State(Stage::PreflightAgent(PreflightAgent { project_path: self.project_path })))
    }
}

pub struct PreflightAgent {
    project_path: PathBuf,
}
impl AlgorithmState for PreflightAgent {
    fn execute(self, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        info!("preflight: checking opencode agent");
        let resp = providers.agent.invoke(AgentRequest {
            prompt: "Say OK".to_string(),
            working_directory: self.project_path.clone(),
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
        Ok(Step::State(Stage::PreflightProject(PreflightProject { project_path: self.project_path })))
    }
}

pub struct PreflightProject {
    project_path: PathBuf,
}
impl AlgorithmState for PreflightProject {
    fn execute(self, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        info!("preflight: checking project files");
        let resp = providers.git.invoke(GitRequest::WalkRustFiles {
            path: self.project_path.clone(),
        })?;
        let count = resp.output.lines().filter(|l| !l.is_empty()).count();
        if count == 0 {
            anyhow::bail!("no .rs files found in project");
        }
        info!("preflight: project OK ({count} rust files)");
        info!("preflight: all checks passed");
        Ok(Step::State(Stage::Initializer(Initializer::new(self.project_path))))
    }
}

impl AlgorithmState for Initializer {
    fn execute(
        self,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let branch_name = format!("verification/{timestamp}");

        // Ensure the project is inside a git work tree. Ask git itself — a
        // subcrate of a bigger repo has no .git of its own, and git-init'ing
        // a nested repo there would be wrong; we branch in the enclosing
        // repo instead.
        let repo_root = providers.git.invoke(GitRequest::RepoRoot)?;
        if !repo_root.success {
            info!(project = %self.project_path.display(), "not inside a git work tree, initializing git repo");
            let resp = providers
                .git
                .invoke(GitRequest::Init { path: self.project_path.clone() })?;
            if !resp.success {
                anyhow::bail!("Failed to init git repo: {}", resp.output);
            }
            info!("adding rust files to git");
            let resp = providers
                .git
                .invoke(GitRequest::AddAll { path: self.project_path.clone() })?;
            if !resp.success {
                anyhow::bail!("Failed to add files: {}", resp.output);
            }
            let resp = providers
                .git
                .invoke(GitRequest::Commit { message: "Initial commit".to_string() })?;
            if !resp.success {
                anyhow::bail!("Failed to create initial commit: {}", resp.output);
            }
            info!("git repo initialized with initial commit");
        } else {
            info!(root = %repo_root.output.trim(), "using existing git work tree");
        }

        info!("IO: git CreateBranch");
        let resp = providers
            .git
            .invoke(GitRequest::CreateBranch {
                name: branch_name.clone(),
            })?;
        info!("IO response: git CreateBranch");
        if !resp.success {
            anyhow::bail!("Failed to create verification branch: {}", resp.output);
        }

        let verification_dir = self.project_path.join("verification").join(&timestamp);
        info!("IO: git CreateDirectory");
        let resp = providers
            .git
            .invoke(GitRequest::CreateDirectory {
                path: verification_dir.clone(),
            })?;
        info!("IO response: git CreateDirectory");
        if !resp.success {
            anyhow::bail!("Failed to create verification directory: {}", resp.output);
        }

        info!("IO: git WalkRustFiles");
        let resp = providers
            .git
            .invoke(GitRequest::WalkRustFiles {
                path: self.project_path.clone(),
            })?;
        let mut all_rust_files: Vec<PathBuf> = resp
            .output
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
        let mut seen = std::collections::HashSet::new();
        all_rust_files.retain(|f| seen.insert(f.clone()));
        info!("IO response: {} rust files", all_rust_files.len());

        Ok(Step::State(Stage::FunctionLister(FunctionLister {
            project_path: self.project_path,
            verification_dir,
            branch_name,
            files: all_rust_files,
            accumulated_bugs: Vec::new(),
        })))
    }
}

pub struct FunctionLister {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    files: Vec<PathBuf>,
    accumulated_bugs: Vec<BugReport>,
}

impl AlgorithmState for FunctionLister {
    fn execute(
        self,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        info!("IO: rust-analyzer ListFunctions");
        let resp = providers
            .rust_analyzer
            .invoke(RustAnalyzerRequest::ListFunctions {
                files: self.files.clone(),
                cfg_verification: true,
            })?;
        info!("IO response: rust-analyzer ListFunctions");

        let functions = match resp {
            RustAnalyzerResponse::FunctionList(fns) => fns,
            _ => anyhow::bail!("Expected FunctionList from rust-analyzer"),
        };

        let target_file_set: HashSet<PathBuf> = self.files.iter().cloned().collect();
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

        let mut function_docs: HashMap<FunctionId, String> = HashMap::new();
        for fi in &guaranteed {
            if !fi.docs.is_empty() {
                let _prev = function_docs.insert(fi.id.clone(), fi.docs.clone());
            }
        }

        let functions = function_map
            .into_iter()
            .flat_map(|(file, fids)| fids.into_iter().map(move |fid| CodeTask { file: file.clone(), fid, body: None }))
            .collect();

        Ok(Step::State(Stage::FunctionAnalyzer(FunctionAnalyzer {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            functions,
            function_docs,
            accumulated_bugs: self.accumulated_bugs,
        })))
    }
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

pub struct FunctionAnalyzer {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    functions: Vec<CodeTask>,
    function_docs: HashMap<FunctionId, String>,
    accumulated_bugs: Vec<BugReport>,
}

/// One function of the [`FunctionAnalyzer`] fan-out; its worker stores the
/// function's body.
struct CodeTask {
    file: PathBuf,
    fid: FunctionId,
    body: Option<String>,
}

fn fetch_code(mut h: Handle<'_, CodeTask>, providers: &Providers<'_>) -> Result<()> {
    if h.body.is_some() {
        return Ok(());
    }
    debug!("IO: rust-analyzer GetFunctionCode");
    let resp = providers
        .rust_analyzer
        .invoke(RustAnalyzerRequest::GetFunctionCode {
            function_id: h.fid.clone(),
        })?;
    debug!("IO response: rust-analyzer GetFunctionCode");
    let body = match resp {
        RustAnalyzerResponse::FunctionCode(code) => code,
        _ => anyhow::bail!("Expected FunctionCode from rust-analyzer"),
    };
    h.body = Some(body);
    Ok(())
}

impl AlgorithmState for FunctionAnalyzer {
    fn execute(
        self,
        _providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let mut function_bodies = Vec::new();
        for task in self.functions {
            let body = task
                .body
                .with_context(|| format!("no code fetched for {}", task.fid.display_name()))?;
            if !body.trim().is_empty() {
                function_bodies.push((task.file, task.fid, body));
            }
        }

        // Contexts are handed out here, in function order, so the split
        // workers number them the same way however they are scheduled.
        let root_ctx = ContextId::root();
        let functions = function_bodies
            .into_iter()
            .map(|(file, fid, body)| SplitTask { file, fid, body, ctx: TaskCtx::new(root_ctx.new_child()), attempt: 0, pieces: None })
            .collect();

        Ok(Step::State(Stage::Splitter(Splitter {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            functions,
            function_docs: self.function_docs,
            accumulated_bugs: self.accumulated_bugs,
        })))
    }
}

pub struct Splitter {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    functions: Vec<SplitTask>,
    function_docs: HashMap<FunctionId, String>,
    accumulated_bugs: Vec<BugReport>,
}

/// A function's code pieces, each with its context.
type FunctionPieces = Vec<(ArcCodePiece, ContextId)>;

/// One function of the [`Splitter`] fan-out; its worker stores the function's
/// code pieces.
struct SplitTask {
    file: PathBuf,
    fid: FunctionId,
    body: String,
    ctx: TaskCtx,
    attempt: usize,
    pieces: Option<FunctionPieces>,
}

fn run_split(
    mut h: Handle<'_, SplitTask>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    cancellation: &Cancellation,
) -> Result<()> {
    while h.pieces.is_none() {
        h.safepoint();
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let task = &mut *h;
        let func_ctx = task.ctx.take()?;
        let (pieces, func_ctx) = split_attempt(providers, pm, func_ctx, &task.file, &task.fid, &task.body, task.attempt)?;
        task.ctx.put(func_ctx);
        task.attempt += 1;
        task.pieces = pieces;
    }
    Ok(())
}

impl AlgorithmState for Splitter {
    fn execute(
        self,
        _providers: &Providers<'_>,
        pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        for task in &self.functions {
            if let Some(docs) = self.function_docs.get(&task.fid) {
                pm.store_function_docs(task.fid.clone(), docs.clone());
            }
        }

        let mut pieces = Vec::new();
        for task in self.functions {
            let split = task
                .pieces
                .with_context(|| format!("function {} was not split", task.fid.display_name()))?;
            pieces.extend(split.into_iter().map(|(piece, ctx)| PieceTask::new(piece, ctx)));
        }

        Ok(Step::State(Stage::FullFormalizer(FullFormalizer {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            pieces,
            iteration: 0,
            accumulated_bugs: self.accumulated_bugs,
        })))
    }
}

/// Splitter calls per function before its whole body becomes one piece.
const MAX_SPLIT_ATTEMPTS: usize = 3;

/// One splitter attempt: the function's pieces, or `None` to retry.
fn split_attempt(
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    func_ctx: ContextId,
    file: &Path,
    fid: &FunctionId,
    body: &str,
    attempt: usize,
) -> Result<(Option<FunctionPieces>, ContextId)> {
    if body.trim().is_empty() {
        warn!(file = %file.display(), function = %fid.display_name(), "skipping function with empty body");
        return Ok((Some(Vec::new()), func_ctx));
    }

    if attempt >= MAX_SPLIT_ATTEMPTS {
        let (piece, ctx) = pm.new_piece(
            &func_ctx,
            file.to_path_buf(),
            fid.clone(),
            fid.line,
            fid.line + body.lines().count() as u32,
            body.to_string(),
        );
        return Ok((Some(vec![(Arc::new(piece), ctx)]), func_ctx));
    }

    let messages = vec![
        crate::llm::system_message(&prompts::splitter_system()),
        crate::llm::user_message(&prompts::splitter_user(
            &file.display().to_string(),
            &fid.display_name(),
            body,
            fid.line,
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
    let func_ctx = *resp.context_id;
    info!(%func_ctx, "IO response: splitter");
    let response = resp.value;

    let pieces = parse_split_pieces(response, &func_ctx, file, fid, body, pm);

    if !pieces.is_empty() {
        return Ok((Some(pieces), func_ctx));
    }
    warn!(attempt = attempt + 1, %func_ctx, function = %fid.display_name(), "splitter produced no valid pieces, retrying");
    Ok((None, func_ctx))
}

fn parse_split_pieces(
    response: String,
    func_ctx: &ContextId,
    file: &Path,
    fid: &FunctionId,
    original_body: &str,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> FunctionPieces {
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

pub struct FullFormalizer {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    pieces: Vec<PieceTask>,
    #[allow(dead_code)]
    iteration: usize,
    accumulated_bugs: Vec<BugReport>,
}

impl FullFormalizer {
    fn functions_count(&self) -> usize {
        let mut seen = HashSet::new();
        for task in &self.pieces {
            let _new = seen.insert(task.piece.function_id().name.clone());
        }
        seen.len()
    }
}

struct PieceOutcome {
    closed: Option<ClosedPiece>,
    unverified: Option<UnverifiedPiece>,
}

impl PieceOutcome {
    fn unverified(piece: UnverifiedPiece) -> Self {
        Self { closed: None, unverified: Some(piece) }
    }
}

/// A child's context: moved into each of its requests and back out of the
/// response. The name stays available for logging while it is in flight.
struct TaskCtx {
    ctx: Option<ContextId>,
    name: String,
}

impl TaskCtx {
    fn new(ctx: ContextId) -> Self {
        Self { name: ctx.to_string(), ctx: Some(ctx) }
    }

    fn take(&mut self) -> Result<ContextId> {
        self.ctx.take().ok_or_else(|| anyhow::anyhow!("context {} is already in flight", self.name))
    }

    fn get(&self) -> Result<&ContextId> {
        self.ctx.as_ref().ok_or_else(|| anyhow::anyhow!("context {} is in flight", self.name))
    }

    fn put(&mut self, ctx: ContextId) {
        self.ctx = Some(ctx);
    }
}

/// One code piece of the [`FullFormalizer`] fan-out. Its worker runs judge
/// attempts (formalize, check the formulas, judge) and keeps their progress
/// here: the next step follows from which fields are filled.
struct PieceTask {
    piece: ArcCodePiece,
    ctx: TaskCtx,
    /// The called functions whose code goes into `info`; `None` until listed.
    callees: Option<Vec<CalleeTask>>,
    /// Called-function context and docs, gathered once before the first attempt.
    info: Option<Arc<PieceContext>>,
    elaboration: String,
    attempt: usize,
    /// The current attempt's formulas; empty until it is formalized.
    formulas: Vec<FormulaTask>,
    outcome: Option<PieceOutcome>,
}

impl PieceTask {
    fn new(piece: ArcCodePiece, ctx: ContextId) -> Self {
        Self {
            piece,
            ctx: TaskCtx::new(ctx),
            callees: None,
            info: None,
            elaboration: String::new(),
            attempt: 0,
            formulas: Vec::new(),
            outcome: None,
        }
    }

    fn info(&self) -> Result<Arc<PieceContext>> {
        self.info.clone().with_context(|| format!("context of piece {} not gathered", self.ctx.name))
    }
}

/// One formula of a piece's judge attempt. Its worker solves it, then has it
/// fixed while the solver reports errors, keeping the progress here.
struct FormulaTask {
    /// The formula as the formalizer extracted it.
    extracted: crate::formula::ExtractedFormula,
    /// The latest formula, after fixes.
    formula: crate::formula::Formula,
    ctx: TaskCtx,
    /// The SMT text last sent to the solver, or the formula if it never translated.
    smt: String,
    /// The error the next fix attempt is asked to fix.
    error: String,
    fix_attempts: usize,
    outcome: Option<SolverOutcome>,
}

impl FormulaTask {
    fn new(
        extracted: crate::formula::ExtractedFormula,
        piece_ctx: &ContextId,
        pm: &dyn crate::piece_manager::DeductivePieceManager,
        iteration: u32,
    ) -> Self {
        let (formula, ctx) = pm.new_formula(piece_ctx, extracted.content.clone(), extracted.source.clone(), iteration);
        Self {
            extracted,
            formula,
            ctx: TaskCtx::new(ctx),
            smt: String::new(),
            error: String::new(),
            fix_attempts: 0,
            outcome: None,
        }
    }

    fn is_done(&self) -> bool {
        match &self.outcome {
            None => false,
            Some(SolverOutcome::Error(_)) => self.fix_attempts >= crate::consts::MAX_INSIST_ATTEMPTS,
            Some(_) => true,
        }
    }

}

/// A function a piece calls; its code and docs go into the piece's context.
struct CalleeTask {
    called: CalledFunction,
    looked_up: bool,
    /// The callee's context part; `None` if its code was not found.
    part: Option<String>,
}

fn look_up_callee(mut h: Handle<'_, CalleeTask>, piece_ctx: &str, providers: &Providers<'_>) -> Result<()> {
    if h.looked_up {
        return Ok(());
    }
    info!(ctx = %piece_ctx, "IO: rust-analyzer GetCalledFunctionCode");
    let resp = providers
        .rust_analyzer
        .invoke(RustAnalyzerRequest::GetCalledFunctionCode { called: h.called.clone() })?;
    info!(ctx = %piece_ctx, "IO response: rust-analyzer GetCalledFunctionCode");
    h.part = match resp {
        RustAnalyzerResponse::CalledFunctionCode(result) if !result.code.starts_with("// Could not find") => {
            let docs_section = if result.docs.is_empty() {
                String::new()
            } else {
                format!("\nDocumentation:\n{}", result.docs)
            };
            Some(format!("--- {} ---\n{}{}", h.called.name, result.code, docs_section))
        }
        _ => None,
    };
    h.looked_up = true;
    Ok(())
}

fn formalize_piece(
    piece: &ArcCodePiece,
    ctx: ContextId,
    context: &str,
    elaboration: &str,
    docs_section: &str,
    providers: &Providers<'_>,
) -> Result<(String, ContextId)> {
    assert!(piece.type_invariant());
    let messages = vec![
        crate::llm::system_message(&prompts::formalizer_system()),
        crate::llm::user_message(&prompts::formalizer_user(
            piece.code(),
            context,
            None,
            elaboration,
            docs_section,
        )),
    ];

    info!(%ctx, "IO: formalizer");
    let resp = providers
        .llm
        .invoke(LlmRequest {
                role: LlmRole::Formalizer,
                messages,
                context_id: Box::new(ctx),
            })?;
    let ctx = *resp.context_id;
    info!(%ctx, "IO response: formalizer");

    Ok((resp.value, ctx))
}

/// The formula's first solver run.
fn check_formula(
    task: &mut FormulaTask,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> Result<()> {
    let fctx = task.ctx.take()?;
    pm.advance_formula(&fctx, Some(FormulaPhase::Open), FormulaPhase::Check);

    let smt_content = match translate_formula(&task.extracted.content, &task.extracted.source, &fctx, providers) {
        Ok(smt) => smt,
        Err(e) => {
            warn!(%fctx, error = %e, "Formula translation failed; routing to fixer");
            pm.advance_formula(&fctx, Some(FormulaPhase::Check), FormulaPhase::Fix);
            task.ctx.put(fctx);
            task.error = format!("python3 translation failed: {e}");
            task.smt = task.extracted.content.clone();
            task.outcome = Some(SolverOutcome::Error(task.error.clone()));
            return Ok(());
        }
    };

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

    let outcome = result.outcome;

    pm.advance_formula(&fctx, Some(FormulaPhase::Check), solver_outcome_to_formula_phase(&outcome));

    task.ctx.put(fctx);
    task.error = match &outcome {
        SolverOutcome::Error(e) => e.clone(),
        _ => String::new(),
    };
    task.smt = smt_content;
    task.outcome = Some(outcome);
    Ok(())
}

fn fix_formula(
    formula_content: &str,
    error: &str,
    fctx: ContextId,
    context: &str,
    docs_section: &str,
    providers: &Providers<'_>,
) -> Result<(String, ContextId)> {
    let messages = vec![
        crate::llm::system_message(&prompts::fixer_system()),
        crate::llm::user_message(&prompts::fixer_user(formula_content, error, context, docs_section)),
    ];

    info!(%fctx, "IO: fixer");
    let resp = providers
        .llm
        .invoke(LlmRequest {
                role: LlmRole::Fixer,
                messages,
                context_id: Box::new(fctx),
            })?;
    let fctx = *resp.context_id;
    info!(%fctx, "IO response: fixer");

    Ok((resp.value, fctx))
}

fn translate_formula(
    content: &str,
    source: &FormulaSource,
    formula_ctx: &ContextId,
    providers: &Providers<'_>,
) -> Result<String> {
    match source {
        FormulaSource::SmtLib => Ok(content.to_string()),
        FormulaSource::PyZ3 => {
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

/// Solves the formula, then has it fixed while the solver reports errors, one
/// IO round per step with a safepoint before each.
fn run_formula(
    mut h: Handle<'_, FormulaTask>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    piece_ctx_info: &PieceContext,
    cancellation: &Cancellation,
) -> Result<()> {
    while !h.is_done() {
        h.safepoint();
        if cancellation.is_cancelled() {
            return Ok(());
        }
        if h.outcome.is_none() {
            check_formula(&mut h, providers, pm)?;
        } else {
            fix_attempt(&mut h, providers, pm, piece_ctx_info)?;
        }
    }
    Ok(())
}

fn fix_attempt(
    task: &mut FormulaTask,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    piece_ctx_info: &PieceContext,
) -> Result<()> {
    let fix_attempts = task.fix_attempts;
    let fctx = task.ctx.take()?;
    info!(%fctx, fix_attempts, "Attempting to fix formula");

    let (fixed_response, fctx) = fix_formula(&task.smt, &task.error, fctx, &piece_ctx_info.called_functions, &piece_ctx_info.docs_section, providers)?;
    task.fix_attempts += 1;
    let Some(fixed_ef) = crate::formula::extract_formulas_from_response(&fixed_response).into_iter().next() else {
        task.ctx.put(fctx);
        return Ok(());
    };

    let translated = match translate_formula(&fixed_ef.content, &fixed_ef.source, &fctx, providers) {
        Ok(smt) => smt,
        Err(e) => {
            warn!(%fctx, fix_attempts, error = %e, "Failed to translate fixed formula; feeding error back to fixer");
            task.ctx.put(fctx);
            task.error = format!("python3 translation failed: {e}");
            task.smt = fixed_ef.content;
            return Ok(());
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
    let fctx = *resp.context_id;
    let fixed_result = resp.value;
    info!(%fctx, outcome = ?fixed_result.outcome, "IO response: solver");

    let outcome = fixed_result.outcome;
    if let SolverOutcome::Error(e) = &outcome {
        task.error = e.clone();
    }
    task.smt = translated;
    task.formula = crate::formula::Formula::new(
        fixed_ef.content,
        fixed_ef.source,
        task.formula.iteration(),
    );

    pm.advance_formula(&fctx, Some(FormulaPhase::Check), solver_outcome_to_formula_phase(&outcome));
    task.ctx.put(fctx);
    task.outcome = Some(outcome);
    Ok(())
}

fn judge_piece(
    piece: &ArcCodePiece,
    ctx: ContextId,
    formulas_summary: &str,
    piece_ctx_info: &PieceContext,
    providers: &Providers<'_>,
) -> Result<(String, ContextId)> {
    assert!(piece.type_invariant());
    let mut messages = vec![
        crate::llm::system_message(&prompts::judge_system()),
        crate::llm::user_message(&prompts::judge_user(piece.code(), formulas_summary, &piece_ctx_info.called_functions, &piece_ctx_info.docs_section)),
    ];

    info!(%ctx, "IO: judge");
    let resp = providers
        .llm
        .invoke(LlmRequest {
                role: LlmRole::Judge,
                messages: messages.clone(),
                context_id: Box::new(ctx),
            })?;

    let mut response = resp.value;
    let mut ctx = *resp.context_id;
    info!(%ctx, "IO response: judge");

    for retry in 0..crate::consts::MAX_JUDGE_CLARIFICATION_RETRIES {
        let trimmed_upper = response.trim().to_uppercase();
        if trimmed_upper == crate::consts::JUDGE_REASONABLE {
            break;
        }
        if !trimmed_upper.contains(crate::consts::JUDGE_REASONABLE) {
            break;
        }

        info!(%ctx, retry, "Judge response contained REASONABLE with extra text, retrying for clarification");

        messages.push(crate::llm::assistant_message(&response));
        messages.push(crate::llm::user_message(&prompts::judge_retry(&response)));

        info!(%ctx, "IO: judge retry");
        let resp = providers
            .llm
            .invoke(LlmRequest {
                role: LlmRole::Judge,
                messages: messages.clone(),
                context_id: Box::new(ctx),
            })?;

        response = resp.value;
        ctx = *resp.context_id;
        info!(%ctx, "IO response: judge retry");
    }

    Ok((response, ctx))
}

/// Runs judge attempts until the piece has an outcome, one step at a time
/// with a safepoint before each. A failed step makes the piece unverified.
fn run_piece(
    mut h: Handle<'_, PieceTask>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    verification_dir: &Path,
    git_lock: &Mutex<()>,
) {
    while h.outcome.is_none() {
        h.safepoint();
        let stepped = if h.callees.is_none() && h.info.is_none() {
            list_callees(&mut h, providers, pm)
        } else if h.info.is_none() {
            gather_piece_context(&mut h, providers, pm)
        } else if h.formulas.is_empty() {
            formalize_attempt(&mut h, providers, pm)
        } else {
            check_and_judge_attempt(&mut h, providers, pm, verification_dir, git_lock)
        };
        if let Err(e) = stepped {
            warn!(ctx = %h.ctx.name, error = %e, "Failed to process piece");
            let unverified = unverified_from_piece(
                &h.piece,
                Vec::new(),
                Vec::new(),
                format!("Processing error: {}", e),
            );
            h.outcome = Some(PieceOutcome::unverified(unverified));
        }
    }
}

fn list_callees(
    task: &mut PieceTask,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> Result<()> {
    assert!(task.piece.type_invariant());
    let ctx = task.ctx.get()?;

    pm.expect_piece_phase_and_set(ctx, &[CodePiecePhase::Open], CodePiecePhase::GetContext);

    info!(%ctx, "IO: rust-analyzer GetCalledFunctions");
    let resp = providers
        .rust_analyzer
        .invoke(RustAnalyzerRequest::GetCalledFunctions {
            function_id: task.piece.function_id().clone(),
        })?;

    let called_functions = match resp {
        RustAnalyzerResponse::CalledFunctionList(fns) => fns,
        _ => Vec::new(),
    };
    info!(%ctx, "IO response: {} called functions", called_functions.len());

    task.callees = Some(
        called_functions
            .into_iter()
            .map(|called| CalleeTask { called, looked_up: false, part: None })
            .collect(),
    );
    Ok(())
}

/// Looks up the callees' code in parallel and assembles the piece's context.
fn gather_piece_context(
    h: &mut Handle<'_, PieceTask>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> Result<()> {
    let piece_ctx = h.ctx.name.clone();
    try_fanout(
        h,
        &|task: &mut PieceTask| task.callees.iter_mut().flatten().collect(),
        |callee, _| look_up_callee(callee, &piece_ctx, providers),
    )?;

    let task: &mut PieceTask = h;
    let parts: Vec<String> = task.callees.take().into_iter().flatten().filter_map(|c| c.part).collect();
    let context = if parts.is_empty() {
        "(no called functions found)".to_string()
    } else {
        parts.join("\n\n")
    };
    info!(ctx = %task.ctx.get()?, "Gathered context for piece");

    let own_docs = pm.get_function_docs(task.piece.function_id());
    let own_docs_section = if own_docs.is_empty() {
        String::new()
    } else {
        format!("\nFunction documentation:\n{}\nNote: Safety comments describe preconditions that must hold when the function is called. Treat them as verification preconditions.", own_docs)
    };

    task.info = Some(Arc::new(PieceContext {
        called_functions: context,
        docs_section: own_docs_section,
    }));
    Ok(())
}

fn formalize_attempt(
    task: &mut PieceTask,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> Result<()> {
    let max_judge_attempts = crate::consts::MAX_JUDGE_ATTEMPTS;
    if task.attempt >= max_judge_attempts {
        // The piece exhausted its judge attempts without being accepted. Its
        // phase is Judge if the last attempt reached the judge, or GetContext
        // if the last attempt hit the "no formulas extracted" branch. Accept
        // either.
        pm.expect_piece_phase_and_set(
            task.ctx.get()?,
            &[CodePiecePhase::Judge, CodePiecePhase::GetContext],
            CodePiecePhase::Unverified,
        );
        task.outcome = Some(PieceOutcome::unverified(unverified_from_piece(
            &task.piece,
            Vec::new(),
            Vec::new(),
            format!("Exhausted {} judge attempts. Last elaboration: {}", max_judge_attempts, task.elaboration),
        )));
        return Ok(());
    }

    let judge_attempt = task.attempt;
    let info = task.info()?;
    let ctx = task.ctx.take()?;
    pm.advance_piece(&ctx, Some(CodePiecePhase::GetContext), CodePiecePhase::Formalizer);

    let (formalizer_response, ctx) = formalize_piece(&task.piece, ctx, &info.called_functions, &task.elaboration, &info.docs_section, providers)?;
    info!(%ctx, judge_attempt, "formalizer step done");

    let extracted = crate::formula::extract_formulas_from_response(&formalizer_response);
    if extracted.is_empty() {
        warn!(%ctx, judge_attempt, "No formulas extracted from formalizer response");
        task.elaboration = format!("Attempt {}: No formulas were extracted. Please produce SMT-LIB2 formulas in ```smt2 code blocks.\n{}", judge_attempt + 1, task.elaboration);
        pm.advance_piece(&ctx, Some(CodePiecePhase::Formalizer), CodePiecePhase::GetContext);
        task.ctx.put(ctx);
        task.attempt += 1;
        return Ok(());
    }

    pm.advance_piece(&ctx, Some(CodePiecePhase::Formalizer), CodePiecePhase::Check);

    // Formula contexts are handed out here, in formula order, so the check
    // workers number them the same way however they are scheduled.
    let iteration = judge_attempt as u32;
    task.formulas = extracted
        .into_iter()
        .map(|ef| FormulaTask::new(ef, &ctx, pm, iteration))
        .collect();
    task.ctx.put(ctx);
    Ok(())
}

fn check_and_judge_attempt(
    h: &mut Handle<'_, PieceTask>,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    verification_dir: &Path,
    git_lock: &Mutex<()>,
) -> Result<()> {
    let piece_ctx_info = h.info()?;
    try_fanout(
        h,
        &|task: &mut PieceTask| task.formulas.iter_mut().collect(),
        |formula, cancellation| run_formula(formula, providers, pm, &piece_ctx_info, cancellation),
    )?;

    let task: &mut PieceTask = h;
    let judge_attempt = task.attempt;
    let iteration = judge_attempt as u32;
    let mut unsat_count = 0usize;
    let mut sat_models = Vec::new();
    let mut unknown_formulas = Vec::new();
    let mut formula_summaries = Vec::new();
    let mut written = Vec::new();

    for formula in &task.formulas {
        let outcome = formula
            .outcome
            .as_ref()
            .with_context(|| format!("formula {} ended without an outcome", formula.ctx.name))?;
        let summary = format!(
            "Formula {} (iteration {}): {:?}\n```\n{}\n```",
            formula.ctx.name,
            formula.formula.iteration(),
            outcome,
            formula.formula.content(),
        );
        formula_summaries.push(summary);

        match outcome {
            SolverOutcome::Unsat => {
                unsat_count += 1;
            }
            SolverOutcome::Sat => {
                sat_models.push(formula.formula.content().to_string());
            }
            SolverOutcome::Unknown => {
                unknown_formulas.push(formula.formula.content().to_string());
            }
            SolverOutcome::Error(e) => {
                unknown_formulas.push(format!("(solver error: {})", e));
            }
        }

        let ext = match formula.extracted.source {
            FormulaSource::SmtLib => "smt2",
            FormulaSource::PyZ3 => "py",
        };
        let filename = format!("ctx{}_iter{}_attempt{}.{}", formula.ctx.name, iteration, judge_attempt, ext);
        info!(ctx = %task.ctx.name, "IO: filesystem write formula");
        if let Ok(resp) = providers
            .filesystem
            .invoke(FileSystemRequest {
                dir: verification_dir.to_path_buf(),
                filename,
                content: formula.extracted.content.clone(),
            })
        {
            written.push(resp.path);
        }
        info!(ctx = %task.ctx.name, "IO response: formula written");
    }

    {
        // Pieces commit in parallel: staging and committing only this
        // attempt's files under the lock keeps every commit to its own piece.
        // The lock guards no data, so a poisoned one is still usable.
        let _git = git_lock.lock().unwrap_or_else(PoisonError::into_inner);
        info!(ctx = %task.ctx.name, "IO: git AddFiles");
        let _ = providers
            .git
            .invoke(GitRequest::AddFiles {
                paths: written,
            })
            ;
        info!(ctx = %task.ctx.name, "IO response: files added");
        info!(ctx = %task.ctx.name, "IO: git Commit");
        let _ = providers
            .git
            .invoke(GitRequest::Commit {
                message: format!("Add formulas for ctx {} (iteration {})", task.ctx.name, judge_attempt),
            })
            ;
        info!(ctx = %task.ctx.name, "IO response: committed");
    }

    let ctx = task.ctx.take()?;
    pm.advance_piece(&ctx, Some(CodePiecePhase::Check), CodePiecePhase::Judge);

    let formulas_summary = formula_summaries.join("\n");
    let (judge_response, ctx) = judge_piece(&task.piece, ctx, &formulas_summary, &piece_ctx_info, providers)?;
    info!(%ctx, judge_attempt, "judge step done");

    let trimmed_upper = judge_response.trim().to_uppercase();
    let is_exact = trimmed_upper == crate::consts::JUDGE_REASONABLE;
    let is_reasonable = is_exact || trimmed_upper.contains(crate::consts::JUDGE_REASONABLE);
    if is_reasonable && !is_exact {
        warn!(%ctx, "Judge response accepted via contains fallback (not exact match)");
    }

    if is_reasonable {
        if sat_models.is_empty() && unknown_formulas.is_empty() {
            pm.advance_piece(&ctx, Some(CodePiecePhase::Judge), CodePiecePhase::Closed);
            task.outcome = Some(PieceOutcome {
                closed: Some(ClosedPiece {
                    file: task.piece.file().clone(),
                    function_id: task.piece.function_id().clone(),
                    start_line: task.piece.start_line(),
                    end_line: task.piece.end_line(),
                    unsat_formulas: unsat_count,
                    sat_formulas: sat_models.len(),
                    unknown_formulas: unknown_formulas.len(),
                }),
                unverified: None,
            });
        } else {
            pm.advance_piece(&ctx, Some(CodePiecePhase::Judge), CodePiecePhase::Unverified);
            task.outcome = Some(PieceOutcome::unverified(unverified_from_piece(
                &task.piece,
                sat_models,
                unknown_formulas,
                judge_response,
            )));
        }
        task.ctx.put(ctx);
        return Ok(());
    }

    task.elaboration = format!(
        "Judge attempt {} feedback: {}\n\nPrevious problems to avoid when reformalizing:\n{}",
        judge_attempt + 1,
        judge_response,
        task.elaboration
    );
    if judge_attempt + 1 < crate::consts::MAX_JUDGE_ATTEMPTS {
        pm.advance_piece(&ctx, Some(CodePiecePhase::Judge), CodePiecePhase::GetContext);
    }
    task.ctx.put(ctx);
    task.attempt += 1;
    task.formulas.clear();
    Ok(())
}

impl AlgorithmState for FullFormalizer {
    fn execute(
        self,
        _providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let functions_count = self.functions_count();
        let total_pieces = self.pieces.len();
        let verification_dir = self.verification_dir;
        let project_path = self.project_path;
        let branch_name = self.branch_name;

        let mut closed_pieces = Vec::new();
        let mut unverified_pieces = Vec::new();

        for task in self.pieces {
            let outcome = task
                .outcome
                .with_context(|| format!("piece {} ended without an outcome", task.ctx.name))?;
            if let Some(cp) = outcome.closed {
                closed_pieces.push(cp);
            }
            if let Some(up) = outcome.unverified {
                unverified_pieces.push(up);
            }
        }

        if unverified_pieces.is_empty() {
            Ok(Step::Result(VerificationResult {
                closed_pieces,
                unverified_pieces,
                bug_reports: self.accumulated_bugs,
                total_functions: functions_count,
                total_pieces,
            }))
        } else {
            Ok(Step::State(Stage::ProblemAnalyzer(ProblemAnalyzer {
                project_path,
                verification_dir,
                branch_name,
                closed: closed_pieces,
                unverified: unverified_pieces,
                analyzed: 0,
                retry: false,
                recheck_files: Vec::new(),
                base_commit: String::new(),
                accumulated_bugs: self.accumulated_bugs,
                total_pieces,
                total_functions: functions_count,
            })))
        }
    }
}

pub struct ProblemAnalyzer {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    closed: Vec<ClosedPiece>,
    unverified: Vec<UnverifiedPiece>,
    /// How many of `unverified` were analyzed so far, one per step.
    analyzed: usize,
    /// Whether an analyzed piece was fixed and has to be re-verified.
    retry: bool,
    recheck_files: Vec<PathBuf>,
    base_commit: String,
    accumulated_bugs: Vec<BugReport>,
    total_pieces: usize,
    total_functions: usize,
}

enum ProblemAnalysis {
    Bug(BugReport),
    Retry,
}

/// Has the agent fix the problem behind an unverified piece, or report it as a
/// bug.
fn analyze_problem(
    piece: &UnverifiedPiece,
    project_path: &Path,
    providers: &Providers<'_>,
) -> Result<ProblemAnalysis> {
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
            return Ok(ProblemAnalysis::Bug(BugReport {
                file: piece.file.clone(),
                function_id: piece.function_id.clone(),
                description: format!(
                    "{} at {}:{}-{}: could not read source file (not in VFS)",
                    piece.function_id.display_name(),
                    piece.file.display(),
                    piece.start_line,
                    piece.end_line,
                ),
            }));
        }
        Err(e) => {
            warn!(file = %piece.file.display(), error = %e, "Failed to get source file from rust-analyzer");
            return Ok(ProblemAnalysis::Bug(BugReport {
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
            }));
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

If you respond RETRY, make sure to commit your changes first."#,
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
            working_directory: project_path.to_path_buf(),
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
            })
            ;
        info!("IO response: files added");
        info!("IO: git Commit");
        let _ = providers
            .git
            .invoke(GitRequest::Commit {
                message: format!("Agent fix for {} at {}:{}", piece.function_id.display_name(), piece.file.display(), piece.start_line),
            })
            ;
        info!("IO response: committed");
        Ok(ProblemAnalysis::Retry)
    } else {
        Ok(ProblemAnalysis::Bug(BugReport {
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
        }))
    }
}

impl AlgorithmState for ProblemAnalyzer {
    fn execute(
        mut self,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        if self.analyzed == 0 {
            info!(
                unverified_count = self.unverified.len(),
                closed_count = self.closed.len(),
                "ProblemAnalyzer starting",
            );
        }

        if self.base_commit.is_empty() {
            info!("IO: git CurrentCommitHash");
            let resp = providers.git.invoke(GitRequest::CurrentCommitHash)?;
            self.base_commit = resp.output.trim().to_string();
            info!(base_commit = %self.base_commit, "IO response: git CurrentCommitHash");
        }

        if let Some(piece) = self.unverified.get(self.analyzed) {
            info!(
                piece_index = self.analyzed,
                total = self.unverified.len(),
                file = %piece.file.display(),
                function = %piece.function_id.display_name(),
                "ProblemAnalyzer: processing unverified piece",
            );
            match analyze_problem(piece, &self.project_path, providers)? {
                ProblemAnalysis::Bug(report) => self.accumulated_bugs.push(report),
                ProblemAnalysis::Retry => {
                    self.recheck_files.push(piece.file.clone());
                    self.retry = true;
                }
            }
            self.analyzed += 1;
        }
        if self.analyzed < self.unverified.len() {
            return Ok(Step::State(Stage::ProblemAnalyzer(self)));
        }

        if !self.retry {
            Ok(Step::Result(VerificationResult {
                closed_pieces: self.closed,
                unverified_pieces: Vec::new(),
                bug_reports: self.accumulated_bugs,
                total_functions: self.total_functions,
                total_pieces: self.total_pieces,
            }))
        } else {
            Ok(Step::State(Stage::Restarter(Restarter {
                project_path: self.project_path,
                verification_dir: self.verification_dir,
                branch_name: self.branch_name,
                base_commit: self.base_commit,
                recheck_files: self.recheck_files,
                accumulated_bugs: self.accumulated_bugs,
            })))
        }
    }
}

pub struct Restarter {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    base_commit: String,
    recheck_files: Vec<PathBuf>,
    accumulated_bugs: Vec<BugReport>,
}

impl AlgorithmState for Restarter {
    fn execute(
        self,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        info!("IO: git FindChangedRustFiles");
        let resp = providers
            .git
            .invoke(GitRequest::FindChangedRustFiles {
                base_commit: self.base_commit.clone(),
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

        let mut all_files: Vec<PathBuf> = self
            .recheck_files
            .iter()
            .chain(changed_files.iter())
            .cloned()
            .collect();
        let mut seen = HashSet::new();
        all_files.retain(|f| seen.insert(f.clone()));

        Ok(Step::State(Stage::FunctionLister(FunctionLister {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            files: all_files,
            accumulated_bugs: self.accumulated_bugs,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece_manager::{DeductivePieceManager, DefaultDeductivePieceManager};
    use refactor_check_core::checkpoint::drive;
    use refactor_check_core::provider::IOProvider;

    /// Any IO means a finished child ran again.
    struct NoIo;

    impl<I, O> IOProvider<I, O> for NoIo {
        fn invoke(&self, _input: I) -> Result<O> {
            anyhow::bail!("a finished child did IO")
        }
    }

    #[test]
    fn restored_full_formalizer_completes_without_io() {
        let pm = DefaultDeductivePieceManager::default();
        let fid = FunctionId::new(PathBuf::from("src/lib.rs"), "f", None, 1);
        let pieces = (0..2)
            .map(|_| {
                let (piece, ctx) = pm.new_piece(ContextId::root(), fid.file.clone(), fid.clone(), 1, 2, "x".to_string());
                let mut task = PieceTask::new(Arc::new(piece), ctx);
                task.outcome = Some(PieceOutcome {
                    closed: Some(ClosedPiece {
                        file: fid.file.clone(),
                        function_id: fid.clone(),
                        start_line: 1,
                        end_line: 2,
                        unsat_formulas: 1,
                        sat_formulas: 0,
                        unknown_formulas: 0,
                    }),
                    unverified: None,
                });
                task
            })
            .collect();
        let start = Stage::FullFormalizer(FullFormalizer {
            project_path: PathBuf::new(),
            verification_dir: PathBuf::new(),
            branch_name: String::new(),
            pieces,
            iteration: 0,
            accumulated_bugs: Vec::new(),
        });
        let providers = Providers {
            llm: &NoIo,
            solver: &NoIo,
            rust_analyzer: &NoIo,
            git: &NoIo,
            filesystem: &NoIo,
            python: &NoIo,
            agent: &NoIo,
        };

        let result = drive(start, |h| fan_out(h, &providers, &pm), |stage| stage.execute(&providers, &pm))
            .expect("finished pieces need no IO");

        assert_eq!(result.closed_pieces.len(), 2);
        assert!(result.unverified_pieces.is_empty());
    }
}
