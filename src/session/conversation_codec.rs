use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;

use serde::ser::{Error as _, SerializeStruct};
use serde::{Serialize, Serializer};

use super::{
    ConversationEntry, ConversationError, ConversationState, MAX_FILE_BYTES, MAX_LINE_BYTES,
};
use crate::session_v2::store::StoreError;

impl From<StoreError> for ConversationError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound => Self::NotFound,
            StoreError::Busy => Self::Busy,
            StoreError::Closing => Self::Closing,
            StoreError::WorkerFailed => Self::WorkerFailed,
            StoreError::TooLarge => Self::TooLarge,
            StoreError::Corrupt => Self::Corrupt,
            StoreError::ConversationCorrupt { line, offset } => Self::CorruptAt { line, offset },
            StoreError::InUse
            | StoreError::AlreadyExists
            | StoreError::InvalidConfig
            | StoreError::CleanupFailed
            | StoreError::Io => Self::Io,
        }
    }
}

impl ConversationError {
    pub(crate) const fn location(&self) -> Option<(u64, u64)> {
        match self {
            Self::CorruptAt { line, offset } => Some((*line, *offset)),
            Self::InvalidEntry
            | Self::Corrupt
            | Self::TooLarge
            | Self::Busy
            | Self::Closing
            | Self::Io
            | Self::WorkerFailed
            | Self::NotFound
            | Self::InvalidPage
            | Self::Degraded => None,
        }
    }

    pub(crate) const fn line(&self) -> Option<u64> {
        match self {
            Self::CorruptAt { line, .. } => Some(*line),
            _ => None,
        }
    }

    pub(crate) const fn offset(&self) -> Option<u64> {
        match self {
            Self::CorruptAt { offset, .. } => Some(*offset),
            _ => None,
        }
    }
}

impl Serialize for ConversationEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate_shape().map_err(S::Error::custom)?;
        match self {
            Self::User {
                seq,
                turn_id,
                timestamp,
                text,
            } => {
                let mut state = serializer.serialize_struct("ConversationEntry", 5)?;
                state.serialize_field("type", "user")?;
                state.serialize_field("seq", seq)?;
                state.serialize_field("turn_id", turn_id)?;
                state.serialize_field("timestamp", timestamp)?;
                state.serialize_field("text", text)?;
                state.end()
            }
            Self::Assistant {
                seq,
                turn_id,
                timestamp,
                text,
                reasoning,
                tool_calls,
                usage,
            } => {
                let mut state = serializer.serialize_struct("ConversationEntry", 8)?;
                state.serialize_field("type", "assistant")?;
                state.serialize_field("seq", seq)?;
                state.serialize_field("turn_id", turn_id)?;
                state.serialize_field("timestamp", timestamp)?;
                state.serialize_field("text", text)?;
                state.serialize_field("reasoning", reasoning)?;
                state.serialize_field("tool_calls", tool_calls)?;
                state.serialize_field("usage", usage)?;
                state.end()
            }
            Self::ToolResult {
                seq,
                turn_id,
                timestamp,
                call_id,
                result,
            } => {
                let mut state = serializer.serialize_struct("ConversationEntry", 6)?;
                state.serialize_field("type", "tool_result")?;
                state.serialize_field("seq", seq)?;
                state.serialize_field("turn_id", turn_id)?;
                state.serialize_field("timestamp", timestamp)?;
                state.serialize_field("call_id", call_id)?;
                state.serialize_field("result", result)?;
                state.end()
            }
            Self::Interaction {
                seq,
                turn_id,
                timestamp,
                interaction_id,
                question,
                answer,
            } => {
                let mut state = serializer.serialize_struct("ConversationEntry", 7)?;
                state.serialize_field("type", "interaction")?;
                state.serialize_field("seq", seq)?;
                state.serialize_field("turn_id", turn_id)?;
                state.serialize_field("timestamp", timestamp)?;
                state.serialize_field("interaction_id", interaction_id)?;
                state.serialize_field("question", question)?;
                state.serialize_field("answer", answer)?;
                state.end()
            }
            Self::Summary {
                seq,
                timestamp,
                through_seq,
                text,
            } => {
                let mut state = serializer.serialize_struct("ConversationEntry", 5)?;
                state.serialize_field("type", "summary")?;
                state.serialize_field("seq", seq)?;
                state.serialize_field("timestamp", timestamp)?;
                state.serialize_field("through_seq", through_seq)?;
                state.serialize_field("text", text)?;
                state.end()
            }
            Self::TurnTerminal {
                seq,
                turn_id,
                timestamp,
                outcome,
            } => {
                let mut state = serializer.serialize_struct("ConversationEntry", 5)?;
                state.serialize_field("type", "turn_terminal")?;
                state.serialize_field("seq", seq)?;
                state.serialize_field("turn_id", turn_id)?;
                state.serialize_field("timestamp", timestamp)?;
                state.serialize_field("outcome", outcome)?;
                state.end()
            }
        }
    }
}

