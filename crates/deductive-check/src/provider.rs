use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use refactor_check_core::provider::{
    DynLlmProvider, DynSolverProvider, IOProvider,
};

use crate::code_piece::FunctionId;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FunctionInfo {
    pub id: FunctionId,
    pub body: String,
    pub start_line: u32,
    pub end_line: u32,
    pub docs: String,
    pub has_guarantees: bool,
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
        called: CalledFunction,
    },
    GetFunctionDocs {
        function_id: FunctionId,
    },
    GetFileContent {
        path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct CalledFunctionCode {
    pub code: String,
    pub docs: String,
}

#[derive(Debug, Clone)]
pub enum RustAnalyzerResponse {
    FunctionList(Vec<FunctionInfo>),
    FunctionCode(String),
    CalledFunctionList(Vec<CalledFunction>),
    CalledFunctionCode(CalledFunctionCode),
    FunctionDocs(String),
    FileContent(String),
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
    Init { path: PathBuf },
    AddAll { path: PathBuf },
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

#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub prompt: String,
    pub working_directory: PathBuf,
    pub files_to_read: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct AgentResponse {
    pub stdout: String,
    pub success: bool,
}

pub type DynAgentProvider = dyn IOProvider<AgentRequest, AgentResponse>;

pub struct Providers<'a> {
    pub llm: &'a DynLlmProvider,
    pub solver: &'a DynSolverProvider,
    pub rust_analyzer: &'a DynRustAnalyzerProvider,
    pub git: &'a DynGitProvider,
    pub filesystem: &'a DynFileSystemProvider,
    pub python: &'a DynPythonProvider,
    pub agent: &'a DynAgentProvider,
}

use std::collections::HashMap;
use std::sync::Arc;

use ra_ap_syntax::ast::{self, AstNode, HasName};
use std::sync::Mutex;

pub struct CliRustAnalyzerProvider {
    host: Mutex<ra_ap_ide::AnalysisHost>,
    path_to_file_id: Mutex<HashMap<PathBuf, ra_ap_vfs::FileId>>,
    file_id_to_path: Mutex<HashMap<ra_ap_vfs::FileId, PathBuf>>,
    _proc_macro_client: Option<ra_ap_proc_macro_api::ProcMacroClient>,
}

fn fetch_docs(analysis: &ra_ap_ide::Analysis, file_id: ra_ap_vfs::FileId, offset: ra_ap_ide::TextSize) -> String {
    let config = ra_ap_ide::GotoDefinitionConfig {
        ra_fixture: ra_ap_ide_db::ra_fixture::RaFixtureConfig::default(),
    };
    let position = ra_ap_ide::FilePosition { file_id, offset };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        analysis.goto_definition(position, &config)
    }));
    match result {
        Ok(Ok(Some(range_info))) => {
            for nav_target in range_info.info {
                if let Some(docs) = nav_target.docs {
                    return docs.as_str().to_string();
                }
            }
            String::new()
        }
        Ok(Ok(None)) => {
            debug!(?file_id, ?offset, "fetch_docs: goto_definition returned None");
            String::new()
        }
        Ok(Err(e)) => {
            debug!(?file_id, ?offset, error = %e, "fetch_docs: goto_definition returned error");
            String::new()
        }
        Err(panic_payload) => {
            let panic_msg = panic_payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            warn!(?file_id, ?offset, panic_msg, "fetch_docs: goto_definition panicked (ra_ap bug), returning empty docs");
            String::new()
        }
    }
}

/// A function has preconditions that its callers must satisfy.
/// a) IS unsafe fn, b) preconditions in doc/contract, c) can panic, d) in invariant impl
fn has_preconditions(fn_ast: &ast::Fn, docs: &str) -> bool {
    fn_is_unsafe_fn(fn_ast)
        || docs_contain_preconditions(docs)
        || fn_has_requires_attr(fn_ast)
        || docs_mention_panic(docs)
        || fn_in_invariant_impl(fn_ast)
}

/// A function is worth verifying (excluding the outgoing-calls check).
/// b) unsafe block, c) in invariant impl, d) postconditions, e) assertions.
/// (a) calls function with preconditions — checked separately via outgoing_calls.
fn has_guarantees(fn_ast: &ast::Fn, docs: &str) -> bool {
    fn_body_contains_unsafe(fn_ast)
        || fn_in_invariant_impl(fn_ast)
        || fn_has_ensures_attr(fn_ast)
        || docs_contain_guarantees(docs)
        || fn_body_contains_assertions(fn_ast)
}

fn fn_is_unsafe_fn(fn_ast: &ast::Fn) -> bool {
    fn_ast.unsafe_token().is_some()
}

fn fn_has_requires_attr(fn_ast: &ast::Fn) -> bool {
    use ra_ap_syntax::ast::HasAttrs;
    fn_ast.attrs().any(|attr| {
        attr.meta()
            .and_then(|m| m.path())
            .map(|p| {
                let last = p.syntax().text().to_string();
                last.split("::").last().unwrap_or(&last) == "requires"
            })
            .unwrap_or(false)
    })
}

