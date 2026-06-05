# Plan: Cross-Piece Relations & No Import-Only Pieces

## Concept

Currently each piece must be strictly equivalent in isolation. This breaks when a
refactoring moves a variable across piece boundaries (e.g., `y` in before becomes
`z` in after). The fix is to let the splitter annotate **boundary relations** as
`/* relation: ... */` comments, the formalizer verifies the relation holds (not
strict equivalence), the splitting judge checks relations are consistent across
adjacent pieces, and the child judge verifies the formula encodes the relation
correctly.

## File Changes

### 1. `splitter.rs` — System prompt (`build_split_messages`)

Add two rules:

- **No import-only pieces**: "Do not create separate pieces for library
  include/import statements (e.g. `use`, `import`, `#include`) unless they might
  have side effects (macros expanding to code, module initialization). Merge
  import changes into the first substantive piece that uses them."

- **Boundary relation comments**: "When a split point creates a variable/state
  mapping gap between BEFORE and AFTER, add a single `/* relation: <description> */`
  comment at the end (or beginning) of the piece. The relation describes how
  states differ between before and after at this boundary. Example:
  `/* relation: after.z == before.y - 1 */` means the variable `z` in the after
  version corresponds to `y - 1` in the before version. Do not repeat the same
  comment in both BEFORE and AFTER — one comment per piece suffices.
  Variables/states not mentioned in a relation comment are assumed to be
  equivalent between before and after."

Update the output format example to include a relation comment.

### 2. `generation.rs` — Formalizer prompt (`build_single_piece_messages`)

Change the system message from "verify equivalence of this BEFORE/AFTER pair" to:

"If `/* relation: ... */` comments appear in the code, they specify how certain
states differ between before and after. Verify that the piece satisfies the
specified relation. All variables/states not mentioned in a relation comment
are assumed to be equivalent between before and after. The conjunction of all
piece relations must imply overall equivalence of the full refactoring. If no
relation comments are present, verify strict equivalence as before."

Also mention any relation comments explicitly in the user message.

### 3. `generation.rs` — Fixer/insist prompts (`build_retry_messages`, `build_retry_insist_messages`)

Same principle: tell the fixer to encode the relation, not just strict
equivalence, when relation comments are present.

### 4. `splitting_judge.rs` — Splitting judge prompt

Add evaluation criteria:

- "Check that `/* relation: ... */` comments are consistent across adjacent
  pieces. The relations must compose: if piece 1 has relation R1 and piece 2
  has relation R2, then R1 ∘ R2 must imply equivalence for the combined code."

- Relax "independently verifiable" to: "each piece should be independently
  verifiable, with any deviations from equivalence explicitly captured in a
  `/* relation: ... */` comment. Unmentioned variables/states are assumed
  equivalent."

### 5. `child_judge.rs` — Child judge prompt

Add: "If the piece contains `/* relation: ... */` comments, verify that the
formula correctly encodes the specified relation between before and after
states. Variables/states not mentioned in the relation are assumed equivalent
and should be encoded as such."

## No structural changes needed

| Component | Change needed? |
|-----------|---------------|
| `CodePiece` struct | No — relation comments are inline in `before`/`after` text |
| `extract_split_pieces` parser | No — comments inside BEFORE/AFTER blocks are already captured |
| State machine flow | No — same pipeline, just different LLM behavior |
| `states.rs`, `transitions.rs`, `results.rs` | No changes |

## Example of new output

```
Piece: variable setup
---- BEFORE ----
x = 1;
y = 2;
---- AFTER ----
x = 1;
z = 1;
/* relation: after.z == before.y - 1 */

Piece: usage
/* relation: after.z == before.y - 1 */
---- BEFORE ----
print(y);
---- AFTER ----
print(z + 1);
```

The formalizer for piece 1: `x` is assumed equivalent (not mentioned in the
relation), and the relation `after.z == before.y - 1` must hold. The formalizer
for piece 2, given the relation, proves `print(y)` ≡ `print(z+1)` since
`z+1 == (y-1)+1 == y`.
