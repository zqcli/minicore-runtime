use serde::{Deserialize, Serialize};

use crate::ids::{InteractionId, TurnId};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running {
        turn_id: TurnId,
    },
    WaitingForInput {
        turn_id: TurnId,
        interaction_id: InteractionId,
    },
    Closing,
}

impl SessionStatus {
    pub const fn turn_id(self) -> Option<TurnId> {
        match self {
            Self::Idle | Self::Closing => None,
            Self::Running { turn_id } | Self::WaitingForInput { turn_id, .. } => Some(turn_id),
        }
    }

    pub const fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }
}