fn fn_has_ensures_attr(fn_ast: &ast::Fn) -> bool {
    use ra_ap_syntax::ast::HasAttrs;
    fn_ast.attrs().any(|attr| {
        attr.meta()
            .and_then(|m| m.path())
            .map(|p| {
                let last = p.syntax().text().to_string();
                last.split("::").last().unwrap_or(&last) == "ensures"
            })
            .unwrap_or(false)
    })
}

fn fn_in_invariant_impl(fn_ast: &ast::Fn) -> bool {
    use ra_ap_syntax::ast::HasAttrs;
    let mut current = fn_ast.syntax().parent();
    while let Some(parent) = current {
        if ra_ap_syntax::ast::Impl::can_cast(parent.kind()) {
            if let Some(impl_node) = ra_ap_syntax::ast::Impl::cast(parent) {
                for attr in impl_node.attrs() {
                    if let Some(path) = attr.meta().and_then(|m| m.path()) {
                        let last = path.syntax().text().to_string();
                        let last = last.split("::").last().unwrap_or(&last);
                        if last == "invariant" {
                            return true;
                        }
                    }
                }
            }
            return false;
        }
        current = parent.parent();
    }
    false
}

/// Check if a call target has preconditions, by navigating to its AST.
fn call_target_has_preconditions(
    analysis: &ra_ap_ide::Analysis,
    target: &ra_ap_ide::NavigationTarget,
    current_file_id: ra_ap_vfs::FileId,
    current_root: &ra_ap_syntax::SyntaxNode,
) -> bool {
    let docs = target
        .docs
        .as_ref()
        .map(|d| d.as_str().to_string())
        .unwrap_or_default();

    if docs_contain_preconditions(&docs) || docs_mention_panic(&docs) {
        return true;
    }

    let root = if target.file_id == current_file_id {
        current_root.clone()
    } else {
        match analysis.parse(target.file_id) {
            Ok(tree) => tree.syntax().clone(),
            Err(_) => return false,
        }
    };

    let target_range = target.full_range;
    let fn_ast = root
        .descendants()
        .find(|n| n.text_range() == target_range)
        .and_then(ast::Fn::cast);

    if let Some(fn_ast) = fn_ast {
        return has_preconditions(&fn_ast, &docs);
    }
    false
}

