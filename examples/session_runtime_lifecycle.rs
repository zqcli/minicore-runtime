fn main() {}

// README_LIFECYCLE_START
use std::error::Error;

use minicore_runtime::{
    KernelConfig, SessionBindings, SessionEventEnvelope, SessionId, SessionLog, SessionRuntime,
    SessionRuntimeOptions, TurnOptions, TurnOutcome, UserInput,
};

pub fn render_event(_envelope: SessionEventEnvelope) {}

pub async fn run_loaded_session(
    session_id: SessionId,
    opened_log: Box<dyn SessionLog>,
    bindings: SessionBindings,
) -> Result<TurnOutcome, Box<dyn Error>> {
    let options = SessionRuntimeOptions::new(
        KernelConfig::default_checked()?,
        bindings,
        tokio::runtime::Handle::current(),
    )?;
    let mut session = SessionRuntime::load(session_id, opened_log, options).await?;

    let events_result = session.take_events();
    let handle = session.handle();
    let state_watch = handle.watch_state();
    let _initial_state = state_watch.borrow().clone();

    let (event_task, turn_result) = match events_result {
        Ok(mut events) => {
            let event_task = tokio::spawn(async move {
                while let Some(envelope) = events.recv().await {
                    render_event(envelope);
                }
            });
            let turn_result = match UserInput::text("Inspect the repository") {
                Ok(input) => match handle.submit(input, TurnOptions::default()).await {
                    Ok(turn) => turn
                        .wait()
                        .await
                        .map_err(|error| Box::new(error) as Box<dyn Error>),
                    Err(error) => Err(Box::new(error) as Box<dyn Error>),
                },
                Err(error) => Err(Box::new(error) as Box<dyn Error>),
            };
            (Some(event_task), turn_result)
        }
        Err(error) => (None, Err(Box::new(error) as Box<dyn Error>)),
    };

    let shutdown_result = session.shutdown().await;
    let event_result = match event_task {
        Some(task) => task
            .await
            .map_err(|error| Box::new(error) as Box<dyn Error>),
        None => Ok(()),
    };

    shutdown_result.map_err(|error| Box::new(error) as Box<dyn Error>)?;
    event_result?;
    turn_result
}
// README_LIFECYCLE_END
