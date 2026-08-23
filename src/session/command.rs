use tokio::sync::oneshot;

use crate::config::{TurnOptions, UserInput};
use crate::conversation::{ConversationSeq, TranscriptPage};
use crate::error::SessionError;
use crate::ids::InteractionId;

use super::turn_handle::TurnHandle;
use crate::interaction::InteractionAnswer;

pub(crate) enum SessionCommand {
    Submit {
        input: UserInput,
        options: TurnOptions,
        reply: oneshot::Sender<Result<TurnHandle, SessionError>>,
    },
    Answer {
        interaction_id: InteractionId,
        answer: InteractionAnswer,
        reply: oneshot::Sender<Result<(), SessionError>>,
    },
    Transcript {
        after: Option<ConversationSeq>,
        limit: usize,
        reply: oneshot::Sender<Result<TranscriptPage, SessionError>>,
    },
}