fn attrs_look_like_test(fn_ast: &ra_ap_syntax::ast::Fn) -> bool {
    use ra_ap_syntax::ast::HasAttrs;

    for attr in fn_ast.attrs() {
        if let Some(meta) = attr.meta() {
            if let Some(path) = meta.path() {
                let path_text = path.syntax().text().to_string();
                if let Some(last) = path_text.split("::").last() {
                    if last == "test" || last == "bench" {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn docs_contain_preconditions(docs: &str) -> bool {
    if docs.is_empty() {
        return false;
    }
    let lower = docs.to_lowercase();
    lower.contains("# safety")
        || lower.contains("# preconditions")
        || lower.contains("# pre-conditions")
}

fn docs_mention_panic(docs: &str) -> bool {
    docs.to_lowercase().contains("# panics")
}

fn fn_body_contains_unsafe(fn_ast: &ast::Fn) -> bool {
    let Some(body) = fn_ast.body() else { return false };
    body.syntax()
        .descendants_with_tokens()
        .any(|e| e.into_token().is_some_and(|t| t.kind() == ra_ap_syntax::T![unsafe]))
}

fn fn_body_contains_assertions(fn_ast: &ast::Fn) -> bool {
    let Some(body) = fn_ast.body() else { return false };
    let syntax = body.syntax();

    for macro_call in syntax.descendants().filter_map(ast::MacroCall::cast) {
        if let Some(path) = macro_call.path() {
            let name = path.syntax().text().to_string();
            let last = name.split("::").last().unwrap_or(&name);
            if matches!(last,
                "assert" | "assert_eq" | "assert_ne"
                | "debug_assert" | "debug_assert_eq" | "debug_assert_ne"
                | "panic" | "unreachable" | "unimplemented"
            ) {
                return true;
            }
        }
    }

    for method_call in syntax.descendants().filter_map(ast::MethodCallExpr::cast) {
        if let Some(name_ref) = method_call.name_ref() {
            let name = name_ref.syntax().text().to_string();
            if name == "unwrap" || name == "expect" {
                return true;
            }
        }
    }

    false
}

fn docs_contain_guarantees(docs: &str) -> bool {
    if docs.is_empty() {
        return false;
    }
    let lower = docs.to_lowercase();
    lower.contains("# guarantees")
        || lower.contains("# postconditions")
        || lower.contains("# post-conditions")
        || lower.contains("# ensures")
        || lower.contains("# safety")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_fn(src: &str) -> ast::Fn {
        let parse = ra_ap_syntax::SourceFile::parse(src, ra_ap_syntax::Edition::Edition2024);
        parse.tree()
            .syntax()
            .descendants()
            .filter_map(ast::Fn::cast)
            .next()
            .expect("no Fn found in source")
    }

    #[test]
    fn test_unsafe_block_has_guarantees() {
        assert!(has_guarantees(&parse_fn("fn foo() { unsafe { *ptr } }"), ""));
        assert!(has_guarantees(&parse_fn("fn foo() { unsafe (*ptr).bar() }"), ""));
        assert!(has_guarantees(&parse_fn("fn foo() { unsafe\n{\n*ptr\n}"), ""));
    }

    #[test]
    fn test_no_guarantees_for_plain_fn() {
        assert!(!has_guarantees(&parse_fn("fn foo() { let x = 1; }"), ""));
        assert!(!has_preconditions(&parse_fn("fn foo() { let x = 1; }"), ""));
    }

    #[test]
    fn test_unsafe_in_comment_ignored() {
        assert!(!has_guarantees(&parse_fn("fn foo() { // unsafe {\n let x = 1; }"), ""));
        assert!(!has_guarantees(&parse_fn("fn foo() { /* unsafe { */ let x = 1; }"), ""));
    }

    #[test]
    fn test_unsafe_in_string_ignored() {
        assert!(!has_guarantees(&parse_fn("fn foo() { let s = \"unsafe {\"; }"), ""));
    }

    #[test]
    fn test_assertions_are_guarantees_not_preconditions() {
        let f = parse_fn("fn foo() { assert!(x > 0); }");
        assert!(has_guarantees(&f, ""));
        assert!(!has_preconditions(&f, ""));
        let f = parse_fn("fn foo() { x.unwrap(); }");
        assert!(has_guarantees(&f, ""));
        assert!(!has_preconditions(&f, ""));
        let f = parse_fn("fn foo() { x.expect(\"msg\"); }");
        assert!(has_guarantees(&f, ""));
        assert!(!has_preconditions(&f, ""));
        let f = parse_fn("fn foo() { unreachable!(); }");
        assert!(has_guarantees(&f, ""));
        assert!(!has_preconditions(&f, ""));
    }

    #[test]
    fn test_docs_guarantees() {
        let empty_fn = parse_fn("fn foo() {}");
        assert!(has_guarantees(&empty_fn, "# Guarantees\nThis function is safe."));
        assert!(has_guarantees(&empty_fn, "# Postconditions\nReturns non-zero."));
        assert!(has_guarantees(&empty_fn, "# Ensures\nresult >= 0"));
        assert!(has_guarantees(&empty_fn, "# Safety\nThis is safe because..."));
        assert!(!has_guarantees(&empty_fn, "# Arguments\nblah"));
        assert!(!has_guarantees(&empty_fn, ""));
    }

    #[test]
    fn test_assertion_in_comment_ignored() {
        assert!(!has_guarantees(&parse_fn("fn foo() { // assert!(x > 0)\n }"), ""));
        assert!(!has_guarantees(&parse_fn("fn foo() { /* .unwrap() */ }"), ""));
    }

    #[test]
    fn test_unsafe_in_raw_string_ignored() {
        assert!(!has_guarantees(&parse_fn("fn foo() { let s = r#\"unsafe {\"#; }"), ""));
        assert!(!has_guarantees(&parse_fn("fn foo() { let s = r\"unsafe {\"; }"), ""));
        assert!(!has_guarantees(&parse_fn("fn foo() { let s = br#\"unsafe {\"#; }"), ""));
    }

    #[test]
    fn test_requires_attr_is_precondition() {
        let src = "#[requires(self.value < 99)]\npub fn inc(&mut self) { self.value += 1; }";
        let f = parse_fn(src);
        assert!(has_preconditions(&f, ""));
        assert!(!has_guarantees(&f, ""));
    }

    #[test]
    fn test_ensures_attr_is_guarantee() {
        let src = "#[ensures(ret.value == 0)]\npub fn new() -> Self { Self { value: 0 } }";
        let f = parse_fn(src);
        assert!(has_guarantees(&f, ""));
        assert!(!has_preconditions(&f, ""));
    }

    #[test]
    fn test_invariant_impl_is_both() {
        let src = r#"#[invariant(self.value < 100)]
impl Value {
    pub fn inc(&mut self) {
        self.value += 1;
    }
}"#;
        let fn_ast = parse_fn(src);
        assert!(fn_in_invariant_impl(&fn_ast));
        assert!(has_preconditions(&fn_ast, ""));
        assert!(has_guarantees(&fn_ast, ""));
    }

    #[test]
    fn test_no_contract_no_preconditions_no_guarantees() {
        let src = r#"impl Value {
    pub fn inc(&mut self) {
        self.value += 1;
    }
}"#;
        let fn_ast = parse_fn(src);
        assert!(!fn_in_invariant_impl(&fn_ast));
        assert!(!has_preconditions(&fn_ast, ""));
        assert!(!has_guarantees(&fn_ast, ""));
    }

    #[test]
    fn test_unsafe_fn_has_preconditions() {
        let f = parse_fn("unsafe fn foo(x: *const i32) -> i32 { *(x) }");
        assert!(has_preconditions(&f, ""));
    }

    #[test]
    fn test_docs_mention_panic_is_precondition() {
        let f = parse_fn("fn foo(x: i32) -> i32 { x }");
        assert!(has_preconditions(&f, "# Panics\nIf x is negative."));
    }
}

fn byte_offset_to_line(source: &str, offset: ra_ap_ide::TextSize) -> u32 {
    let byte = usize::from(offset);
    assert!(byte <= source.len(), "byte_offset_to_line: offset {} exceeds source len {}", byte, source.len());
    let mut line = 1u32;
    for (i, ch) in source.char_indices() {
        if i >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
        }
    }
    line
}

fn line_to_byte_offset(source: &str, line: u32) -> ra_ap_ide::TextSize {
    let mut byte_offset = 0usize;
    let mut current_line = 1u32;
    for ch in source.chars() {
        if current_line >= line {
            break;
        }
        byte_offset += ch.len_utf8();
        if ch == '\n' {
            current_line += 1;
        }
    }
    assert!(byte_offset <= source.len(), "line_to_byte_offset: line {} maps to offset {} exceeding source len {}", line, byte_offset, source.len());
    ra_ap_ide::TextSize::from(byte_offset as u32)
}

fn fn_impl_for(fn_ast: &ra_ap_syntax::ast::Fn) -> Option<String> {
    let syntax = fn_ast.syntax();
    let mut current = syntax.parent();
    while let Some(parent) = current {
        if ra_ap_syntax::ast::Impl::can_cast(parent.kind()) {
            if let Some(impl_node) = ra_ap_syntax::ast::Impl::cast(parent) {
                if let Some(self_ty) = impl_node.self_ty() {
                    let ty_text = self_ty.syntax().text().to_string();
                    return Some(ty_text);
                }
            }
            return None;
        }
        current = parent.parent();
    }
    None
}

fn fn_is_in_trait(fn_ast: &ra_ap_syntax::ast::Fn) -> bool {
    fn_ast
        .syntax()
        .ancestors()
        .skip(1)
        .any(|a| ra_ap_syntax::ast::Trait::can_cast(a.kind()))
}

impl CliRustAnalyzerProvider {
    pub fn new(project_path: String) -> Result<Self> {
        let project_path = PathBuf::from(&project_path);
        let root =
            ra_ap_paths::AbsPathBuf::assert_utf8(std::env::current_dir()?.join(&project_path));
        let manifest = ra_ap_project_model::ProjectManifest::discover_single(&root)?;

        let cargo_config = ra_ap_project_model::CargoConfig {
            features: ra_ap_project_model::CargoFeatures::Selected {
                features: vec!["verification".to_string()],
                no_default_features: false,
            },
            set_test: false,
            sysroot: Some(ra_ap_project_model::RustLibSource::Discover),
            ..Default::default()
        };

        let mut workspace = ra_ap_project_model::ProjectWorkspace::load(
            manifest,
            &cargo_config,
            &|_| {},
        )?;

        let build_scripts = workspace.run_build_scripts(&cargo_config, &|_| {})?;
        workspace.set_build_scripts(build_scripts);

        let load_config = ra_ap_load_cargo::LoadCargoConfig {
            load_out_dirs_from_check: false,
            with_proc_macro_server: ra_ap_load_cargo::ProcMacroServerChoice::Sysroot,
            prefill_caches: false,
            num_worker_threads: 1,
            proc_macro_processes: 1,
        };

        let (raw_db, vfs, proc_macro_client) = ra_ap_load_cargo::load_workspace(
            workspace,
            &cargo_config.extra_env,
            &load_config,
        )?;

        let mut host = ra_ap_ide::AnalysisHost::new(None);
        *host.raw_database_mut() = raw_db;

        let mut path_to_file_id: HashMap<PathBuf, ra_ap_vfs::FileId> = HashMap::new();
        let mut file_id_to_path: HashMap<ra_ap_vfs::FileId, PathBuf> = HashMap::new();
        for (file_id, vfs_path) in vfs.iter() {
            if let Some(abs_path) = vfs_path.as_path() {
                let path_buf: PathBuf = abs_path.to_path_buf().into();
                path_to_file_id.insert(path_buf.clone(), file_id);
                file_id_to_path.insert(file_id, path_buf);
            }
        }

        Ok(Self {
            host: Mutex::new(host),
            path_to_file_id: Mutex::new(path_to_file_id),
            file_id_to_path: Mutex::new(file_id_to_path),
            _proc_macro_client: proc_macro_client,
        })
    }

    fn analysis(&self) -> ra_ap_ide::Analysis {
        self.host.lock().unwrap_or_else(|e| e.into_inner()).analysis()
    }

    fn find_file_id(&self, file_path: &Path) -> Option<ra_ap_vfs::FileId> {
        let map = self.path_to_file_id.lock().unwrap_or_else(|e| e.into_inner());
        map.get(file_path).copied()
    }

    fn list_functions_in_file(&self, file_path: &Path) -> Vec<FunctionInfo> {
        let analysis = self.analysis();
        let Some(file_id) = self.find_file_id(file_path) else {
            return Vec::new();
        };

        let Ok(source) = analysis.file_text(file_id) else {
            return Vec::new();
        };
        let source_text = source.as_ref();

        let Ok(tree) = analysis.parse(file_id) else {
            return Vec::new();
        };
        let syntax_root = tree.syntax();

        let file_id_to_path = self
            .file_id_to_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let call_config = ra_ap_ide::CallHierarchyConfig {
            exclude_tests: true,
            ra_fixture: ra_ap_ide_db::ra_fixture::RaFixtureConfig::default(),
        };

        let mut functions = Vec::new();

        for fn_ast in syntax_root.descendants().filter_map(ra_ap_syntax::ast::Fn::cast) {
            if attrs_look_like_test(&fn_ast) {
                continue;
            }

            if fn_is_in_trait(&fn_ast) {
                continue;
            }

            let Some(name_node) = fn_ast.name() else {
                continue;
            };
            let name = name_node.text().to_string();

            let syntax = fn_ast.syntax();
            let range = syntax.text_range();
            assert!(
                range.start() <= range.end(),
                "list_functions: negative range {:?} for function {}", range, name,
            );

            let start_line = byte_offset_to_line(source_text, range.start());
            let end_line = byte_offset_to_line(source_text, range.end());

            let impl_for = fn_impl_for(&fn_ast);

            let name_range = name_node.syntax().text_range();
            let nav_offset = name_range.start();
            assert!(
                usize::from(nav_offset) <= source_text.len(),
                "list_functions: nav_offset {} exceeds source len {} for function {}",
                usize::from(nav_offset), source_text.len(), name,
            );
            let docs = fetch_docs(&analysis, file_id, nav_offset);

            let mut has_guarantees = has_guarantees(&fn_ast, &docs);

            if !has_guarantees {
                let position = ra_ap_ide::FilePosition { file_id, offset: nav_offset };
                let call_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    analysis.outgoing_calls(&call_config, position)
                }));
                match call_result {
                    Ok(Ok(Some(call_items))) => {
                        for item in &call_items {
                            if call_target_has_preconditions(
                                &analysis,
                                &item.target,
                                file_id,
                                syntax_root,
                            ) {
                                has_guarantees = true;
                                break;
                            }
                        }
                    }
                    Ok(Ok(None)) | Ok(Err(_)) => {}
                    Err(panic_payload) => {
                        let panic_msg = panic_payload
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".to_string());
                        warn!(?file_id, ?nav_offset, panic_msg, "outgoing_calls panicked (ra_ap bug), skipping precondition check for {}", name);
                    }
                }
            }

            let Some(file_path) = file_id_to_path.get(&file_id).cloned() else {
                continue;
            };

            let fid = FunctionId::new(file_path, &name, impl_for.as_deref(), start_line);
            functions.push(FunctionInfo {
                id: fid,
                body: String::new(),
                start_line,
                end_line,
                docs,
                has_guarantees,
            });
        }

        functions
    }

    fn get_function_code(&self, function_id: &FunctionId) -> Result<String> {
        let analysis = self.analysis();
        let Some(file_id) = self.find_file_id(&function_id.file) else {
            anyhow::bail!("File not found in project: {:?}", function_id.file);
        };
        let config = ra_ap_ide::FileStructureConfig { exclude_locals: true };
        let structure = analysis.file_structure(&config, file_id)?;

        let source = analysis.file_text(file_id)?;

        for node in &structure {
            if let ra_ap_ide::StructureNodeKind::SymbolKind(
                ra_ap_ide_db::SymbolKind::Function | ra_ap_ide_db::SymbolKind::Method,
            ) = node.kind
            {
                assert!(
                    node.node_range.start() <= node.node_range.end(),
                    "get_function_code: negative range {:?} for node {}", node.node_range, node.label,
                );
                let node_line = byte_offset_to_line(&source, node.node_range.start());
                if node_line == function_id.line && node.label == function_id.name {
                    let start = usize::from(node.node_range.start());
                    let end = usize::from(node.node_range.end());
                    assert!(end <= source.len(), "get_function_code: node range {}..{} exceeds source len {}", start, end, source.len());
                    return Ok(source[start..end].to_string());
                }
            }
        }

        let lines: Vec<&str> = source.lines().collect();
        let start = usize::try_from(function_id.line).unwrap_or(1).saturating_sub(1);
        let end = (start + 50).min(lines.len());
        let code = lines[start..end].join("\n");
        if code.trim().is_empty() {
            anyhow::bail!("Function {} at {:?}:{} resolved to empty code", function_id.name, function_id.file, function_id.line);
        }
        Ok(code)
    }

    fn get_called_functions(&self, function_id: &FunctionId) -> Result<Vec<CalledFunction>> {
        let analysis = self.analysis();
        let Some(file_id) = self.find_file_id(&function_id.file) else {
            return Ok(Vec::new());
        };

        let source = analysis.file_text(file_id)?;
        let offset = line_to_byte_offset(&source, function_id.line);
        let position = ra_ap_ide::FilePosition { file_id, offset };

        let config = ra_ap_ide::CallHierarchyConfig {
            exclude_tests: true,
            ra_fixture: ra_ap_ide_db::ra_fixture::RaFixtureConfig::default(),
        };

        let file_id_to_path = self.file_id_to_path.lock().unwrap_or_else(|e| e.into_inner()).clone();

        let mut called = Vec::new();
        if let Ok(Some(call_items)) = analysis.outgoing_calls(&config, position) {
            for item in call_items {
                assert!(
                    item.target.full_range.start() <= item.target.full_range.end(),
                    "get_called_functions: negative range {:?} for call target {}", item.target.full_range, item.target.name,
                );
                let name = item.target.name.to_string();
                let called_file = file_id_to_path
                    .get(&item.target.file_id)
                    .cloned();
                let start_line = called_file.as_ref().and_then(|f| {
                    let fid = self.find_file_id(f)?;
                    let src = analysis.file_text(fid).ok()?;
                    Some(byte_offset_to_line(&src, item.target.full_range.start()))
                });
                called.push(CalledFunction {
                    name,
                    file: called_file,
                    start_line,
                });
            }
        }

        called.sort_by(|a, b| a.name.cmp(&b.name));
        called.dedup_by(|a, b| a.name == b.name);
        Ok(called)
    }

    fn get_called_function_code(&self, called: &CalledFunction) -> Result<CalledFunctionCode> {
        let Some(called_file) = &called.file else {
            return Ok(CalledFunctionCode {
                code: format!("// Could not find: {}", called.name),
                docs: String::new(),
            });
        };
        let analysis = self.analysis();
        let Some(file_id) = self.find_file_id(called_file) else {
            return Ok(CalledFunctionCode {
                code: format!("// Could not find file: {:?}", called_file),
                docs: String::new(),
            });
        };

        let config = ra_ap_ide::FileStructureConfig { exclude_locals: true };
        let structure = analysis.file_structure(&config, file_id)?;
        let source = analysis.file_text(file_id)?;

        for node in &structure {
            if node.label == called.name {
                if let ra_ap_ide::StructureNodeKind::SymbolKind(
                    ra_ap_ide_db::SymbolKind::Function | ra_ap_ide_db::SymbolKind::Method,
                ) = node.kind
                {
                    assert!(
                        node.node_range.start() <= node.node_range.end(),
                        "get_called_function_code: negative range {:?} for node {}", node.node_range, node.label,
                    );
                    let start = usize::from(node.node_range.start());
                    let end = usize::from(node.node_range.end());
                    if end <= source.len() {
                        let code = source[start..end].to_string();
                        let docs = fetch_docs(&analysis, file_id, node.navigation_range.start());
                        let impl_ctx = extract_impl_context(&source, start);
                        let full_code = if impl_ctx.is_empty() {
                            code
                        } else {
                            format!("{}\n{}", impl_ctx, code)
                        };
                        debug!(
                            name = %node.label,
                            impl_ctx = %impl_ctx,
                            "get_called_function_code: resolved node",
                        );
                        return Ok(CalledFunctionCode { code: full_code, docs });
                    }
                }
            }
        }

        Ok(CalledFunctionCode {
            code: format!("// Could not find: {}", called.name),
            docs: String::new(),
        })
    }

    fn get_function_docs(&self, function_id: &FunctionId) -> String {
        let analysis = self.analysis();
        let Some(file_id) = self.find_file_id(&function_id.file) else {
            return String::new();
        };
        let Ok(source) = analysis.file_text(file_id) else {
            return String::new();
        };
        let offset = line_to_byte_offset(&source, function_id.line);
        fetch_docs(&analysis, file_id, offset)
    }
}

#[async_trait]
impl IOProvider<RustAnalyzerRequest, RustAnalyzerResponse> for CliRustAnalyzerProvider {
    async fn invoke(&self, input: RustAnalyzerRequest) -> Result<RustAnalyzerResponse> {
        match input {
            RustAnalyzerRequest::ListFunctions { files, cfg_verification: _ } => {
                let mut all_functions = Vec::new();
                for file_path in &files {
                    all_functions.extend(self.list_functions_in_file(file_path));
                }
                Ok(RustAnalyzerResponse::FunctionList(all_functions))
            }
            RustAnalyzerRequest::GetFunctionCode { function_id } => {
                let code = self.get_function_code(&function_id)?;
                Ok(RustAnalyzerResponse::FunctionCode(code))
            }
            RustAnalyzerRequest::GetCalledFunctions { function_id } => {
                let called = self.get_called_functions(&function_id)?;
                Ok(RustAnalyzerResponse::CalledFunctionList(called))
            }
            RustAnalyzerRequest::GetCalledFunctionCode { called } => {
                let result = self.get_called_function_code(&called)?;
                Ok(RustAnalyzerResponse::CalledFunctionCode(result))
            }
            RustAnalyzerRequest::GetFunctionDocs { function_id } => {
                let docs = self.get_function_docs(&function_id);
                Ok(RustAnalyzerResponse::FunctionDocs(docs))
            }
            RustAnalyzerRequest::GetFileContent { path } => {
                let analysis = self.analysis();
                if let Some(file_id) = self.find_file_id(&path) {
                    if let Ok(source) = analysis.file_text(file_id) {
                        return Ok(RustAnalyzerResponse::FileContent(source.to_string()));
                    }
                }
                Ok(RustAnalyzerResponse::FileContent(String::new()))
            }
        }
    }
}

pub struct CliGitProvider {
    project_path: PathBuf,
}

impl CliGitProvider {
    #[must_use]
    pub fn new(project_path: PathBuf) -> Self {
        Self { project_path }
    }
}

#[async_trait]
impl IOProvider<GitRequest, GitResponse> for CliGitProvider {
    async fn invoke(&self, input: GitRequest) -> Result<GitResponse> {
        let dir = &self.project_path;
        match input {
            GitRequest::CreateBranch { name } => {
                let output = tokio::process::Command::new("git")
                    .args(["checkout", "-b", &name])
                    .current_dir(dir)
                    .output()
                    .await?;
                Ok(git_response(output))
            }
            GitRequest::Commit { message } => {
                let output = tokio::process::Command::new("git")
                    .args(["commit", "-m", &message])
                    .current_dir(dir)
                    .output()
                    .await?;
                Ok(git_response(output))
            }
            GitRequest::AddFiles { paths } => {
                let mut args = vec!["add".to_string()];
                for p in &paths {
                    args.push(p.to_string_lossy().to_string());
                }
                let output = tokio::process::Command::new("git")
                    .args(&args)
                    .current_dir(dir)
                    .output()
                    .await?;
                Ok(git_response(output))
            }
            GitRequest::FindChangedRustFiles { base_commit } => {
                let output = tokio::process::Command::new("git")
                    .args(["diff", "--name-only", &base_commit])
                    .current_dir(dir)
                    .output()
                    .await?;
                Ok(git_response(output))
            }
            GitRequest::CurrentCommitHash => {
                let output = tokio::process::Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(dir)
                    .output()
                    .await?;
                Ok(git_response(output))
            }
            GitRequest::CreateDirectory { path } => {
                std::fs::create_dir_all(&path)?;
                Ok(GitResponse {
                    success: true,
                    output: path.to_string_lossy().to_string(),
                })
            }
            GitRequest::WalkRustFiles { path } => {
                let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                let files = walkdir_rust_files(&canonical);
                Ok(GitResponse {
                    success: true,
                    output: files.into_iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>().join("\n"),
                })
            }
            GitRequest::Init { path: _ } => {
                let output = tokio::process::Command::new("git")
                    .args(["init"])
                    .current_dir(dir)
                    .output()
                    .await?;
                Ok(git_response(output))
            }
            GitRequest::AddAll { path: _ } => {
                let output = tokio::process::Command::new("git")
                    .args(["add", "*.rs", "*.toml", "*.lock"])
                    .current_dir(dir)
                    .output()
                    .await?;
                Ok(git_response(output))
            }
        }
    }
}

fn git_response(output: std::process::Output) -> GitResponse {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stdout}\n{stderr}").trim().to_string(),
        (false, true) => stdout.trim().to_string(),
        (true, false) => stderr.trim().to_string(),
        (true, true) => String::new(),
    };
    GitResponse {
        success: output.status.success(),
        output: combined,
    }
}

