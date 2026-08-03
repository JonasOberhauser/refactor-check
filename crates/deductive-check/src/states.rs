use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use refactor_check_core::context_id::ContextId;
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

pub enum Step {
    State(Box<dyn AlgorithmState>),
    Result(VerificationResult),
}

#[async_trait]
pub trait AlgorithmState: Send + Sync + 'static {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step>;
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
#[async_trait]
impl AlgorithmState for PreflightGit {
    async fn execute(self: Box<Self>, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        info!("preflight: checking git");
        providers.git.invoke(GitRequest::CurrentCommitHash).await?;
        info!("preflight: git OK");
        Ok(Step::State(Box::new(PreflightSolver { project_path: self.project_path })))
    }
}

pub struct PreflightSolver {
    project_path: PathBuf,
}
#[async_trait]
impl AlgorithmState for PreflightSolver {
    async fn execute(self: Box<Self>, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        info!("preflight: checking z3 solver");
        let root = ContextId::root();
        let resp = providers.solver.invoke(SolverRequest {
            formula: "(check-sat)".to_string(),
            context_id: Box::new(root.new_child()),
        }).await?;
        info!(ctx = %resp.context_id, "preflight: solver OK");
        Ok(Step::State(Box::new(PreflightPython { project_path: self.project_path })))
    }
}

pub struct PreflightPython {
    project_path: PathBuf,
}
#[async_trait]
impl AlgorithmState for PreflightPython {
    async fn execute(self: Box<Self>, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        info!("preflight: checking python3");
        providers.python.invoke(PythonRequest {
            script: r#"print("(check-sat)")"#.to_string(),
        }).await?;
        info!("preflight: python3 OK");
        Ok(Step::State(Box::new(PreflightAgent { project_path: self.project_path })))
    }
}

pub struct PreflightAgent {
    project_path: PathBuf,
}
#[async_trait]
impl AlgorithmState for PreflightAgent {
    async fn execute(self: Box<Self>, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        info!("preflight: checking opencode agent");
        let resp = providers.agent.invoke(AgentRequest {
            prompt: "Say OK".to_string(),
            working_directory: self.project_path.clone(),
            files_to_read: vec![],
        }).await?;
        if !resp.success {
            anyhow::bail!("agent check failed: {}", resp.stdout);
        }
        info!("preflight: opencode OK");
        Ok(Step::State(Box::new(PreflightProject { project_path: self.project_path })))
    }
}

pub struct PreflightProject {
    project_path: PathBuf,
}
#[async_trait]
impl AlgorithmState for PreflightProject {
    async fn execute(self: Box<Self>, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        info!("preflight: checking project files");
        let resp = providers.git.invoke(GitRequest::WalkRustFiles {
            path: self.project_path.clone(),
        }).await?;
        let count = resp.output.lines().filter(|l| !l.is_empty()).count();
        if count == 0 {
            anyhow::bail!("no .rs files found in project");
        }
        info!("preflight: project OK ({count} rust files)");
        info!("preflight: all checks passed");
        Ok(Step::State(Box::new(Initializer::new(self.project_path))))
    }
}

#[async_trait]
impl AlgorithmState for Initializer {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let branch_name = format!("verification/{timestamp}");

        // Ensure the project is a git repo
        let git_dir = self.project_path.join(".git");
        if !git_dir.exists() {
            info!(project = %self.project_path.display(), "no .git found, initializing git repo");
            let resp = providers
                .git
                .invoke(GitRequest::Init { path: self.project_path.clone() })
                .await?;
            if !resp.success {
                anyhow::bail!("Failed to init git repo: {}", resp.output);
            }
            info!("adding rust files to git");
            let resp = providers
                .git
                .invoke(GitRequest::AddAll { path: self.project_path.clone() })
                .await?;
            if !resp.success {
                anyhow::bail!("Failed to add files: {}", resp.output);
            }
            let resp = providers
                .git
                .invoke(GitRequest::Commit { message: "Initial commit".to_string() })
                .await?;
            if !resp.success {
                anyhow::bail!("Failed to create initial commit: {}", resp.output);
            }
            info!("git repo initialized with initial commit");
        }

