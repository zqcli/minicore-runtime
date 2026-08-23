use minicore_runtime::storage::SessionLogErrorKind;

use super::Script;

#[derive(Clone, Copy)]
pub(super) enum ScriptOutcome {
    Continue,
    Error(SessionLogErrorKind),
    UnknownOutcome { committed: bool },
}

pub(super) async fn run_script(script: Option<Script>) -> ScriptOutcome {
    match script {
        None | Some(Script::Continue) => ScriptOutcome::Continue,
        Some(Script::Error(kind)) => ScriptOutcome::Error(kind),
        Some(Script::UnknownOutcome { committed }) => ScriptOutcome::UnknownOutcome { committed },
        Some(Script::GateContinue(gate)) => {
            gate.block().await;
            ScriptOutcome::Continue
        }
        Some(Script::GateError(gate, kind)) => {
            gate.block().await;
            ScriptOutcome::Error(kind)
        }
        Some(Script::GateUnknownOutcome(gate, committed)) => {
            gate.block().await;
            ScriptOutcome::UnknownOutcome { committed }
        }
        Some(Script::Delay(duration)) => {
            tokio::time::sleep(duration).await;
            ScriptOutcome::Continue
        }
        Some(Script::Panic) => panic!("scripted fake SessionLog panic"),
    }
}