/// Extract contract attributes (#[invariant], #[requires], #[ensures]) and
/// doc comments from the parent impl block and struct definition of a function.
/// Uses ra_ap_syntax AST traversal per AGENTS.md rules.
fn extract_impl_context(source: &str, func_offset: usize) -> String {
    use ra_ap_syntax::{ast::{self, AstNode, HasAttrs}, SourceFile, TextSize};

    let parse = SourceFile::parse(source, ra_ap_syntax::Edition::CURRENT);
    let tree = parse.tree();

    let offset = TextSize::from(func_offset as u32);
    let Some(token) = tree.syntax().token_at_offset(offset).next() else {
        return String::new();
    };

    let mut node = token.parent();
    while let Some(parent) = node {
        if let Some(impl_node) = ast::Impl::cast(parent.clone()) {
            let mut context = String::new();

            for attr in impl_node.attrs() {
                let text = attr.syntax().text().to_string();
                if text.contains("invariant")
                    || text.contains("requires")
                    || text.contains("ensures")
                {
                    context.push_str(&text);
                    context.push('\n');
                }
            }

            for token in impl_node.syntax().descendants_with_tokens() {
                if let ra_ap_syntax::SyntaxElement::Token(t) = token {
                    let text = t.text();
                    if text.trim_start().starts_with("///") || text.trim_start().starts_with("//!") {
                        context.push_str(text.trim());
                        context.push('\n');
                    }
                }
            }

            if let Some(self_ty) = impl_node.self_ty() {
                let type_name = self_ty.syntax().text().to_string();
                for item in tree.syntax().descendants() {
                    if let Some(s) = ast::Struct::cast(item.clone()) {
                        if s.name().is_some_and(|n| n.text() == type_name) {
                            for token in s.syntax().descendants_with_tokens() {
                                if let ra_ap_syntax::SyntaxElement::Token(t) = token {
                                    let text = t.text();
                                    if text.trim_start().starts_with("///") {
                                        context.push_str(text.trim());
                                        context.push('\n');
                                    }
                                }
                            }
                        }
                    }
                }
            }

            return context.trim().to_string();
        }
        node = parent.parent();
    }

    String::new()
}

