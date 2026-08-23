#[cfg(test)]
mod legacy;
mod runner;
mod runner_protocol;
mod tool_driver;
mod turn_context;

pub(crate) use runner::run_turn;
pub(crate) use runner_protocol::{
    CommitAck, RunnerCommitError, RunnerEvent, RunnerOutcome, RunnerProgress, SuspensionError,
    TurnRunnerExit, TurnSuspension, take_commit_reply_for_actor, take_resume_for_actor,
};
pub(crate) use tool_driver::{
    ToolDriver, ToolDriverBuildError, ToolDriverConfig, ToolDriverProgress, ToolDriverResult,
};
pub(crate) use turn_context::{
    TurnRunnerControl, TurnRunnerIdentity, TurnRunnerKernel, TurnRunnerRequest,
    TurnRunnerRequestError,
};

#[cfg(test)]
pub(crate) use legacy::{
    MAX_RUNNER_EVENT_CAPACITY as LEGACY_MAX_RUNNER_EVENT_CAPACITY,
    RunnerEvent as LegacyRunnerEvent, RunnerEventSink as LegacyRunnerEventSink,
    TimestampSource as LegacyTimestampSource, TurnContext as LegacyTurnContext,
    TurnContextDependencies as LegacyTurnContextDependencies,
    TurnContextError as LegacyTurnContextError, TurnFailure as LegacyTurnFailure,
    TurnTaskResult as LegacyTurnTaskResult, run_turn as legacy_run_turn,
};

const _: () = {
    let _ = std::mem::size_of::<SuspensionError>();
    let _ = SuspensionError::stale_turn;
    let _ = std::mem::size_of::<TurnSuspension>();
    let _ = take_resume_for_actor;
    let _ = std::mem::size_of::<CommitAck>();
    let _ = std::mem::size_of::<RunnerCommitError>();
    // Temporary P4-C ownership anchors; the final actor replaces these references.
    let _ = [
        RunnerCommitError::Stale,
        RunnerCommitError::Degraded,
        RunnerCommitError::DurabilityUnavailable,
        RunnerCommitError::DurabilityUnknown,
        RunnerCommitError::RuntimeClosed,
    ];
    let _ = std::mem::size_of::<RunnerEvent>();
    let _ = take_commit_reply_for_actor;
    let _ = std::mem::size_of::<RunnerOutcome>();
    let _ = RunnerOutcome::usage;
    let _ = RunnerOutcome::diagnostic;
    let _ = std::mem::size_of::<RunnerProgress>();
    let _ = std::mem::size_of::<TurnRunnerExit>();
    let _ = std::mem::size_of::<TurnRunnerControl>();
    let _ = std::mem::size_of::<TurnRunnerIdentity>();
    let _ = std::mem::size_of::<TurnRunnerKernel>();
    let _ = std::mem::size_of::<TurnRunnerRequest>();
    let _ = std::mem::size_of::<TurnRunnerRequestError>();
    let _ = TurnRunnerKernel::from_kernel;
    let _ = TurnRunnerRequest::new;
    let _ = run_turn;
    let _ = std::mem::size_of::<ToolDriver>();
    let _ = std::mem::size_of::<ToolDriverBuildError>();
    let _ = std::mem::size_of::<ToolDriverConfig>();
    let _ = std::mem::size_of::<ToolDriverProgress>();
    let _ = std::mem::size_of::<ToolDriverResult>();
    let _ = ToolDriverConfig::from_kernel_values;
    let _ = ToolDriverProgress::tool_call_id;
    let _ = ToolDriverProgress::tool_name;
    let _ = ToolDriverProgress::progress;
    let _ = ToolDriverResult::output;
    let _ = ToolDriverResult::outcome;
    let _ = ToolDriver::new;
    let _ = ToolDriver::run;
};
