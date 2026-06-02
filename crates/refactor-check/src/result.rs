use refactor_check_core::smt::SolverOutcome;

pub struct FormulaResult {
    pub formula: String,
    pub piece_id: u64,
    pub piece_label: String,
    pub outcome: SolverOutcome,
    pub verdict: String,
    pub explanation: Option<String>,
}

pub struct AgentResult {
    pub formulas: Vec<FormulaResult>,
    pub overall_equivalent: bool,
    pub open_count: usize,
    pub reasonable_sat: usize,
    pub reasonable_unsat: usize,
    pub reasonable_unknown: usize,
}
