use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tracing::{info, warn};

use crate::code_piece::{ArcCodePiece, FunctionId};
use crate::formula::FormulaSource;
use crate::phase::{CodePiecePhase, FormulaPhase};
use crate::prompts;
use crate::provider::{FileSystemRequest, GitRequest, PythonRequest, RustAnalyzerRequest, RustAnalyzerResponse, Providers, FunctionInfo};
use crate::result::{BugReport, ClosedPiece, UnverifiedPiece, VerificationResult};
use refactor_check_core::provider::{LlmRequest, LlmRole, SolverRequest};
use refactor_check_core::smt::SolverOutcome;

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

#[async_trait]
impl AlgorithmState for Initializer {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let branch_name = format!("verification/{timestamp}");

        let resp = providers
            .git
            .invoke(GitRequest::CreateBranch {
                name: branch_name.clone(),
            })
            .await?;
        if !resp.success {
            anyhow::bail!("Failed to create verification branch: {}", resp.output);
        }

        let verification_dir = self.project_path.join("verification").join(&timestamp);
        let resp = providers
            .git
            .invoke(GitRequest::CreateDirectory {
                path: verification_dir.clone(),
            })
            .await?;
        if !resp.success {
            anyhow::bail!("Failed to create verification directory: {}", resp.output);
        }

        let resp = providers
            .git
            .invoke(GitRequest::WalkRustFiles {
                path: self.project_path.clone(),
            })
            .await?;
        let all_rust_files: Vec<PathBuf> = resp
            .output
            .lines()
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect();

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
        let resp = providers
            .rust_analyzer
            .invoke(RustAnalyzerRequest::ListFunctions {
                files: self.files.clone(),
                cfg_verification: true,
            })
            .await?;

        let functions = match resp {
            RustAnalyzerResponse::FunctionList(fns) => fns,
            _ => anyhow::bail!("Expected FunctionList from rust-analyzer"),
        };

        let target_file_set: HashSet<PathBuf> = self.files.iter().cloned().collect();
        let filtered = filter_connected_functions(functions, &target_file_set);

        let function_map: std::collections::HashMap<PathBuf, Vec<FunctionId>> = {
            let mut map: std::collections::HashMap<PathBuf, Vec<FunctionId>> = std::collections::HashMap::new();
            for fi in &filtered {
                map.entry(fi.id.file.clone()).or_default().push(fi.id.clone());
            }
            map
        };