fn walkdir_rust_files(path: &PathBuf) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let dir_name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if dir_name == "tests" || dir_name == "target" || dir_name == "benches" {
                    continue;
                }
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
        Self::new(PathBuf::from("."))
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

pub struct CliAgentProvider {
    binary: String,
    base_args: Vec<String>,
    error_gate: Option<Arc<refactor_check_core::error_gate::ErrorGate>>,
}

impl CliAgentProvider {
    pub fn new(binary: String, args: Vec<String>) -> Self {
        Self { binary, base_args: args, error_gate: None }
    }

    pub fn with_error_gate(mut self, gate: Arc<refactor_check_core::error_gate::ErrorGate>) -> Self {
        self.error_gate = Some(gate);
        self
    }
}

fn extract_text_from_json_events(stdout: &str) -> String {
    let mut texts = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(part) = value.get("part") {
                if let Some(part_type) = part.get("type") {
                    if part_type.as_str() == Some("text") {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                            texts.push(text.to_string());
                        }
                    }
                }
            }
            if value.get("type").and_then(|t| t.as_str()) == Some("error") {
                if let Some(err) = value.get("error") {
                    if let Some(msg) = err.get("data").and_then(|d| d.get("message")).and_then(|m| m.as_str()) {
                        texts.push(format!("[agent error: {msg}]"));
                    }
                }
            }
        }
    }
    texts.join("")
}

