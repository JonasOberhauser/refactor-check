use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

static NEXT_CODE_PIECE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FunctionId {
    pub file: PathBuf,
    pub name: String,
    pub impl_for: Option<String>,
    pub line: u32,
}

impl FunctionId {
    pub fn new(file: PathBuf, name: &str, impl_for: Option<&str>, line: u32) -> Self {
        Self {
            file,
            name: name.to_string(),
            impl_for: impl_for.map(|s| s.to_string()),
            line,
        }
    }

    pub fn display_name(&self) -> String {
        match &self.impl_for {
            Some(impl_for) => format!("{}::{} (line {})", impl_for, self.name, self.line),
            None => format!("{} (line {})", self.name, self.line),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DeductiveCodePiece {
    id: u64,
    file: PathBuf,
    function_id: FunctionId,
    start_line: u32,
    end_line: u32,
    code: String,
}

impl DeductiveCodePiece {
    pub(crate) fn with_id(
        id: u64,
        file: PathBuf,
        function_id: FunctionId,
        start_line: u32,
        end_line: u32,
        code: String,
    ) -> Self {
        Self {
            id,
            file,
            function_id,
            start_line,
            end_line,
            code,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn file(&self) -> &PathBuf {
        &self.file
    }

    pub fn function_id(&self) -> &FunctionId {
        &self.function_id
    }

    pub fn start_line(&self) -> u32 {
        self.start_line
    }

    pub fn end_line(&self) -> u32 {
        self.end_line
    }

    pub fn code(&self) -> &str {
        &self.code
    }
}

pub type ArcCodePiece = Arc<DeductiveCodePiece>;

pub fn next_code_piece_id() -> u64 {
    NEXT_CODE_PIECE_ID.fetch_add(1, Ordering::Relaxed)
}