pub(super) fn read_replay_file(path: &Path) -> Result<ConversationState, StoreError> {
    let file = File::open(path).map_err(map_conversation_io)?;
    let mut reader = BufReader::with_capacity(MAX_LINE_BYTES + 1, file);
    let mut line = Vec::with_capacity(MAX_LINE_BYTES);
    let mut total_bytes = 0_u64;
    let mut line_start = 0_u64;
    let mut line_number = 1_u64;
    let mut state = ConversationState::empty();

    loop {
        let (take_len, complete) = {
            let buffer = reader.fill_buf().map_err(map_conversation_io)?;
            if buffer.is_empty() {
                break;
            }
            let take_len = buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(buffer.len(), |index| index + 1);
            let next_total = total_bytes
                .checked_add(take_len as u64)
                .ok_or(StoreError::TooLarge)?;
            if next_total > MAX_FILE_BYTES as u64 {
                return Err(StoreError::TooLarge);
            }
            let line_len = line
                .len()
                .checked_add(take_len)
                .ok_or(StoreError::TooLarge)?;
            if line_len > MAX_LINE_BYTES {
                return Err(StoreError::TooLarge);
            }
            (
                take_len,
                take_len <= buffer.len() && buffer[take_len - 1] == b'\n',
            )
        };
        let buffer = reader.fill_buf().map_err(map_conversation_io)?;
        line.extend_from_slice(&buffer[..take_len]);
        reader.consume(take_len);
        total_bytes = total_bytes
            .checked_add(take_len as u64)
            .ok_or(StoreError::TooLarge)?;
        if complete {
            parse_complete_line(&line, &mut state, line_number, line_start)?;
            line.clear();
            line_start = total_bytes;
            line_number = line_number.checked_add(1).ok_or(StoreError::TooLarge)?;
        }
    }

    if !line.is_empty() {
        truncate_partial_tail(path, line_start)?;
        state.partial_tail = true;
    }
    Ok(state)
}

fn parse_complete_line(
    line: &[u8],
    state: &mut ConversationState,
    line_number: u64,
    line_start: u64,
) -> Result<(), StoreError> {
    let content = &line[..line.len() - 1];
    let content = content.strip_suffix(b"\r").unwrap_or(content);
    if content.is_empty() {
        return Err(StoreError::ConversationCorrupt {
            line: line_number,
            offset: line_start,
        });
    }
    let entry = serde_json::from_slice::<ConversationEntry>(content).map_err(|_| {
        StoreError::ConversationCorrupt {
            line: line_number,
            offset: line_start,
        }
    })?;
    state
        .apply(Arc::new(entry))
        .map_err(|error| map_state_error(error, line_number, line_start))
}

pub(super) fn map_state_error(error: ConversationError, line: u64, offset: u64) -> StoreError {
    match error {
        ConversationError::TooLarge => StoreError::TooLarge,
        _ => StoreError::ConversationCorrupt { line, offset },
    }
}

fn truncate_partial_tail(path: &Path, length: u64) -> Result<(), StoreError> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(map_conversation_io)?;
    file.set_len(length).map_err(map_conversation_io)?;
    file.flush().map_err(map_conversation_io)?;
    file.sync_data().map_err(map_conversation_io)
}

pub(super) fn encode_line(
    entry: &ConversationEntry,
    line_number: u64,
    line_offset: u64,
) -> Result<Vec<u8>, StoreError> {
    let mut line = serde_json::to_vec(entry).map_err(|_| StoreError::ConversationCorrupt {
        line: line_number,
        offset: line_offset,
    })?;
    line.push(b'\n');
    if line.len() > MAX_LINE_BYTES {
        return Err(StoreError::TooLarge);
    }
    Ok(line)
}

pub(super) fn append_line_sync(path: &Path, line: &[u8]) -> Result<(), StoreError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(StoreError::TooLarge);
    }
    append_bytes_sync(path, line)
}

pub(super) fn append_bytes_sync(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(map_conversation_io)?;
    let current = file.metadata().map_err(map_conversation_io)?.len();
    let final_len = current
        .checked_add(bytes.len() as u64)
        .ok_or(StoreError::TooLarge)?;
    if final_len > MAX_FILE_BYTES as u64 {
        return Err(StoreError::TooLarge);
    }
    file.write_all(bytes).map_err(map_conversation_io)?;
    file.flush().map_err(map_conversation_io)?;
    file.sync_data().map_err(map_conversation_io)
}

pub(super) fn file_len(path: &Path) -> Result<u64, StoreError> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(map_conversation_io)
}

fn map_conversation_io(error: io::Error) -> StoreError {
    match error.kind() {
        io::ErrorKind::NotFound => StoreError::NotFound,
        _ => StoreError::Io,
    }
}