        info!("IO: git CreateBranch");
        let resp = providers
            .git
            .invoke(GitRequest::CreateBranch {
                name: branch_name.clone(),
            })
            .await?;
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
            })
            .await?;
        info!("IO response: git CreateDirectory");
        if !resp.success {
            anyhow::bail!("Failed to create verification directory: {}", resp.output);
        }

        info!("IO: git WalkRustFiles");
        let resp = providers
            .git
            .invoke(GitRequest::WalkRustFiles {
                path: self.project_path.clone(),
            })
            .await?;
        let mut all_rust_files: Vec<PathBuf> = resp
            .output
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();
        let mut seen = std::collections::HashSet::new();
        all_rust_files.retain(|f| seen.insert(f.clone()));
        info!("IO response: {} rust files", all_rust_files.len());

        Ok(Step::State(Box::new(FunctionLister {
            project_path: self.project_path,
            verification_dir,
            branch_name,
            files: all_rust_files,
        })))
    }
}

pub struct FunctionLister {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    files: Vec<PathBuf>,
}

#[async_trait]
impl AlgorithmState for FunctionLister {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        info!("IO: rust-analyzer ListFunctions");
        let resp = providers
            .rust_analyzer
            .invoke(RustAnalyzerRequest::ListFunctions {
                files: self.files.clone(),
                cfg_verification: true,
            })
            .await?;
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
                function_docs.insert(fi.id.clone(), fi.docs.clone());
            }
        }

        Ok(Step::State(Box::new(FunctionAnalyzer {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            functions: function_map,
            function_docs,
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
            keep.insert((fi.id.file.clone(), fi.id.name.clone(), fi.id.line));
        }
    }

    let name_to_files: std::collections::HashMap<String, HashSet<PathBuf>> = {
        let mut map = std::collections::HashMap::new();
        for fi in &functions {
            let key = match &fi.id.impl_for {
                Some(imp) => format!("{}::{}", imp, fi.id.name),
                None => fi.id.name.clone(),
            };
            map.entry(key).or_insert_with(HashSet::new).insert(fi.id.file.clone());
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
                keep.insert(key);
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
    functions: std::collections::HashMap<PathBuf, Vec<FunctionId>>,
    function_docs: HashMap<FunctionId, String>,
}

#[async_trait]
impl AlgorithmState for FunctionAnalyzer {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let mut futs = Vec::new();
        for (file, function_ids) in &self.functions {
            for fid in function_ids {
                let file = file.clone();
                let fid = fid.clone();
                let fut = async {
                    debug!("IO: rust-analyzer GetFunctionCode");
                    let resp = providers
                        .rust_analyzer
                        .invoke(RustAnalyzerRequest::GetFunctionCode {
                            function_id: fid.clone(),
                        })
                        .await?;
                    debug!("IO response: rust-analyzer GetFunctionCode");
                    let body = match resp {
                        RustAnalyzerResponse::FunctionCode(code) => code,
                        _ => anyhow::bail!("Expected FunctionCode from rust-analyzer"),
                    };
                    Ok::<_, anyhow::Error>((file, fid, body))
                };
                futs.push(fut);
            }
        }

        let function_bodies: Vec<(PathBuf, FunctionId, String)> = futures::future::try_join_all(futs)
            .await?
            .into_iter()
            .filter(|(_, _, body)| !body.trim().is_empty())
            .collect();

        Ok(Step::State(Box::new(Splitter {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            function_bodies,
            function_docs: self.function_docs,
        })))
    }
}

pub struct Splitter {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    function_bodies: Vec<(PathBuf, FunctionId, String)>,
    function_docs: HashMap<FunctionId, String>,
}

#[async_trait]
impl AlgorithmState for Splitter {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let root_ctx = ContextId::root();
        let futs: Vec<_> = self.function_bodies.iter().map(|(file, fid, body)| {
            split_function(providers, pm, root_ctx, file, fid, body)
        }).collect();

        let results = futures::future::try_join_all(futs).await?;

        for (_file, fid, _body) in &self.function_bodies {
            if let Some(docs) = self.function_docs.get(fid) {
                pm.store_function_docs(fid.clone(), docs.clone());
            }
        }

        let code_pieces: Vec<(ArcCodePiece, ContextId)> = results.into_iter().flatten().collect();

        Ok(Step::State(Box::new(FullFormalizer {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            pieces: code_pieces,
            iteration: 0,
        })))
    }
}

async fn split_function(
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    root_ctx: &ContextId,
    file: &Path,
    fid: &FunctionId,
    body: &str,
) -> Result<Vec<(ArcCodePiece, ContextId)>> {
    if body.trim().is_empty() {
        warn!(file = %file.display(), function = %fid.display_name(), "skipping function with empty body");
        return Ok(Vec::new());
    }

    let mut func_ctx = root_ctx.new_child();

    let max_attempts = 3;
    for attempt in 0..max_attempts {
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
            })
            .await?;
        func_ctx = *resp.context_id;
        info!(%func_ctx, "IO response: splitter");
        let response = resp.value;

        let pieces = parse_split_pieces(response, &func_ctx, file, fid, body, pm);

        if !pieces.is_empty() {
            return Ok(pieces);
        }
        warn!(attempt = attempt + 1, %func_ctx, function = %fid.display_name(), "splitter produced no valid pieces, retrying");
    }

    let (piece, ctx) = pm.new_piece(
        &func_ctx,
        file.to_path_buf(),
        fid.clone(),
        fid.line,
        fid.line + body.lines().count() as u32,
        body.to_string(),
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

pub struct FullFormalizer {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    pieces: Vec<(ArcCodePiece, ContextId)>,
    #[allow(dead_code)]
    iteration: usize,
}

impl FullFormalizer {
    fn functions_count(&self) -> usize {
        let mut seen = HashSet::new();
        for (piece, _) in &self.pieces {
            seen.insert(piece.function_id().name.clone());
        }
        seen.len()
    }
}

struct PieceOutcome {
    closed: Option<ClosedPiece>,
    unverified: Option<UnverifiedPiece>,
}

async fn gather_context(
    piece: &ArcCodePiece,
    ctx: &ContextId,
    providers: &Providers<'_>,
) -> Result<String> {
    assert!(piece.type_invariant());
    info!(%ctx, "IO: rust-analyzer GetCalledFunctions");
    let resp = providers
        .rust_analyzer
        .invoke(RustAnalyzerRequest::GetCalledFunctions {
            function_id: piece.function_id().clone(),
        })
        .await?;

    let called_functions = match resp {
        RustAnalyzerResponse::CalledFunctionList(fns) => fns,
        _ => Vec::new(),
    };
    info!(%ctx, "IO response: {} called functions", called_functions.len());

    let futs: Vec<_> = called_functions.into_iter().map(|cf| {
        async move {
            let name = cf.name.clone();
            info!(%ctx, "IO: rust-analyzer GetCalledFunctionCode");
            let resp = providers
                .rust_analyzer
                .invoke(RustAnalyzerRequest::GetCalledFunctionCode {
                    called: cf,
                })
                .await?;
            info!(%ctx, "IO response: rust-analyzer GetCalledFunctionCode");
            let result = match resp {
                RustAnalyzerResponse::CalledFunctionCode(r) => r,
                _ => return Ok::<_, anyhow::Error>(None),
            };
            if result.code.starts_with("// Could not find") {
                return Ok(None);
            }
            let docs_section = if result.docs.is_empty() {
                String::new()
            } else {
                format!("\nDocumentation:\n{}", result.docs)
            };
            Ok(Some(format!("--- {} ---\n{}{}", name, result.code, docs_section)))
        }
    }).collect();

    let context_parts: Vec<String> = futures::future::try_join_all(futs)
        .await?
        .into_iter()
        .flatten()
        .collect();

    if context_parts.is_empty() {
        Ok("(no called functions found)".to_string())
    } else {
        Ok(context_parts.join("\n\n"))
    }
}

async fn formalize_piece(
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
            })
            .await?;
    let ctx = *resp.context_id;
    info!(%ctx, "IO response: formalizer");

    Ok((resp.value, ctx))
}

