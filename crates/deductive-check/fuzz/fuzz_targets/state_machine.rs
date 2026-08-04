#![no_main]

use deductive_check_fuzz::run_state_machine;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    run_state_machine(data);
});
