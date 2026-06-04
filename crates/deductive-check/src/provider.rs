use std::path::{Path, PathBuf};

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
pub struct CalledFunction {
    pub name: String,
    pub file: Option<PathBuf>,
    pub start_line: Option<u32>,
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
    GetCalledFunctions {
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
    CalledFunctionList(Vec<CalledFunction>),
    CalledFunctionCode(String),
}

pub type DynRustAnalyzerProvider = dyn IOProvider<RustAnalyzerRequest, RustAnalyzerResponse>;

#[derive(Debug, Clone)]
pub enum GitRequest {
    CreateBranch { name: String },
    Commit { message: String },
    AddFiles { paths: Vec<PathBuf> },
    FindChangedRustFiles { base_commit: String },
    CurrentCommitHash,
    CreateDirectory { path: PathBuf },
    WalkRustFiles { path: PathBuf },
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

pub struct CliRustAnalyzerProvider;

impl CliRustAnalyzerProvider {
    #[must_use]
    pub fn new(_path: String) -> Self {
        Self
    }
}

fn make_abs_path(path: &Path) -> triomphe::Arc<ra_ap_paths::AbsPathBuf> {
    let abs = ra_ap_paths::AbsPathBuf::assert_utf8(path.to_path_buf());
    triomphe::Arc::from(abs)
}

fn list_functions_in_file(file_path: &Path, cfg_verification: bool) -> Result<Vec<FunctionInfo>> {
    let mut source = std::fs::read_to_string(file_path)?;
    if cfg_verification {
        source = strip_cfg_attribute(&source, "verification");
    }
    let cwd = make_abs_path(file_path);
    let (analysis, file_id) = ra_ap_ide::Analysis::from_single_file(source.clone(), cwd);
    let config = ra_ap_ide::FileStructureConfig { exclude_locals: true };
    let structure = analysis.file_structure(&config, file_id)?;

    let mut functions = Vec::new();
    for node in &structure {
        if let ra_ap_ide::StructureNodeKind::SymbolKind(
            ra_ap_ide_db::SymbolKind::Function | ra_ap_ide_db::SymbolKind::Method,
        ) = node.kind
        {
            let start_line = u32::from(node.node_range.start()) + 1;
            let end_line = u32::from(node.node_range.end()) + 1;

            let impl_for = node.parent.and_then(|parent_idx| {
                structure.get(parent_idx).and_then(|parent| {
                    if let ra_ap_ide::StructureNodeKind::SymbolKind(ra_ap_ide_db::SymbolKind::Impl) = parent.kind {
                        let label = parent.label.trim();
                        label.strip_prefix("impl ").map(|s| {
                            if let Some(space) = s.find(" for ") {
                                s[..space].trim().to_string()
                            } else if let Some(brace) = s.find('<') {
                                s[..brace].trim().to_string()
                            } else if let Some(brace) = s.find('{') {
                                s[..brace].trim().to_string()
                            } else {
                                s.trim().to_string()
                            }
                        })
                    } else {
                        None
                    }
                })
            });

            let fid = FunctionId::new(
                file_path.to_path_buf(),
                &node.label,
                impl_for.as_deref(),
                start_line,
            );
            functions.push(FunctionInfo {
                id: fid,
                body: String::new(),
                start_line,
                end_line,
            });
        }
    }
    Ok(functions)
}

fn get_called_functions(file_path: &Path, function_id: &FunctionId) -> Result<Vec<CalledFunction>> {
    let source = std::fs::read_to_string(file_path)?;
    let cwd = make_abs_path(file_path);
    let (analysis, file_id) = ra_ap_ide::Analysis::from_single_file(source, cwd);

    let config = ra_ap_ide::CallHierarchyConfig {
        exclude_tests: false,
        ra_fixture: ra_ap_ide_db::ra_fixture::RaFixtureConfig::default(),
    };

    let offset = ra_ap_ide::TextSize::from(function_id.line.saturating_sub(1));
    let position = ra_ap_ide::FilePosition { file_id, offset };

    let mut called = Vec::new();

    if let Ok(Some(call_items)) = analysis.outgoing_calls(&config, position) {
        for item in call_items {
            let name = item.target.name.to_string();

            called.push(CalledFunction {
                name,
                file: None,
                start_line: Some(u32::from(item.target.full_range.start()) + 1),
            });
        }
    }

    called.sort_by(|a, b| a.name.cmp(&b.name));
    called.dedup_by(|a, b| a.name == b.name);
    Ok(called)
}

#[async_trait]
impl IOProvider<RustAnalyzerRequest, RustAnalyzerResponse> for CliRustAnalyzerProvider {
    async fn invoke(&self, input: RustAnalyzerRequest) -> Result<RustAnalyzerResponse> {
        match input {
            RustAnalyzerRequest::ListFunctions { files, cfg_verification } => {
                let mut all_functions = Vec::new();

                for file_path in &files {
                    match list_functions_in_file(file_path, cfg_verification) {
                        Ok(fns) => all_functions.extend(fns),
                        Err(_) => continue,
                    }
                }

                Ok(RustAnalyzerResponse::FunctionList(all_functions))
            }
            RustAnalyzerRequest::GetFunctionCode { function_id } => {
                let source = std::fs::read_to_string(&function_id.file)?;
                let cwd = make_abs_path(&function_id.file);
                let (analysis, file_id) = ra_ap_ide::Analysis::from_single_file(source.clone(), cwd);
                let config = ra_ap_ide::FileStructureConfig { exclude_locals: true };
                let structure = analysis.file_structure(&config, file_id)?;

                for node in &structure {
                    if let ra_ap_ide::StructureNodeKind::SymbolKind(
                        ra_ap_ide_db::SymbolKind::Function | ra_ap_ide_db::SymbolKind::Method,
                    ) = node.kind
                    {
                        let node_start = u32::from(node.node_range.start()) + 1;
                        if node_start == function_id.line && node.label == function_id.name {
                            let start = usize::from(node.node_range.start());
                            let end = usize::from(node.node_range.end());
                            if end <= source.len() && start <= end {
                                let body = source[start..end].to_string();
                                return Ok(RustAnalyzerResponse::FunctionCode(body));
                            }
                        }
                    }
                }

                let lines: Vec<&str> = source.lines().collect();
                let start = usize::try_from(function_id.line).unwrap_or(1).saturating_sub(1);
                let end = (start + 50).min(lines.len());
                let body = lines[start..end].join("\n");
                Ok(RustAnalyzerResponse::FunctionCode(body))
            }
            RustAnalyzerRequest::GetCalledFunctions { function_id } => {
                match get_called_functions(&function_id.file, &function_id) {
                    Ok(called) => Ok(RustAnalyzerResponse::CalledFunctionList(called)),
                    Err(_) => Ok(RustAnalyzerResponse::CalledFunctionList(Vec::new())),
                }
            }
            RustAnalyzerRequest::GetCalledFunctionCode { function_id, called_name } => {
                let source = match std::fs::read_to_string(&function_id.file) {
                    Ok(s) => s,
                    Err(_) => {
                        return Ok(RustAnalyzerResponse::CalledFunctionCode(format!(
                            "// Could not read file: {:?}", function_id.file
                        )));
                    }
                };

                let cwd = make_abs_path(&function_id.file);
                let (analysis, file_id) =
                    ra_ap_ide::Analysis::from_single_file(source.clone(), cwd);

                let config = ra_ap_ide::FileStructureConfig { exclude_locals: true };
                let structure = analysis.file_structure(&config, file_id)?;

                for node in &structure {
                    if node.label == called_name {
                        if let ra_ap_ide::StructureNodeKind::SymbolKind(
                            ra_ap_ide_db::SymbolKind::Function | ra_ap_ide_db::SymbolKind::Method,
                        ) = node.kind
                        {
                            let start = usize::from(node.node_range.start());
                            let end = usize::from(node.node_range.end());
                            if end <= source.len() && start <= end {
                                let code = source[start..end].to_string();
                                return Ok(RustAnalyzerResponse::CalledFunctionCode(code));
                            }
                        }
                    }
                }

                let offset = ra_ap_ide::TextSize::from(
                    source[..source.lines().take(usize::try_from(function_id.line).unwrap_or(1).saturating_sub(1)).map(|l| l.len() + 1).sum::<usize>()].len() as u32
                );

                let position = ra_ap_ide::FilePosition { file_id, offset };

                let refs_config = ra_ap_ide::FindAllRefsConfig {
                    search_scope: None,
                    ra_fixture: ra_ap_ide_db::ra_fixture::RaFixtureConfig::default(),
                    exclude_imports: false,
                    exclude_tests: false,
                };

                if let Ok(Some(refs)) = analysis.find_all_refs(position, &refs_config) {
                    for search_result in refs {
                        for (ref_file_id, ranges) in &search_result.references {
                            if let Ok(ref_source) = analysis.file_text(*ref_file_id) {
                                for (text_range, _category) in ranges {
                                    let ref_line =
                                        ref_source[..usize::from(text_range.start())].lines().count();
                                    let ref_lines: Vec<&str> = ref_source.lines().collect();
                                    let end_line = (ref_line + 30).min(ref_lines.len());
                                    if end_line > ref_line {
                                        let code = ref_lines[ref_line..end_line].join("\n");
                                        return Ok(RustAnalyzerResponse::CalledFunctionCode(code));
                                    }
                                }
                            }
                        }
                    }
                }

                Ok(RustAnalyzerResponse::CalledFunctionCode(format!(
                    "// Could not find: {called_name}"
                )))
            }
        }
    }
}

fn strip_cfg_attribute(source: &str, attr_name: &str) -> String {
    let simple_attr = format!("#[cfg({})]", attr_name);
    let all_prefix = "#[cfg(all(";
    let all_suffix = ")]";

    let mut result = String::with_capacity(source.len());
    let mut lines = source.lines().peekable();

    for line in lines.by_ref() {
        let trimmed = line.trim();

        if trimmed == simple_attr {
            result.push('\n');
            continue;
        }

        if trimmed.starts_with(all_prefix) && trimmed.ends_with(all_suffix) && trimmed.contains(attr_name) {
            let modified = trimmed
                .replace(&format!("{},", attr_name), "")
                .replace(&format!(", {}", attr_name), "")
                .replace(&format!(",\t{}", attr_name), "")
                .replace(attr_name, "");
            let cleaned = modified
                .replace("#[cfg(all()]", "")
                .replace("#[cfg(all( ))]", "")
                .replace("#[cfg(all())]", "");
            if !cleaned.trim().is_empty() && cleaned.trim() != "#[cfg(all())]" && cleaned.trim() != "#[cfg(all( ))]" {
                result.push_str(&cleaned);
            }
            result.push('\n');
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    result
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
            GitRequest::WalkRustFiles { path } => {
                let files = walkdir_rust_files(&path);
                Ok(GitResponse {
                    success: true,
                    output: files.into_iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>().join("\n"),
                })
            }
        }
    }
}

fn walkdir_rust_files(path: &PathBuf) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                files.extend(walkdir_rust_files(&p));
            } else if let Some(ext) = p.extension() {
                if ext == "rs" {
                    files.push(p);
                }
            }
        }
    }
    files
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