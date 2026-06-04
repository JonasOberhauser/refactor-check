use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use refactor_check_core::provider::{IOProvider, LlmRequest, SolverRequest};
use refactor_check_core::smt::SolverResult;

use crate::code_piece::FunctionId;

pub type DynLlmProvider = dyn IOProvider<LlmRequest, String>;
pub type DynSolverProvider = dyn IOProvider<SolverRequest, SolverResult>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FunctionInfo {
    pub id: FunctionId,
    pub body: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CodePieceInfo {
    pub file: PathBuf,
    pub function_id: FunctionId,
    pub start_line: u32,
    pub end_line: u32,
    pub code: String,
}

#[derive(Debug, Clone)]
pub enum RustAnalyzerRequest {
    ListFunctions {
        files: Vec<PathBuf>,
        cfg_verification: bool,
    },
    GetFunctionCode {
        function_id: FunctionId,
    },
    GetCalledFunctionCode {
        function_id: FunctionId,
        called_name: String,
    },
}

#[derive(Debug, Clone)]
pub enum RustAnalyzerResponse {
    FunctionList(Vec<FunctionInfo>),
    FunctionCode(String),
    CalledFunctionCode(String),
}

pub type DynRustAnalyzerProvider = dyn IOProvider<RustAnalyzerRequest, RustAnalyzerResponse>;

#[derive(Debug, Clone)]
pub enum GitRequest {
    CreateBranch {
        name: String,
    },
    Commit {
        message: String,
    },
    AddFiles {
        paths: Vec<PathBuf>,
    },
    FindChangedRustFiles {
        base_commit: String,
    },
    CurrentCommitHash,
    CreateDirectory {
        path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct GitResponse {
    pub success: bool,
    pub output: String,
}

pub type DynGitProvider = dyn IOProvider<GitRequest, GitResponse>;

#[derive(Debug, Clone)]
pub struct FileSystemRequest {
    pub dir: PathBuf,
    pub filename: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct FileSystemResponse {
    pub path: PathBuf,
}

pub type DynFileSystemProvider = dyn IOProvider<FileSystemRequest, FileSystemResponse>;

#[derive(Debug, Clone)]
pub struct PythonRequest {
    pub script: String,
}

#[derive(Debug, Clone)]
pub struct PythonResponse {
    pub smtlib: String,
    pub explanation: String,
}

pub type DynPythonProvider = dyn IOProvider<PythonRequest, PythonResponse>;

pub struct Providers<'a> {
    pub llm: &'a DynLlmProvider,
    pub solver: &'a DynSolverProvider,
    pub rust_analyzer: &'a DynRustAnalyzerProvider,
    pub git: &'a DynGitProvider,
    pub filesystem: &'a DynFileSystemProvider,
    pub python: &'a DynPythonProvider,
}

pub struct CliRustAnalyzerProvider {
    path: String,
}

impl CliRustAnalyzerProvider {
    #[must_use]
    pub fn new(path: String) -> Self {
        Self { path }
    }
}

#[async_trait]
impl IOProvider<RustAnalyzerRequest, RustAnalyzerResponse> for CliRustAnalyzerProvider {
    async fn invoke(&self, _input: RustAnalyzerRequest) -> Result<RustAnalyzerResponse> {
        anyhow::bail!("CliRustAnalyzerProvider not yet implemented (path: {})", self.path)
    }
}

pub struct CliGitProvider;

impl CliGitProvider {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl IOProvider<GitRequest, GitResponse> for CliGitProvider {
    async fn invoke(&self, input: GitRequest) -> Result<GitResponse> {
        match input {
            GitRequest::CreateBranch { name } => {
                let output = tokio::process::Command::new("git")
                    .args(["checkout", "-b", &name])
                    .output()
                    .await?;
                Ok(GitResponse {
                    success: output.status.success(),
                    output: String::from_utf8_lossy(&output.stdout).to_string(),
                })
            }
            GitRequest::Commit { message } => {
                let output = tokio::process::Command::new("git")
                    .args(["commit", "-m", &message])
                    .output()
                    .await?;
                Ok(GitResponse {
                    success: output.status.success(),
                    output: String::from_utf8_lossy(&output.stdout).to_string(),
                })
            }
            GitRequest::AddFiles { paths } => {
                let mut args = vec!["add".to_string()];
                for p in &paths {
                    args.push(p.to_string_lossy().to_string());
                }
                let output = tokio::process::Command::new("git")
                    .args(&args)
                    .output()
                    .await?;
                Ok(GitResponse {
                    success: output.status.success(),
                    output: String::from_utf8_lossy(&output.stdout).to_string(),
                })
            }
            GitRequest::FindChangedRustFiles { base_commit } => {
                let output = tokio::process::Command::new("git")
                    .args(["diff", "--name-only", &base_commit])
                    .output()
                    .await?;
                Ok(GitResponse {
                    success: output.status.success(),
                    output: String::from_utf8_lossy(&output.stdout).to_string(),
                })
            }
            GitRequest::CurrentCommitHash => {
                let output = tokio::process::Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .await?;
                Ok(GitResponse {
                    success: output.status.success(),
                    output: String::from_utf8_lossy(&output.stdout).to_string(),
                })
            }
            GitRequest::CreateDirectory { path } => {
                std::fs::create_dir_all(&path)?;
                Ok(GitResponse {
                    success: true,
                    output: path.to_string_lossy().to_string(),
                })
            }
        }
    }
}

pub struct LocalFileSystemProvider;

impl LocalFileSystemProvider {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl IOProvider<FileSystemRequest, FileSystemResponse> for LocalFileSystemProvider {
    async fn invoke(&self, input: FileSystemRequest) -> Result<FileSystemResponse> {
        std::fs::create_dir_all(&input.dir)?;
        let path = input.dir.join(&input.filename);
        std::fs::write(&path, &input.content)?;
        Ok(FileSystemResponse { path })
    }
}

impl Default for CliGitProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for LocalFileSystemProvider {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ProcessPythonProvider;

impl ProcessPythonProvider {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for ProcessPythonProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl IOProvider<PythonRequest, PythonResponse> for ProcessPythonProvider {
    async fn invoke(&self, input: PythonRequest) -> Result<PythonResponse> {
        let output = tokio::process::Command::new("python3")
            .arg("-c")
            .arg(&input.script)
            .output()
            .await?;

        if !output.status.success() {
            anyhow::bail!(
                "Python script failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let smtlib = stdout.clone();
        let explanation = String::new();

        Ok(PythonResponse { smtlib, explanation })
    }
}