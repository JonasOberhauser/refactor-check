use anyhow::Result;

use crate::provider::SolverProvider;
use crate::smt::SolverResult;

pub async fn execute(formula: &str, solver: &dyn SolverProvider) -> Result<SolverResult> {
    solver.run(formula).await
}