#[async_trait]
impl IOProvider<AgentRequest, AgentResponse> for CliAgentProvider {
    async fn invoke(&self, input: AgentRequest) -> Result<AgentResponse> {
        loop {
            let mut args = self.base_args.clone();
            args.push(input.prompt.clone());
            args.push("--format".to_string());
            args.push("json".to_string());
            args.push("--dir".to_string());
            args.push(input.working_directory.to_string_lossy().to_string());
            for file in &input.files_to_read {
                args.push("-f".to_string());
                args.push(file.to_string_lossy().to_string());
            }

            let mut cmd = tokio::process::Command::new(&self.binary);
            cmd.args(&args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());

            let output = match cmd.output().await {
                Ok(output) => output,
                Err(e) => {
                    if let Some(gate) = &self.error_gate {
                        gate.report_and_wait(&format!(
                            "Failed to spawn agent binary '{}': {e}\n\
                             Possible fixes:\n\
                             - Install the binary and ensure it is in PATH\n\
                             - Restart with --agent-binary /correct/path/to/binary",
                            self.binary,
                        )).await?;
                        continue;
                    }
                    return Err(e.into());
                }
            };

            let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr_raw = String::from_utf8_lossy(&output.stderr).to_string();
            let extracted = extract_text_from_json_events(&stdout_raw);

            if extracted.is_empty() && !output.status.success() {
                if let Some(gate) = &self.error_gate {
                    gate.report_and_wait(&format!(
                        "Agent binary '{}' failed (exit {:?}): {}",
                        self.binary,
                        output.status.code(),
                        stderr_raw.trim(),
                    )).await?;
                    continue;
                }
            }

            return Ok(AgentResponse {
                stdout: extracted,
                success: output.status.success(),
            });
        }
    }
}

