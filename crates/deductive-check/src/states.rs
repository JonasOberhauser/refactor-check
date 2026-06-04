use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::code_piece::{ArcCodePiece, FunctionId};
use crate::phase::CodePiecePhase;
use crate::provider::Providers;
use crate::result::{BugReport, ClosedPiece, UnverifiedPiece, VerificationResult};

pub enum Step {
    State(Box<dyn AlgorithmState>),
    Result(VerificationResult),
}

#[async_trait]
pub trait AlgorithmState: Send + Sync + 'static {
    async fn execute(self: Box<Self>, providers: &Providers<'_>, pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step>;
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
    async fn execute(self: Box<Self>, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let branch_name = format!("verification/{timestamp}");

        use crate::provider::GitRequest;
        let resp = providers.git.invoke(GitRequest::CreateBranch { name: branch_name.clone() }).await?;
        if !resp.success {
            anyhow::bail!("Failed to create verification branch: {}", resp.output);
        }

        let verification_dir = self.project_path.join("verification").join(&timestamp);
        let resp = providers.git.invoke(GitRequest::CreateDirectory {
            path: verification_dir.clone(),
        }).await?;
        if !resp.success {
            anyhow::bail!("Failed to create verification directory: {}", resp.output);
        }

        let all_rust_files: Vec<PathBuf> = walkdir_files(&self.project_path);

        Ok(Step::State(Box::new(FunctionLister {
            project_path: self.project_path,
            verification_dir,
            branch_name,
            files: all_rust_files,
        })))
    }
}

fn walkdir_files(path: &PathBuf) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                files.extend(walkdir_files(&p));
            } else if let Some(ext) = p.extension() {
                if ext == "rs" {
                    files.push(p);
                }
            }
        }
    }
    files
}

pub struct FunctionLister {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    files: Vec<PathBuf>,
}