async fn check_formula(
    formula_content: &str,
    formula_source: &FormulaSource,
    piece_ctx: &ContextId,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    iteration: u32,
) -> Result<(crate::formula::Formula, ContextId, SolverOutcome, String)> {
    let (formula, fctx) = pm.new_formula(piece_ctx, formula_content.to_string(), formula_source.clone(), iteration);

    pm.advance_formula(&fctx, Some(FormulaPhase::Open), FormulaPhase::Check);

    let smt_content = match translate_formula(formula_content, formula_source, &fctx, providers).await {
        Ok(smt) => smt,
        Err(_) => {
            warn!(%fctx, "Formula translation failed");
            pm.advance_formula(&fctx, Some(FormulaPhase::Check), FormulaPhase::ClosedUnknown);
            return Ok((formula, fctx, SolverOutcome::Unknown, String::new()));
        }
    };

    info!(%fctx, "IO: solver");
    let resp = providers
        .solver
            .invoke(SolverRequest {
                formula: smt_content.clone(),
                context_id: Box::new(fctx),
            })
            .await?;
    let fctx = *resp.context_id;
    let result = resp.value;
    info!(%fctx, outcome = ?result.outcome, "IO response: solver");

    let outcome = result.outcome.clone();

    pm.advance_formula(&fctx, Some(FormulaPhase::Check), solver_outcome_to_formula_phase(&outcome));

    Ok((formula, fctx, outcome, smt_content))
}

