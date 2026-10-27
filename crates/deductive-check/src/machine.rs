use anyhow::Result;
use std::sync::Mutex;
use tracing::info;

use servyi_states::checkpoint::{Handle, with_checkpoint_pair};
use servyi_states::state::{Context, ParallelState, State, Step};
use servyi_states::ContextId;

use crate::piece_manager::DeductivePieceManager;
use crate::provider::Providers;
use crate::result::VerificationResult;
use crate::states::{
    FormalizeBatch, FormalizeCollect, Landed, MachineEnv, PieceWork, PreflightGit,
    Signal, SplitBatch, SplitCollect, SplitWork,
};

/// Resume data carried around a pending fan-out phase, naming the
/// collector the landed results continue into.
enum Resume {
    Split(SplitBatch),
    Formalize(FormalizeBatch),
}

/// The root node: the linear machine plus, while a fan-out runs, its
/// phase. One phase slot with one landing type — the driver has
/// exactly one fan-out call site.
struct Workload {
    machine: Context<'static, Signal, MachineEnv<'static>>,
    phase: Option<ParallelState<'static, Landed, MachineEnv<'static>>>,
    resume: Option<Resume>,
}

/// The root task: the workload's ONE checkpoint pair. The driver
/// alternates linear transitions with fan-out phases; a pending phase
/// is handed to `run_children` — the phase's execute, which contains
/// the fan-out over its children — through the root handle's fanout,
/// so every child, down to individual pieces, is part of this single
/// tree and a checkpoint request parks it all from the root.
pub fn run(
    project_path: &str,
    providers: &'static Providers<'static>,
    pm: &'static dyn DeductivePieceManager,
) -> Result<VerificationResult> {
    let env = MachineEnv { providers, pm };
    let workload = Workload {
        machine: Context::new(
            ContextId::root(),
            Box::new(PreflightGit::new(std::path::PathBuf::from(project_path))),
        ),
        phase: None,
        resume: None,
    };
    with_checkpoint_pair(workload, |mut handle, _requester| loop {
        handle.safepoint();
        if handle.phase.is_some() {
            run_phase(&mut handle, &env)?;
        }
        match handle.machine.execute(&env)? {
            Step::Continue(next) => handle.machine.state = next,
            Step::Done(Signal::Finished(result)) => {
                info!(
                    total_pieces = result.total_pieces,
                    closed = result.closed_pieces.len(),
                    unverified = result.unverified_pieces.len(),
                    bugs = result.bug_reports.len(),
                    "Verification complete"
                );
                return Ok(result);
            }
            Step::Done(Signal::SplitFunctions(batch)) => {
                let (phase, resume) = build_split_phase(batch);
                handle.phase = Some(phase);
                handle.resume = Some(resume);
            }
            Step::Done(Signal::FormalizePieces(batch)) => {
                let (phase, resume) = build_formalize_phase(batch);
                handle.phase = Some(phase);
                handle.resume = Some(resume);
            }
        }
    })
}

/// Hand the pending phase to `run_children` through the root handle's
/// fanout: the ParallelState becomes a child handle, its `run_children`
/// contains the fan-out over the phase's children (one dedicated
/// thread each, results landing in their slots, first error aborts),
/// and the landed results install the collector named by the resume.
fn run_phase(handle: &mut Handle<'_, Workload>, env: &MachineEnv<'static>) -> Result<()> {
    let landed: Mutex<Option<Result<Vec<Landed>>>> = Mutex::new(None);
    handle.fanout(
        &|w: &mut Workload| match w.phase.as_mut() {
            Some(phase) => vec![phase],
            None => Vec::new(),
        },
        |mut phase: Handle<'_, ParallelState<'static, Landed, MachineEnv<'static>>>| {
            let result = phase.run_children(env);
            *landed.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        },
    );
    let landed = landed
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .expect("the pending phase always runs")?;
    handle.phase = None;
    match handle.resume.take() {
        Some(Resume::Split(resume)) => {
            handle.machine.state = Box::new(SplitCollect { resume, landed });
        }
        Some(Resume::Formalize(resume)) => {
            handle.machine.state = Box::new(FormalizeCollect { resume, landed });
        }
        None => {}
    }
    Ok(())
}

/// Spawn one splitter child per function; the batch's functions are
/// consumed, the rest rides along as the resume.
fn build_split_phase(mut batch: SplitBatch) -> (
    ParallelState<'static, Landed, MachineEnv<'static>>,
    Resume,
) {
    let mut phase = ParallelState::new(ContextId::root());
    for work in std::mem::take(&mut batch.functions) {
        phase.spawn(move |_id: &ContextId| {
            Box::new(SplitWork {
                file: work.file,
                function_id: work.function_id,
                body: work.body,
            }) as Box<dyn State<Landed, MachineEnv<'static>> + Send + Sync + 'static>
        });
    }
    (phase, Resume::Split(batch))
}

/// Spawn one formalizer child per piece; the batch's pieces are
/// consumed, the rest rides along as the resume.
fn build_formalize_phase(mut batch: FormalizeBatch) -> (
    ParallelState<'static, Landed, MachineEnv<'static>>,
    Resume,
) {
    let mut phase = ParallelState::new(ContextId::root());
    let verification_dir = batch.verification_dir.clone();
    for (piece, ctx) in std::mem::take(&mut batch.pieces) {
        let verification_dir = verification_dir.clone();
        phase.spawn(move |_id: &ContextId| {
            Box::new(PieceWork {
                piece,
                ctx: Some(ctx),
                verification_dir,
            }) as Box<dyn State<Landed, MachineEnv<'static>> + Send + Sync + 'static>
        });
    }
    (phase, Resume::Formalize(batch))
}