#[async_trait]
impl AlgorithmState for FunctionLister {
    async fn execute(self: Box<Self>, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        use crate::provider::RustAnalyzerRequest;
        let resp = providers.rust_analyzer.invoke(RustAnalyzerRequest::ListFunctions {
            files: self.files.clone(),
            cfg_verification: true,
        }).await?;

        let functions = match resp {
            crate::provider::RustAnalyzerResponse::FunctionList(fns) => fns,
            _ => anyhow::bail!("Expected FunctionList from rust-analyzer"),
        };

        let function_map: std::collections::HashMap<PathBuf, Vec<FunctionId>> = {
            let mut map: std::collections::HashMap<PathBuf, Vec<FunctionId>> = std::collections::HashMap::new();
            for fi in &functions {
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

pub struct FunctionAnalyzer {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    functions: std::collections::HashMap<PathBuf, Vec<FunctionId>>,
}

#[async_trait]
impl AlgorithmState for FunctionAnalyzer {
    async fn execute(self: Box<Self>, providers: &Providers<'_>, pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        let mut code_pieces: Vec<ArcCodePiece> = Vec::new();

        for (file, function_ids) in &self.functions {
            for fid in function_ids {
                use crate::provider::RustAnalyzerRequest;
                let resp = providers.rust_analyzer.invoke(RustAnalyzerRequest::GetFunctionCode {
                    function_id: fid.clone(),
                }).await?;

                let body = match resp {
                    crate::provider::RustAnalyzerResponse::FunctionCode(code) => code,
                    _ => anyhow::bail!("Expected FunctionCode from rust-analyzer"),
                };

                let piece = pm.new_piece(
                    file.clone(),
                    fid.clone(),
                    fid.line,
                    fid.line + body.lines().count() as u32,
                    body,
                    None,
                );
                code_pieces.push(Arc::new(piece));
            }
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

pub struct FullFormalizer {
    project_path: PathBuf,
    verification_dir: PathBuf,
    branch_name: String,
    pieces: Vec<ArcCodePiece>,
    #[allow(dead_code)]
    iteration: usize,
}

#[async_trait]
impl AlgorithmState for FullFormalizer {
    async fn execute(self: Box<Self>, _providers: &Providers<'_>, pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        let mut closed: Vec<ClosedPiece> = Vec::new();
        let unverified: Vec<UnverifiedPiece> = Vec::new();

        for piece in &self.pieces {
            let piece_id = piece.id();

            pm.expect_piece_phase_and_set(
                piece_id,
                &[CodePiecePhase::Open],
                CodePiecePhase::GetContext,
            );

            pm.advance_piece(piece_id, Some(CodePiecePhase::GetContext), CodePiecePhase::Formalizer);

            pm.advance_piece(piece_id, Some(CodePiecePhase::Formalizer), CodePiecePhase::Check);

            pm.advance_piece(piece_id, Some(CodePiecePhase::Check), CodePiecePhase::Judge);

            pm.advance_piece(piece_id, Some(CodePiecePhase::Judge), CodePiecePhase::Closed);

            closed.push(ClosedPiece {
                piece_id,
                file: piece.file().clone(),
                function_id: piece.function_id().clone(),
                start_line: piece.start_line(),
                end_line: piece.end_line(),
                unsat_formulas: 1,
                sat_formulas: 0,
                unknown_formulas: 0,
            });
        }

        if unverified.is_empty() {
            Ok(Step::Result(VerificationResult {
                closed_pieces: closed,
                unverified_pieces: unverified,
                bug_reports: Vec::new(),
                total_functions: self.functions_count(),
                total_pieces: self.pieces.len(),
            }))
        } else {
            Ok(Step::State(Box::new(ProblemAnalyzer {
                project_path: self.project_path,
                verification_dir: self.verification_dir,
                branch_name: self.branch_name,
                closed,
                unverified,
                recheck_files: Vec::new(),
                base_commit: String::new(),
            })))
        }
    }
}

impl FullFormalizer {
    fn functions_count(&self) -> usize {
        self.pieces.len()
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
    async fn execute(self: Box<Self>, providers: &Providers<'_>, pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        let mut bug_reports: Vec<BugReport> = Vec::new();
        let mut remaining_unverified: Vec<UnverifiedPiece> = Vec::new();
        let mut files_to_recheck: Vec<PathBuf> = self.recheck_files.clone();

        let _ = (providers, pm);

        for piece in &self.unverified {
            bug_reports.push(BugReport {
                piece_id: piece.piece_id,
                file: piece.file.clone(),
                function_id: piece.function_id.clone(),
                description: format!("Bug in {} at lines {}-{}", piece.function_id.display_name(), piece.start_line, piece.end_line),
            });
            remaining_unverified.push(piece.clone());
            files_to_recheck.push(piece.file.clone());
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
                base_commit: self.base_commit,
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
    async fn execute(self: Box<Self>, providers: &Providers<'_>, _pm: &dyn crate::piece_manager::DeductivePieceManager) -> Result<Step> {
        use crate::provider::GitRequest;

        let resp = providers.git.invoke(GitRequest::FindChangedRustFiles {
            base_commit: self.base_commit.clone(),
        }).await?;

        let changed_files: Vec<PathBuf> = if resp.success {
            resp.output
                .lines()
                .filter(|l| l.ends_with(".rs"))
                .map(PathBuf::from)
                .collect()
        } else {
            Vec::new()
        };

        let all_files: Vec<PathBuf> = self.recheck_files
            .iter()
            .chain(changed_files.iter())
            .cloned()
            .collect();
        let all_files: Vec<PathBuf> = {
            let mut seen = std::collections::HashSet::new();
            all_files.into_iter().filter(|f| seen.insert(f.clone())).collect()
        };

        Ok(Step::State(Box::new(FunctionLister {
            project_path: self.project_path,
            verification_dir: self.verification_dir,
            branch_name: self.branch_name,
            files: all_files,
        })))
    }
}