async fn fix_formula(
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
            })
            .await?;
    let fctx = *resp.context_id;
    info!(%fctx, "IO response: fixer");

    Ok((resp.value, fctx))
}

async fn translate_formula(
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
                })
                .await?;
            info!(%formula_ctx, "IO response: python translated");

            if resp.smtlib.is_empty() {
                anyhow::bail!("Python script produced no SMT output for ctx {}", formula_ctx);
            }
            Ok(resp.smtlib)
        }
    }
}

async fn check_and_fix_formula(
    ef: &crate::formula::ExtractedFormula,
    piece_ctx: &ContextId,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    iteration: u32,
    max_fix_attempts: usize,
    piece_ctx_info: &PieceContext,
) -> Result<(crate::formula::Formula, SolverOutcome, ContextId)> {
    let (mut formula, mut fctx, mut outcome, mut current_smt) = check_formula(
        &ef.content, &ef.source, piece_ctx, providers, pm, iteration,
    ).await?;

    let mut fix_attempts = 0;
    while matches!(outcome, SolverOutcome::Error(_)) && fix_attempts < max_fix_attempts {
        let error_msg = match &outcome {
            SolverOutcome::Error(e) => e.clone(),
            _ => unreachable!(),
        };

        info!(%fctx, fix_attempts, "Attempting to fix formula");

        let (fixed_response, fctx2) = fix_formula(&current_smt, &error_msg, fctx, &piece_ctx_info.called_functions, &piece_ctx_info.docs_section, providers).await?;
        fctx = fctx2;
        let fixed_extracted = crate::formula::extract_formulas_from_response(&fixed_response);

        if fixed_extracted.is_empty() {
            fix_attempts += 1;
            continue;
        }

        let fixed_ef = &fixed_extracted[0];

        let translated = match translate_formula(&fixed_ef.content, &fixed_ef.source, &fctx, providers).await {
            Ok(smt) => smt,
            Err(e) => {
                warn!(%fctx, fix_attempts, error = %e, "Failed to translate fixed formula");
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
            })
            .await?;
        fctx = *resp.context_id;
        let fixed_result = resp.value;
        info!(%fctx, outcome = ?fixed_result.outcome, "IO response: solver");

        outcome = fixed_result.outcome.clone();
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

async fn judge_piece(
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
            })
            .await?;

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
            })
            .await?;

        response = resp.value;
        ctx = *resp.context_id;
        info!(%ctx, "IO response: judge retry");
    }

    Ok((response, ctx))
}

