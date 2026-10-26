use anyhow::Result;
use refactor_check_core::context_id::ContextId;
use tracing::{debug, warn};

use crate::piece_manager::PieceManager;
use crate::provider::{DynLlmProvider, LlmRequest, LlmRole};
use crate::states::{CodePiece, WaitForSplit};

/// Splitter retry budget.
const SPLIT_SYSTEM_PROMPT: &str = "You are an expert in code refactoring analysis. Given a refactoring with BEFORE and AFTER \
 code, identify the largest matching fragments that can be independently verified for \
 equivalence by an SMT solver. \
 \n\nOutput format:\n\
 Piece: <label>\n\
 ---- BEFORE ----\n\
 <before code for this fragment>\n\
 ---- AFTER ----\n\
 <matching after code for this fragment>\n\
 /* relation: <description of how states differ between before and after> */\n\n\
 Rules:\n\
 - If the refactoring is simple, output one piece.\n\
 - For loops, conditionals, or complex restructurings, split into matching fragments.\n\
 - Each BEFORE must have a matching AFTER.\n\
 - Labels must be short and descriptive.\n\
 - Prefer fewer larger pieces over many tiny ones.\n\
 - If a piece contains code whose equivalence to code in another piece \
 has already been verified, add a comment like: the code from ... to ... \
 has already been verified to be equivalent to the code from ... to ..., \
 so the formalizer can reuse that information.\n\
 - Do not create separate pieces for library include/import statements \
 (e.g. use, import, #include) unless they might have side effects \
 (macros expanding to code, module initialization). Merge import changes \
 into the first substantive piece that uses them.\n\
 - When a split point creates a variable/state mapping gap between BEFORE \
 and AFTER, add a single /* relation: ... */ comment at the end of the \
 piece. The relation describes how states differ between before and after \
 at this boundary. Example: /* relation: after.z == before.y - 1 */ means \
 the variable z in the after version corresponds to y - 1 in the before \
 version. Do not repeat the same comment in both BEFORE and AFTER — one \
 comment per piece suffices. Variables/states not mentioned in a relation \
 comment are assumed to be equivalent between before and after.";
const RESPLIT_PROMPT_HEADER: &str = "The following pieces timed out during SMT verification. Split each piece \
 into finer sub-pieces that the SMT solver can handle:\n\n";
const SPLIT_ATTEMPTS: u32 = 3;
/// Splitter output grammar.
const PIECE_PREFIX_COLON: &str = "Piece:";
const PIECE_PREFIX_SPACE: &str = "Piece ";
const BEFORE_HEADER: &str = "---- BEFORE ----";
const AFTER_HEADER: &str = "---- AFTER ----";
const LABEL_TRIM_COLON: char = ':';
const PIECE_NEWLINE: char = '\n';

pub fn execute(
    state: &WaitForSplit,
    llm: &DynLlmProvider,
    pm: &dyn PieceManager,
    parent_ctx: &ContextId,
) -> Result<Vec<CodePiece>> {
    let mut split_ctx = parent_ctx.new_child();
    for attempt in 0..SPLIT_ATTEMPTS {
        let messages = build_split_messages(state);
        let resp = llm.invoke(LlmRequest { role: LlmRole::Splitter, messages, context_id: Box::new(split_ctx) })?;
        split_ctx = *resp.context_id;
        let response = resp.value;
        let pieces = extract_split_pieces(&response, pm, &split_ctx);

        if !pieces.is_empty() {
            let ids: Vec<&str> = pieces.iter().map(|p| p.ctx_display()).collect();
            debug!(count = pieces.len(), ids = ?ids, attempt = attempt + 1, "splitter produced pieces");
            return Ok(pieces);
        }
        warn!(attempt = attempt + 1, "splitter produced no valid pieces, retrying");
    }
    Err(anyhow::anyhow!(
        "splitter failed to produce valid pieces after 3 attempts"
    ))
}

fn build_split_messages(state: &WaitForSplit) -> Vec<crate::llm::Message> {
    let system = crate::llm::system_message(
        SPLIT_SYSTEM_PROMPT
    );

    let prompt = if state.pieces_to_resplit.is_empty() {
        if let Some(ref feedback) = state.judge_feedback {
            format!(
                "Split this refactoring into independently verifiable pieces.\n\n\
                 The previous split was rejected by the judge:\n{feedback}\n\n\
                 Please produce a better decomposition.\n\n{}",
                state.input_content
            )
        } else {
            format!(
                "Split this refactoring into independently verifiable pieces:\n\n{}",
                state.input_content
            )
        }
    } else {
        let mut prompt = String::from(
            RESPLIT_PROMPT_HEADER,
        );
        for (i, (piece, reason)) in state.pieces_to_resplit.iter().enumerate() {
            prompt.push_str(&format!(
                "Timeout piece {}:\nLabel: {}\nBefore:\n{}\nAfter:\n{}\nReason: {}\n\n",
                i + 1,
                piece.label(),
                piece.before(),
                piece.after(),
                reason,
            ));
        }
        prompt
    };

    vec![system, crate::llm::user_message(&prompt)]
}

