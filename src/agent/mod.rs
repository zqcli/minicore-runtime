#[cfg(test)]
mod legacy;
mod runner_protocol;
mod tool_driver;

pub(crate) use runner_protocol::{SuspensionError, TurnSuspension, take_resume_for_actor};
pub(crate) use tool_driver::{
    ToolDriver, ToolDriverBuildError, ToolDriverConfig, ToolDriverProgress, ToolDriverResult,
};

#[cfg(test)]
pub(crate) use legacy::{
    MAX_RUNNER_EVENT_CAPACITY, RunnerEvent, RunnerEventSink, TimestampSource, TurnContext,
    TurnContextDependencies, TurnContextError, TurnFailure, TurnTaskResult, run_turn,
};

const _: () = {
    let _ = std::mem::size_of::<SuspensionError>();
    let _ = SuspensionError::stale_turn;
    let _ = std::mem::size_of::<TurnSuspension>();
    let _ = take_resume_for_actor;
    let _ = std::mem::size_of::<ToolDriver>();
    let _ = std::mem::size_of::<ToolDriverBuildError>();
    let _ = std::mem::size_of::<ToolDriverConfig>();
    let _ = std::mem::size_of::<ToolDriverProgress>();
    let _ = std::mem::size_of::<ToolDriverResult>();
    let _ = ToolDriverConfig::from_kernel_values;
    let _ = ToolDriverProgress::tool_call_id;
    let _ = ToolDriverProgress::progress;
    let _ = ToolDriverResult::output;
    let _ = ToolDriverResult::outcome;
    let _ = ToolDriver::new;
    let _ = ToolDriver::run;
};
