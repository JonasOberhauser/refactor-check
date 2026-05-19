use anyhow::Result;
use tracing::{debug, warn};

use crate::provider::{LlmProvider, LlmRole};
use crate::states::{CodePiece, WaitForSplit};

pub async fn execute(
    state: &WaitForSplit,
    llm: &dyn LlmProvider,
) -> Result<Vec<CodePiece>> {
    let messages = build_split_messages(state);
    let response = llm.chat(LlmRole::Splitter, messages).await?;
    let pieces = extract_split_pieces(&response);

    if pieces.is_empty() {
        if state.pieces_to_resplit.is_empty() {
            debug!("splitter produced no pieces, falling back to full generation");
            Ok(vec![])
        } else {
            warn!("splitter produced no valid pieces for re-split");
            Err(anyhow::anyhow!("splitter produced no valid pieces for re-split"))
        }
    } else {
        debug!(count = pieces.len(), "splitter produced pieces");
        Ok(pieces)
    }
}

fn build_split_messages(state: &WaitForSplit) -> Vec<crate::llm::Message> {
    let system = crate::llm::system_message(
        "You are an expert in code refactoring analysis. Given a refactoring with BEFORE and AFTER \
         code, identify the largest matching fragments that can be independently verified for \
         equivalence by an SMT solver. \
         \n\nOutput format:\n\
         Piece: <label>\n\
         ---- BEFORE ----\n\
         <before code for this fragment>\n\
         ---- AFTER ----\n\
         <matching after code for this fragment>\n\n\
         Rules:\n\
         - If the refactoring is simple, output one piece.\n\
         - For loops, conditionals, or complex restructurings, split into matching fragments.\n\
         - Each BEFORE must have a matching AFTER.\n\
         - Labels must be short and descriptive.\n\
         - Prefer fewer larger pieces over many tiny ones.",
    );

    let prompt = if state.pieces_to_resplit.is_empty() {
        format!(
            "Split this refactoring into independently verifiable pieces:\n\n{}",
            state.input_content
        )
    } else {
        let mut prompt = String::from(
            "The following pieces timed out during SMT verification. Split each piece \
             into finer sub-pieces that the SMT solver can handle:\n\n",
        );
        for (i, (piece, reason)) in state.pieces_to_resplit.iter().enumerate() {
            prompt.push_str(&format!(
                "Timeout piece {}:\nLabel: {}\nBefore:\n{}\nAfter:\n{}\nReason: {}\n\n",
                i + 1,
                piece.label,
                piece.before,
                piece.after,
                reason,
            ));
        }
        prompt
    };

    vec![system, crate::llm::user_message(&prompt)]
}

fn extract_split_pieces(response: &str) -> Vec<CodePiece> {
    let mut pieces = Vec::new();
    let mut current_label = None;
    let mut current_section: Option<&mut String> = None;
    let mut before_buf = String::new();
    let mut after_buf = String::new();

    for line in response.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("Piece:") || trimmed.starts_with("Piece ") {
            if let Some(label) = current_label.take() {
                if !before_buf.trim().is_empty() || !after_buf.trim().is_empty() {
                    pieces.push(CodePiece {
                        label,
                        before: before_buf.trim().to_string(),
                        after: after_buf.trim().to_string(),
                    });
                }
                before_buf = String::new();
                after_buf = String::new();
            }
            let label = trimmed
                .trim_start_matches("Piece:")
                .trim_start_matches("Piece ")
                .trim()
                .trim_matches(':')
                .trim()
                .to_string();
            current_label = Some(label);
            current_section = None;
            continue;
        }

        if trimmed == "---- BEFORE ----" {
            current_section = Some(&mut before_buf);
            continue;
        }
        if trimmed == "---- AFTER ----" {
            current_section = Some(&mut after_buf);
            continue;
        }

        if let Some(ref mut buf) = current_section {
            buf.push_str(trimmed);
            buf.push('\n');
        }
    }

    if let Some(label) = current_label {
        if !before_buf.trim().is_empty() || !after_buf.trim().is_empty() {
            pieces.push(CodePiece {
                label,
                before: before_buf.trim().to_string(),
                after: after_buf.trim().to_string(),
            });
        }
    }

    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_single_piece() {
        let response = "\
Piece: main
---- BEFORE ----
fn before() { x + 1 }
---- AFTER ----
fn after() { x + 1 }";
        let pieces = extract_split_pieces(response);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].label, "main");
        assert!(pieces[0].before.contains("fn before"));
        assert!(pieces[0].after.contains("fn after"));
    }

    #[test]
    fn test_extract_multiple_pieces() {
        let response = "\
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
        let pieces = extract_split_pieces(response);
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[0].label, "prelude");
        assert_eq!(pieces[1].label, "loop_body");
        assert_eq!(pieces[2].label, "postlude");
    }

    #[test]
    fn test_extract_nested_piece_names() {
        let response = "\
Piece: init
---- BEFORE ----
let x = 0;
---- AFTER ----
let x = 0;";
        let pieces = extract_split_pieces(response);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].before, "let x = 0;");
    }
}