#[cfg(test)]
mod agent_tests {
    use super::extract_text_from_json_events;

    #[test]
    fn test_extract_text_from_single_text_event() {
        let input = r#"{"type":"text","part":{"type":"text","text":"Hello world"}}"#;
        assert_eq!(extract_text_from_json_events(input), "Hello world");
    }

    #[test]
    fn test_extract_text_from_multiple_events() {
        let input = r#"{"type":"step_start","part":{"type":"step-start"}}
{"type":"text","part":{"type":"text","text":"Bug: overflow"}}
{"type":"step_finish","part":{"type":"step-finish"}}"#;
        assert_eq!(extract_text_from_json_events(input), "Bug: overflow");
    }

    #[test]
    fn test_extract_text_from_error_event() {
        let input = r#"{"type":"error","error":{"name":"APIError","data":{"message":"Missing Auth","statusCode":401}}}"#;
        assert_eq!(extract_text_from_json_events(input), "[agent error: Missing Auth]");
    }

    #[test]
    fn test_extract_text_empty_input() {
        assert_eq!(extract_text_from_json_events(""), "");
    }

    #[test]
    fn test_extract_text_invalid_json_lines() {
        let input = "not json\n{\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"ok\"}}\nalso not json";
        assert_eq!(extract_text_from_json_events(input), "ok");
    }
}