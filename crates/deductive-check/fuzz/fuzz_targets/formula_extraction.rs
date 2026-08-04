#![no_main]

use deductive_check::formula::{extract_fenced_blocks, extract_formulas_from_response};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let _ = extract_fenced_blocks(data);
    let _ = extract_formulas_from_response(data);
});