async fn process_piece(
    piece: &ArcCodePiece,
    mut ctx: ContextId,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    verification_dir: &Path,
) -> Result<PieceOutcome> {
    assert!(piece.type_invariant());

    pm.expect_piece_phase_and_set(&ctx, &[CodePiecePhase::Open], CodePiecePhase::GetContext);

    let context = gather_context(piece, &ctx, providers).await?;
    info!(%ctx, "Gathered context for piece");

    let own_docs = pm.get_function_docs(piece.function_id());
    let own_docs_section = if own_docs.is_empty() {
        String::new()
    } else {
        format!("\nFunction documentation:\n{}\nNote: Safety comments describe preconditions that must hold when the function is called. Treat them as verification preconditions.", own_docs)
    };

    let mut elaboration = String::new();
    let max_judge_attempts = crate::consts::MAX_JUDGE_ATTEMPTS;

    for judge_attempt in 0..max_judge_attempts {
        pm.advance_piece(&ctx, Some(CodePiecePhase::GetContext), CodePiecePhase::Formalizer);

        let (formalizer_response, ctx2) = formalize_piece(piece, ctx, &context, &elaboration, &own_docs_section, providers).await?;
        ctx = ctx2;
        info!(%ctx, judge_attempt, "formalizer step done");

        let extracted = crate::formula::extract_formulas_from_response(&formalizer_response);
        if extracted.is_empty() {
            warn!(%ctx, judge_attempt, "No formulas extracted from formalizer response");
            elaboration = format!("Attempt {}: No formulas were extracted. Please produce SMT-LIB2 formulas in ```smt2 code blocks.\n{}", judge_attempt + 1, elaboration);
            pm.advance_piece(&ctx, Some(CodePiecePhase::Formalizer), CodePiecePhase::GetContext);
            continue;
        }

        pm.advance_piece(&ctx, Some(CodePiecePhase::Formalizer), CodePiecePhase::Check);

        let max_fix_attempts = crate::consts::MAX_INSIST_ATTEMPTS;
        let iteration = judge_attempt as u32;

        let piece_ctx_info = PieceContext {
            called_functions: context.clone(),
            docs_section: own_docs_section.clone(),
        };

        let formula_futs: Vec<_> = extracted.iter().map(|ef| {
            check_and_fix_formula(
                ef, &ctx, providers, pm,
                iteration, max_fix_attempts,
                &piece_ctx_info,
            )
        }).collect();

        let formula_results = futures::future::join_all(formula_futs).await;

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
            info!(%ctx, "IO: filesystem write formula");
            let _ = providers
                .filesystem
                .invoke(FileSystemRequest {
                    dir: verification_dir.to_path_buf(),
                    filename,
                    content: ef.content.clone(),
                })
                .await;
            info!(%ctx, "IO response: formula written");
        }

        info!(%ctx, "IO: git AddFiles");
        let _ = providers
            .git
            .invoke(GitRequest::AddFiles {
                paths: vec![verification_dir.to_path_buf()],
            })
            .await;
        info!(%ctx, "IO response: files added");
        info!(%ctx, "IO: git Commit");
        let _ = providers
            .git
            .invoke(GitRequest::Commit {
                message: format!("Add formulas for ctx {} (iteration {})", ctx, judge_attempt),
            })
            .await;
        info!(%ctx, "IO response: committed");

        pm.advance_piece(&ctx, Some(CodePiecePhase::Check), CodePiecePhase::Judge);

        let formulas_summary = formula_summaries.join("\n");
        let (judge_response, ctx2) = judge_piece(piece, ctx, &formulas_summary, &piece_ctx_info, providers).await?;
        ctx = ctx2;
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
            pm.advance_piece(&ctx, Some(CodePiecePhase::Judge), CodePiecePhase::Unverified);
            return Ok(PieceOutcome {
                closed: None,
                unverified: Some(unverified_from_piece(
                    piece,
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
            pm.advance_piece(&ctx, Some(CodePiecePhase::Judge), CodePiecePhase::GetContext);
        } else {
            break;
        }
    }

    pm.advance_piece(&ctx, Some(CodePiecePhase::Judge), CodePiecePhase::Unverified);
    Ok(PieceOutcome {
        closed: None,
        unverified: Some(unverified_from_piece(
            piece,
            Vec::new(),
            Vec::new(),
            format!("Exhausted {} judge attempts. Last elaboration: {}", max_judge_attempts, elaboration),
        )),
    })
}

#[async_trait]
impl AlgorithmState for FullFormalizer {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let functions_count = self.functions_count();
        let total_pieces = self.pieces.len();
        let verification_dir = self.verification_dir;
        let project_path = self.project_path;
        let branch_name = self.branch_name;

        let mut pieces_only = Vec::new();
        let mut ctxs_only = Vec::new();
        for (piece, ctx) in self.pieces {
            pieces_only.push(piece);
            ctxs_only.push(ctx);
        }

        let ctx_labels: Vec<String> = ctxs_only.iter().map(|c| c.to_string()).collect();

        let futs: Vec<_> = pieces_only.iter().zip(ctxs_only).map(|(piece, ctx)| {
            process_piece(piece, ctx, providers, pm, &verification_dir)
        }).collect();

        let results = futures::future::join_all(futs).await;

        let mut closed_pieces = Vec::new();
        let mut unverified_pieces = Vec::new();

        for (i, result) in results.into_iter().enumerate() {
            match result {
                Ok(outcome) => {
                    if let Some(cp) = outcome.closed {
                        closed_pieces.push(cp);
                    }
                    if let Some(up) = outcome.unverified {
                        unverified_pieces.push(up);
                    }
                }
                Err(e) => {
                    let piece = &pieces_only[i];
                    warn!(ctx = %ctx_labels[i], error = %e, "Failed to process piece");
                    unverified_pieces.push(unverified_from_piece(
                        piece,
                        Vec::new(),
                        Vec::new(),
                        format!("Processing error: {}", e),
                    ));
                }
            }
        }

        if unverified_pieces.is_empty() {
            Ok(Step::Result(VerificationResult {
                closed_pieces,
                unverified_pieces,
                bug_reports: Vec::new(),
                total_functions: functions_count,
                total_pieces,
            }))
        } else {
            Ok(Step::State(Box::new(ProblemAnalyzer {
                project_path,
                verification_dir,
                branch_name,
                closed: closed_pieces,
                unverified: unverified_pieces,
                recheck_files: Vec::new(),
                base_commit: String::new(),
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
    recheck_files: Vec<PathBuf>,
    base_commit: String,
}

#[async_trait]
impl AlgorithmState for ProblemAnalyzer {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        info!(
            unverified_count = self.unverified.len(),
            closed_count = self.closed.len(),
            "ProblemAnalyzer starting",
        );

        let mut bug_reports = Vec::new();
        let mut remaining_unverified = Vec::new();
        let mut files_to_recheck = self.recheck_files.clone();

        let base_commit = if self.base_commit.is_empty() {
            info!("IO: git CurrentCommitHash");
            let resp = providers.git.invoke(GitRequest::CurrentCommitHash).await?;
            resp.output.trim().to_string()
        } else {
            self.base_commit.clone()
        };
        info!(%base_commit, "IO response: git CurrentCommitHash");

        for (i, piece) in self.unverified.iter().enumerate() {
            info!(
                piece_index = i,
                total = self.unverified.len(),
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
                .await
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
                    working_directory: self.project_path.clone(),
                    files_to_read: vec![piece.file.clone()],
                })
                .await?;
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
                    .await;
                info!("IO response: files added");
                info!("IO: git Commit");
                let _ = providers
                    .git
                    .invoke(GitRequest::Commit {
                        message: format!("Agent fix for {} at {}:{}", piece.function_id.display_name(), piece.file.display(), piece.start_line),
                    })
                    .await;
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
            Ok(Step::Result(VerificationResult {
                closed_pieces: self.closed,
                unverified_pieces: Vec::new(),
                bug_reports,
                total_functions: 0,
                total_pieces: 0,
            }))
        } else {
            Ok(Step::State(Box::new(Restarter {
                project_path: self.project_path,
                verification_dir: self.verification_dir,
                branch_name: self.branch_name,
                base_commit,
                recheck_files: files_to_recheck,
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
}

#[async_trait]
impl AlgorithmState for Restarter {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        info!("IO: git FindChangedRustFiles");
        let resp = providers
            .git
            .invoke(GitRequest::FindChangedRustFiles {
                base_commit: self.base_commit.clone(),
            })
            .await?;

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

        Ok(Step::State(Box::new(FunctionLister {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            files: all_files,
        })))
    }
}