fn extract_split_pieces(response: &str, pm: &dyn PieceManager, split_ctx: &ContextId) -> Vec<CodePiece> {
    let mut pieces = Vec::new();
    let mut current_label: Option<String> = None;
    let mut current_section: Option<&mut String> = None;
    let mut before_buf = String::new();
    let mut after_buf = String::new();

    for line in response.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with(PIECE_PREFIX_COLON) || trimmed.starts_with(PIECE_PREFIX_SPACE) {
            if let Some(label) = current_label.take() {
                if !before_buf.trim().is_empty() && !after_buf.trim().is_empty() {
                    let p = pm.new_piece(
                        split_ctx,
                        &label,
                        before_buf.trim(),
                        after_buf.trim(),
                    );
                    debug!(ctx = %p.ctx_display(), label = %p.label(), "extracted piece");
                    pieces.push(p);
                }
                before_buf = String::new();
                after_buf = String::new();
            }
            let label = trimmed
                .trim_start_matches(PIECE_PREFIX_COLON)
                .trim_start_matches(PIECE_PREFIX_SPACE)
                .trim()
                .trim_matches(LABEL_TRIM_COLON)
                .trim()
                .to_string();
            current_label = Some(label);
            current_section = None;
            continue;
        }

        if trimmed == BEFORE_HEADER {
            current_section = Some(&mut before_buf);
            continue;
        }
        if trimmed == AFTER_HEADER {
            current_section = Some(&mut after_buf);
            continue;
        }

        if let Some(ref mut buf) = current_section {
            buf.push_str(line);
            buf.push(PIECE_NEWLINE);
        }
    }

    if let Some(label) = current_label {
        if !before_buf.trim().is_empty() && !after_buf.trim().is_empty() {
            let p = pm.new_piece(
                split_ctx,
                &label,
                before_buf.trim(),
                after_buf.trim(),
            );
            debug!(ctx = %p.ctx_display(), label = %p.label(), "extracted final piece");
            pieces.push(p);
        }
    }

    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece_manager::DefaultPieceManager;

    /// Fixture responses and expected labels for the extraction tests.
    const RESPONSE_SINGLE: &str = "
Piece: main
---- BEFORE ----
fn before() { x + 1 }
---- AFTER ----
fn after() { x + 1 }";
    const RESPONSE_THREE: &str = "
Piece: prelude
---- BEFORE ----
let x = init();
---- AFTER ----
let x = init();

Piece: loop_body
---- BEFORE ----
while cond { f(x); }
---- AFTER ----
while cond { g(x); }

Piece: postlude
---- BEFORE ----
cleanup(x);
---- AFTER ----
cleanup(x);";
    const RESPONSE_ZERO_ASSIGN: &str = "
Piece: init
---- BEFORE ----
let x = 0;
---- AFTER ----
let x = 0;";
    const RESPONSE_MISSING_AFTER: &str = "
Piece: broken
---- BEFORE ----
fn before() { x + 1 }
---- AFTER ----";
    const RESPONSE_MISSING_BEFORE: &str = "
Piece: broken
---- BEFORE ----
---- AFTER ----
fn after() { x + 1 }";
    const LABEL_MAIN: &str = "main";
    const LABEL_PRELUDE: &str = "prelude";
    const LABEL_LOOP_BODY: &str = "loop_body";
    const LABEL_POSTLUDE: &str = "postlude";
    const BEFORE_FN: &str = "fn before";
    const AFTER_FN: &str = "fn after";
    const ZERO_ASSIGN: &str = "let x = 0;";
    const ONE_PIECE: usize = 1;
    const THREE_PIECES: usize = 3;
    const FIRST_PIECE: usize = 0;
    const SECOND_PIECE: usize = 1;
    const THIRD_PIECE: usize = 2;

    #[test]
    fn test_extract_single_piece() {
        let pm = DefaultPieceManager::new();
        let response = RESPONSE_SINGLE;
        let pieces = extract_split_pieces(response, &pm, ContextId::root());
        assert_eq!(pieces.len(), ONE_PIECE);
        assert_eq!(pieces[FIRST_PIECE].label(), LABEL_MAIN);
        assert!(pieces[FIRST_PIECE].before().contains(BEFORE_FN));
        assert!(pieces[FIRST_PIECE].after().contains(AFTER_FN));
    }

    #[test]
    fn test_extract_multiple_pieces() {
        let pm = DefaultPieceManager::new();
        let response = RESPONSE_THREE;
        let pieces = extract_split_pieces(response, &pm, ContextId::root());
        assert_eq!(pieces.len(), THREE_PIECES);
        assert_eq!(pieces[FIRST_PIECE].label(), LABEL_PRELUDE);
        assert_eq!(pieces[SECOND_PIECE].label(), LABEL_LOOP_BODY);
        assert_eq!(pieces[THIRD_PIECE].label(), LABEL_POSTLUDE);
    }

    #[test]
    fn test_extract_nested_piece_names() {
        let pm = DefaultPieceManager::new();
        let response = RESPONSE_ZERO_ASSIGN;
        let pieces = extract_split_pieces(response, &pm, ContextId::root());
        assert_eq!(pieces.len(), ONE_PIECE);
        assert_eq!(pieces[FIRST_PIECE].before(), ZERO_ASSIGN);
    }

    #[test]
    fn test_extract_piece_missing_after_is_rejected() {
        let pm = DefaultPieceManager::new();
        let response = RESPONSE_MISSING_AFTER;
        let pieces = extract_split_pieces(response, &pm, ContextId::root());
        assert!(pieces.is_empty(), "piece with empty AFTER should be rejected");
    }

    #[test]
    fn test_extract_piece_missing_before_is_rejected() {
        let pm = DefaultPieceManager::new();
        let response = RESPONSE_MISSING_BEFORE;
        let pieces = extract_split_pieces(response, &pm, ContextId::root());
        assert!(pieces.is_empty(), "piece with empty BEFORE should be rejected");
    }
}