        Ok(Step::State(Box::new(FunctionAnalyzer {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            functions: function_map,
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
}

#[async_trait]
impl AlgorithmState for FunctionAnalyzer {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        _pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let mut function_bodies: Vec<(PathBuf, FunctionId, String)> = Vec::new();

        for (file, function_ids) in &self.functions {
            for fid in function_ids {
                let resp = providers
                    .rust_analyzer
                    .invoke(RustAnalyzerRequest::GetFunctionCode {
                        function_id: fid.clone(),
                    })
                    .await?;

                let body = match resp {
                    RustAnalyzerResponse::FunctionCode(code) => code,
                    _ => anyhow::bail!("Expected FunctionCode from rust-analyzer"),
                };

                function_bodies.push((file.clone(), fid.clone(), body));
            }
        }

        Ok(Step::State(Box::new(Splitter {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            function_bodies,
        })))
    }
}

pub struct Splitter {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    function_bodies: Vec<(PathBuf, FunctionId, String)>,
}

#[async_trait]
impl AlgorithmState for Splitter {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let mut code_pieces: Vec<ArcCodePiece> = Vec::new();

        for (file, fid, body) in &self.function_bodies {
            let pieces = split_function(providers, pm, file, fid, body).await?;
            code_pieces.extend(pieces);
        }

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
    file: &Path,
    fid: &FunctionId,
    body: &str,
) -> Result<Vec<ArcCodePiece>> {
    let messages = vec![
        crate::llm::system_message(&prompts::splitter_system()),
        crate::llm::user_message(&prompts::splitter_user(
            &file.display().to_string(),
            &fid.display_name(),
            body,
            fid.line,
        )),
    ];

    let response = providers
        .llm
        .invoke(LlmRequest {
            role: LlmRole::Splitter,
            messages,
            piece_id: None,
        })
        .await?;

    let pieces = parse_split_pieces(response, file, fid, body, pm);
    if pieces.is_empty() {
        let piece = pm.new_piece(
            file.to_path_buf(),
            fid.clone(),
            fid.line,
            fid.line + body.lines().count() as u32,
            body.to_string(),
        );
        return Ok(vec![Arc::new(piece)]);
    }

    Ok(pieces)
}

fn parse_split_pieces(
    response: String,
    file: &Path,
    fid: &FunctionId,
    original_body: &str,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
) -> Vec<ArcCodePiece> {
    let mut pieces = Vec::new();
    let mut current_lines: Vec<String> = Vec::new();
    let mut current_start: u32 = fid.line;

    for line in response.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("// Start point:") {
            if !current_lines.is_empty() {
                let code = current_lines.join("\n");
                let end_line = current_start + code.lines().count() as u32;
                let piece = pm.new_piece(
                    file.to_path_buf(),
                    fid.clone(),
                    current_start,
                    end_line,
                    code,
                );
                pieces.push(Arc::new(piece));
                current_lines.clear();
            }
            if let Some(pos_str) = trimmed.strip_prefix("// Start point:") {
                if let Some(lineno) = parse_line_number(pos_str.trim()) {
                    current_start = lineno;
                }
            }
            continue;
        }
        if trimmed.starts_with("// Handover point:") {
            current_lines.push(line.to_string());
            continue;
        }
        if !trimmed.is_empty() || !current_lines.is_empty() {
            current_lines.push(line.to_string());
        }
    }

    if !current_lines.is_empty() {
        let code = current_lines.join("\n");
        let end_line = current_start + code.lines().count() as u32;
        let piece = pm.new_piece(
            file.to_path_buf(),
            fid.clone(),
            current_start,
            end_line,
            code,
        );
        pieces.push(Arc::new(piece));
    }

    if pieces.is_empty() {
        let piece = pm.new_piece(
            file.to_path_buf(),
            fid.clone(),
            fid.line,
            fid.line + original_body.lines().count() as u32,
            original_body.to_string(),
        );
        pieces.push(Arc::new(piece));
    }

    pieces
}

fn parse_line_number(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(idx) = s.rfind(':') {
        s[idx + 1..].parse().ok()
    } else if let Some(idx) = s.rfind(' ') {
        s[idx + 1..].parse().ok()
    } else {
        s.parse().ok()
    }
}

pub struct FullFormalizer {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    pieces: Vec<ArcCodePiece>,
    #[allow(dead_code)]
    iteration: usize,
}

impl FullFormalizer {
    fn functions_count(&self) -> usize {
        let mut seen = HashSet::new();
        for piece in &self.pieces {
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
    providers: &Providers<'_>,
) -> Result<String> {
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

    let mut context_parts = Vec::new();

    for cf in &called_functions {
        let called_name = cf.name.clone();
        let resp = providers
            .rust_analyzer
            .invoke(RustAnalyzerRequest::GetCalledFunctionCode {
                function_id: piece.function_id().clone(),
                called_name: called_name.clone(),
            })
            .await?;

        if let RustAnalyzerResponse::CalledFunctionCode(called_code) = resp {
            if !called_code.starts_with("// Could not find") {
                context_parts.push(format!("--- {} ---\n{}", called_name, called_code));
            }
        }
    }

    if context_parts.is_empty() {
        Ok("(no called functions found)".to_string())
    } else {
        Ok(context_parts.join("\n\n"))
    }
}

async fn formalize_piece(
    piece: &ArcCodePiece,
    context: &str,
    elaboration: &str,
    providers: &Providers<'_>,
) -> Result<String> {
    let messages = vec![
        crate::llm::system_message(&prompts::formalizer_system()),
        crate::llm::user_message(&prompts::formalizer_user(
            piece.code(),
            context,
            None,
            elaboration,
        )),
    ];

    let response = providers
        .llm
        .invoke(LlmRequest {
            role: LlmRole::Formalizer,
            messages,
            piece_id: Some(piece.id()),
        })
        .await?;

    Ok(response)
}

async fn check_formula(
    formula_content: &str,
    formula_source: &FormulaSource,
    piece_id: u64,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    iteration: u32,
) -> Result<(crate::formula::Formula, SolverOutcome)> {
    let formula = pm.new_formula(piece_id, formula_content.to_string(), formula_source.clone(), iteration);

    pm.advance_formula(formula.id(), Some(FormulaPhase::Open), FormulaPhase::Check);

    let smt_content = match formula_source {
        FormulaSource::PyZ3 => {
            let resp = providers
                .python
                .invoke(PythonRequest {
                    script: formula_content.to_string(),
                })
                .await?;

            if resp.smtlib.is_empty() {
                warn!(piece_id, formula_id = formula.id(), "Python script produced no SMT output");
                pm.advance_formula(formula.id(), Some(FormulaPhase::Check), FormulaPhase::ClosedUnknown);
                return Ok((formula, SolverOutcome::Unknown));
            }
            resp.smtlib
        }
        FormulaSource::SmtLib => formula_content.to_string(),
    };

    let result = providers
        .solver
        .invoke(SolverRequest {
            formula: smt_content.clone(),
            piece_id: Some(piece_id),
        })
        .await?;

    let outcome = result.outcome.clone();

    match &outcome {
        SolverOutcome::Unsat => {
            pm.advance_formula(formula.id(), Some(FormulaPhase::Check), FormulaPhase::ClosedUnsat);
        }
        SolverOutcome::Sat => {
            pm.advance_formula(formula.id(), Some(FormulaPhase::Check), FormulaPhase::ClosedSat);
        }
        SolverOutcome::Unknown => {
            pm.advance_formula(formula.id(), Some(FormulaPhase::Check), FormulaPhase::ClosedUnknown);
        }
        SolverOutcome::Error(_) => {
            pm.advance_formula(formula.id(), Some(FormulaPhase::Check), FormulaPhase::Fix);
        }
    }

    Ok((formula, outcome))
}

async fn fix_formula(
    formula_content: &str,
    error: &str,
    piece_id: u64,
    providers: &Providers<'_>,
) -> Result<String> {
    let messages = vec![
        crate::llm::system_message(&prompts::fixer_system()),
        crate::llm::user_message(&prompts::fixer_user(formula_content, error)),
    ];

    let response = providers
        .llm
        .invoke(LlmRequest {
            role: LlmRole::Fixer,
            messages,
            piece_id: Some(piece_id),
        })
        .await?;

    Ok(response)
}

async fn judge_piece(
    piece: &ArcCodePiece,
    formulas_summary: &str,
    providers: &Providers<'_>,
) -> Result<String> {
    let messages = vec![
        crate::llm::system_message(&prompts::judge_system()),
        crate::llm::user_message(&prompts::judge_user(piece.code(), formulas_summary)),
    ];

    let response = providers
        .llm
        .invoke(LlmRequest {
            role: LlmRole::Judge,
            messages,
            piece_id: Some(piece.id()),
        })
        .await?;

    Ok(response)
}

async fn process_piece(
    piece: &ArcCodePiece,
    providers: &Providers<'_>,
    pm: &dyn crate::piece_manager::DeductivePieceManager,
    verification_dir: &Path,
) -> Result<PieceOutcome> {
    let piece_id = piece.id();

    pm.expect_piece_phase_and_set(piece_id, &[CodePiecePhase::Open], CodePiecePhase::GetContext);

    let context = gather_context(piece, providers).await?;
    info!(piece_id, "Gathered context for piece");

    let mut elaboration = String::new();
    let max_judge_attempts = crate::consts::MAX_JUDGE_ATTEMPTS;
    let max_fix_attempts = crate::consts::MAX_INSIST_ATTEMPTS;

    for judge_attempt in 0..max_judge_attempts {
        pm.advance_piece(piece_id, Some(CodePiecePhase::GetContext), CodePiecePhase::Formalizer);

        let formalizer_response = formalize_piece(piece, &context, &elaboration, providers).await?;
        info!(piece_id, judge_attempt, "Formalizer responded");

        let extracted = crate::formula::extract_formulas_from_response(&formalizer_response);
        if extracted.is_empty() {
            warn!(piece_id, judge_attempt, "No formulas extracted from formalizer response");
            elaboration = format!("Attempt {}: No formulas were extracted. Please produce SMT-LIB2 formulas in ```smt2 code blocks.\n{}", judge_attempt + 1, elaboration);
            pm.advance_piece(piece_id, Some(CodePiecePhase::Formalizer), CodePiecePhase::GetContext);
            continue;
        }

        pm.advance_piece(piece_id, Some(CodePiecePhase::Formalizer), CodePiecePhase::Check);

        let mut unsat_count = 0usize;
        let mut sat_models = Vec::new();
        let mut unknown_formulas = Vec::new();
        let mut formula_summaries = Vec::new();

        for ef in &extracted {
            let iteration = judge_attempt as u32;
            let (formula, mut outcome) = check_formula(
                &ef.content,
                &ef.source,
                piece_id,
                providers,
                pm,
                iteration,
            )
            .await?;

            let mut fix_attempts = 0;
            while matches!(outcome, SolverOutcome::Error(_)) && fix_attempts < max_fix_attempts {
                let error_msg = match &outcome {
                    SolverOutcome::Error(e) => e.clone(),
                    _ => unreachable!(),
                };

                info!(piece_id, formula_id = formula.id(), fix_attempts, "Attempting to fix formula");

                pm.expect_formula_phase_and_set(
                    formula.id(),
                    &[FormulaPhase::Fix],
                    FormulaPhase::Check,
                );

                let fixed_response = fix_formula(&ef.content, &error_msg, piece_id, providers).await?;
                let fixed_extracted = crate::formula::extract_formulas_from_response(&fixed_response);

                if fixed_extracted.is_empty() {
                    fix_attempts += 1;
                    continue;
                }

                let fixed_ef = &fixed_extracted[0];

                let fixed_result = providers
                    .solver
                    .invoke(SolverRequest {
                        formula: fixed_ef.content.clone(),
                        piece_id: Some(piece_id),
                    })
                    .await?;

                outcome = fixed_result.outcome.clone();
                fix_attempts += 1;

                match &outcome {
                    SolverOutcome::Unsat => {
                        pm.advance_formula(formula.id(), Some(FormulaPhase::Check), FormulaPhase::ClosedUnsat);
                        break;
                    }
                    SolverOutcome::Sat => {
                        pm.advance_formula(formula.id(), Some(FormulaPhase::Check), FormulaPhase::ClosedSat);
                        break;
                    }
                    SolverOutcome::Unknown => {
                        pm.advance_formula(formula.id(), Some(FormulaPhase::Check), FormulaPhase::ClosedUnknown);
                        break;
                    }
                    SolverOutcome::Error(_) => {
                        pm.advance_formula(formula.id(), Some(FormulaPhase::Check), FormulaPhase::Fix);
                        continue;
                    }
                }
            }

            let summary = format!(
                "Formula {} (iteration {}): {:?}",
                formula.id(),
                formula.iteration(),
                outcome
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
            let filename = format!("piece{}_formula{}_iter{}_attempt{}.{}", piece_id, formula.id(), iteration, judge_attempt, ext);
            let _ = providers
                .filesystem
                .invoke(FileSystemRequest {
                    dir: verification_dir.to_path_buf(),
                    filename,
                    content: ef.content.clone(),
                })
                .await;
        }

        let _ = providers
            .git
            .invoke(GitRequest::AddFiles {
                paths: vec![verification_dir.to_path_buf()],
            })
            .await;
        let _ = providers
            .git
            .invoke(GitRequest::Commit {
                message: format!("Add formulas for piece {} (iteration {})", piece_id, judge_attempt),
            })
            .await;

        pm.advance_piece(piece_id, Some(CodePiecePhase::Check), CodePiecePhase::Judge);

        let formulas_summary = formula_summaries.join("\n");
        let judge_response = judge_piece(piece, &formulas_summary, providers).await?;
        info!(piece_id, judge_attempt, "Judge responded");

        let is_reasonable = judge_response
            .trim()
            .to_uppercase()
            .starts_with(crate::consts::JUDGE_REASONABLE);

        if is_reasonable {
            if sat_models.is_empty() && unknown_formulas.is_empty() {
                pm.advance_piece(piece_id, Some(CodePiecePhase::Judge), CodePiecePhase::Closed);
                return Ok(PieceOutcome {
                    closed: Some(ClosedPiece {
                        piece_id,
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
            pm.advance_piece(piece_id, Some(CodePiecePhase::Judge), CodePiecePhase::Unverified);
            return Ok(PieceOutcome {
                closed: None,
                unverified: Some(UnverifiedPiece {
                    piece_id,
                    file: piece.file().clone(),
                    function_id: piece.function_id().clone(),
                    start_line: piece.start_line(),
                    end_line: piece.end_line(),
                    sat_models,
                    unknown_formulas,
                    elaboration: judge_response,
                }),
            });
        }

        elaboration = format!(
            "Judge attempt {} feedback: {}\n\nPrevious problems to avoid when reformalizing:\n{}",
            judge_attempt + 1,
            judge_response,
            elaboration
        );
        pm.advance_piece(piece_id, Some(CodePiecePhase::Judge), CodePiecePhase::GetContext);
    }

    pm.advance_piece(piece_id, Some(CodePiecePhase::Judge), CodePiecePhase::Unverified);
    Ok(PieceOutcome {
        closed: None,
        unverified: Some(UnverifiedPiece {
            piece_id,
            file: piece.file().clone(),
            function_id: piece.function_id().clone(),
            start_line: piece.start_line(),
            end_line: piece.end_line(),
            sat_models: Vec::new(),
            unknown_formulas: Vec::new(),
            elaboration: format!("Exhausted {} judge attempts. Last elaboration: {}", max_judge_attempts, elaboration),
        }),
    })
}

#[async_trait]
impl AlgorithmState for FullFormalizer {
    async fn execute(
        self: Box<Self>,
        providers: &Providers<'_>,
        pm: &dyn crate::piece_manager::DeductivePieceManager,
    ) -> Result<Step> {
        let mut closed_pieces = Vec::new();
        let mut unverified_pieces = Vec::new();

        for piece in &self.pieces {
            match process_piece(piece, providers, pm, &self.verification_dir).await {
                Ok(outcome) => {
                    if let Some(cp) = outcome.closed {
                        closed_pieces.push(cp);
                    }
                    if let Some(up) = outcome.unverified {
                        unverified_pieces.push(up);
                    }
                }
                Err(e) => {
                    warn!(piece_id = piece.id(), error = %e, "Failed to process piece");
                    unverified_pieces.push(UnverifiedPiece {
                        piece_id: piece.id(),
                        file: piece.file().clone(),
                        function_id: piece.function_id().clone(),
                        start_line: piece.start_line(),
                        end_line: piece.end_line(),
                        sat_models: Vec::new(),
                        unknown_formulas: Vec::new(),
                        elaboration: format!("Processing error: {}", e),
                    });
                }
            }
        }

        if unverified_pieces.is_empty() {
            Ok(Step::Result(VerificationResult {
                closed_pieces,
                unverified_pieces,
                bug_reports: Vec::new(),
                total_functions: self.functions_count(),
                total_pieces: self.pieces.len(),
            }))
        } else {
            Ok(Step::State(Box::new(ProblemAnalyzer {
                project_path: self.project_path,
                verification_dir: self.verification_dir,
                branch_name: self.branch_name,
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
        let mut bug_reports = Vec::new();
        let mut remaining_unverified = Vec::new();
        let mut files_to_recheck = self.recheck_files.clone();

        for piece in &self.unverified {
            let messages = vec![
                crate::llm::system_message(&prompts::analyzer_system()),
                crate::llm::user_message(&prompts::analyzer_user(
                    &format!("{}:{}", piece.file.display(), piece.start_line),
                    &piece.sat_models,
                    &piece.unknown_formulas,
                    &piece.elaboration,
                )),
            ];

            let response = providers
                .llm
                .invoke(LlmRequest {
                    role: LlmRole::Analyzer,
                    messages,
                    piece_id: Some(piece.piece_id),
                })
                .await?;

            let is_retry = response.trim().to_uppercase().starts_with("RETRY");

            if is_retry {
                remaining_unverified.push(piece.clone());
                files_to_recheck.push(piece.file.clone());
            } else {
                bug_reports.push(BugReport {
                    piece_id: piece.piece_id,
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
            let base_commit = if self.base_commit.is_empty() {
                let resp = providers.git.invoke(GitRequest::CurrentCommitHash).await?;
                resp.output.trim().to_string()
            } else {
                self.base_commit.clone()
            };

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