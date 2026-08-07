use std::fmt;
#[cfg(feature = "heavy-tests")]
use std::fs::File;
use std::io::Read;

use thiserror::Error;

use crate::conversation_storage::{
    ExclusiveWritableConversationLease, SessionHeader, StoredSessionEntry,
};
use crate::wire::SessionId;
use crate::wire::conversation_jsonl::{
    ConversationCodecError, ConversationDecodeFacts, ConversationLineCodec,
    MAX_CONVERSATION_ENTRY_BYTES, MAX_CONVERSATION_HEADER_BYTES,
};

/// The V1 physical-file cap, including any final unterminated bytes.
pub(crate) const MAX_CONVERSATION_FILE_BYTES: u64 = 1_073_741_824;
/// The V1 cap for newline-terminated physical lines after the Header.
pub(crate) const MAX_CONVERSATION_ENTRY_RECORDS: u64 = 1_000_000;

const SCANNER_CHUNK_BYTES: usize = 65_536;

/// Scan access is read-only unless the caller borrows a storage-owned proof for this exact
/// physical file observation. The scanner never acquires an OS lock and never mutates the file.
#[allow(dead_code, reason = "writable lease access remains a storage seam")]
pub(crate) enum ConversationScanAccess<'lease> {
    ReadOnly,
    /// The caller has already established exclusive writable access. The scanner does not create
    /// or acquire a lease; it only returns a safe truncation offset for a final partial tail.
    ExclusiveWritable(&'lease ExclusiveWritableConversationLease),
}

impl ConversationScanAccess<'_> {
    fn verify(
        &self,
        opened_session_id: SessionId,
        declared_file_bytes: u64,
    ) -> Result<(), ConversationScanError> {
        match self {
            Self::ReadOnly => Ok(()),
            Self::ExclusiveWritable(lease)
                if lease.session_id() == opened_session_id
                    && lease.declared_file_bytes() == declared_file_bytes =>
            {
                Ok(())
            }
            Self::ExclusiveWritable(_) => Err(ConversationScanError::LeaseMismatch),
        }
    }

    const fn permits_tail_truncation(&self) -> bool {
        matches!(self, Self::ExclusiveWritable(_))
    }

    const fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConversationPhysicalLocation {
    line_number: u64,
    offset: u64,
}

#[allow(dead_code, reason = "locations remain a scanner consumer seam")]
impl ConversationPhysicalLocation {
    pub(crate) const fn new(line_number: u64, offset: u64) -> Self {
        Self {
            line_number,
            offset,
        }
    }

    pub(crate) const fn line_number(self) -> u64 {
        self.line_number
    }

    pub(crate) const fn offset(self) -> u64 {
        self.offset
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConversationLineFault {
    OversizedLine,
    InvalidUtf8,
    MalformedJson,
    InvalidEntry,
    UnknownRecordVariant,
    UnknownEntryVariant,
    SessionMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConversationPartialTailAction {
    Ignore,
    TruncateTo { offset: u64 },
}

/// Whether a successfully decoded physical JSONL record exactly matches the V1 writer form.
/// This is a bounded fact only: the scanner never retains or exposes the physical line itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConversationLineCanonicality {
    Canonical,
    NonCanonical,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ConversationScanEvent {
    Entry {
        location: ConversationPhysicalLocation,
        canonicality: ConversationLineCanonicality,
        entry: Box<StoredSessionEntry>,
        /// Bounded salvage facts counted by the codec for this exact line. The scanner retains
        /// no raw line bytes; only these counts cross the event boundary.
        decode_facts: ConversationDecodeFacts,
    },
    Fault {
        location: ConversationPhysicalLocation,
        fault: ConversationLineFault,
    },
    PartialTail {
        location: ConversationPhysicalLocation,
        action: ConversationPartialTailAction,
    },
}

impl fmt::Debug for ConversationScanEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Entry {
                location,
                canonicality,
                decode_facts,
                ..
            } => formatter
                .debug_struct("ConversationScanEvent::Entry")
                .field("location", location)
                .field("canonicality", canonicality)
                .field("decode_facts", decode_facts)
                .finish(),
            Self::Fault { location, fault } => formatter
                .debug_struct("ConversationScanEvent::Fault")
                .field("location", location)
                .field("fault", fault)
                .finish(),
            Self::PartialTail { location, action } => formatter
                .debug_struct("ConversationScanEvent::PartialTail")
                .field("location", location)
                .field("action", action)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum ConversationScanError {
    #[error("conversation history exceeds the V1 physical file limit")]
    FileTooLarge,
    #[error("conversation history has too many complete entry records")]
    HistoryTooLarge,
    #[error("conversation header is corrupt")]
    HeaderCorrupt { code: ConversationCodecError },
    #[error("conversation header has an unsupported format version")]
    UnsupportedFormatVersion,
    #[error("conversation file does not contain a complete Header")]
    MissingHeader,
    #[error("conversation writable lease does not match the opened file")]
    LeaseMismatch,
    #[error("conversation file changed after its metadata was read")]
    InputChanged,
    #[error("conversation scanner input is unavailable")]
    InputUnavailable,
    #[error("conversation scanner counter overflow")]
    CounterOverflow,
    #[error("conversation scanner invariant was violated")]
    InvariantViolation,
}

#[derive(Clone, Copy)]
struct ConversationScanLimits {
    file_bytes: u64,
    header_bytes: usize,
    entry_bytes: usize,
    entry_records: u64,
}

impl ConversationScanLimits {
    const V1: Self = Self {
        file_bytes: MAX_CONVERSATION_FILE_BYTES,
        header_bytes: MAX_CONVERSATION_HEADER_BYTES,
        entry_bytes: MAX_CONVERSATION_ENTRY_BYTES,
        entry_records: MAX_CONVERSATION_ENTRY_RECORDS,
    };
}

enum PhysicalLineRead {
    Complete {
        offset: u64,
        oversized: bool,
        exact_lf: bool,
    },
    Partial {
        offset: u64,
        oversized: bool,
    },
    Oversized,
    End,
}

/// Streaming physical V1 JSONL scanner.
///
/// `open` checks a known declared physical length before it asks `reader` for a byte. The
/// read-only metadata-unavailable seam instead applies the same cap while it streams. After its
/// strict Header is available, `next_event` yields ordered decoded entries, complete-line faults,
/// and at most one final partial-tail action. It retains no raw line data after yielding an event.
pub(crate) struct ConversationJsonlScanner<'lease, R> {
    reader: R,
    declared_file_bytes: Option<u64>,
    access: ConversationScanAccess<'lease>,
    limits: ConversationScanLimits,
    header: Option<SessionHeader>,
    header_is_canonical: bool,
    chunk: [u8; SCANNER_CHUNK_BYTES],
    chunk_position: usize,
    chunk_length: usize,
    read_bytes: u64,
    consumed_bytes: u64,
    last_lf_end_offset: u64,
    line_buffer: Vec<u8>,
    next_line_number: u64,
    complete_entry_records: u64,
    finished: bool,
}

impl<R> fmt::Debug for ConversationJsonlScanner<'_, R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConversationJsonlScanner")
            .field("declared_file_bytes", &self.declared_file_bytes)
            .field(
                "has_exclusive_writable_lease",
                &self.access.permits_tail_truncation(),
            )
            .field("next_line_number", &self.next_line_number)
            .field("complete_entry_records", &self.complete_entry_records)
            .field("finished", &self.finished)
            .finish()
    }
}

impl<'lease, R: Read> ConversationJsonlScanner<'lease, R> {
    pub(crate) fn open(
        reader: R,
        declared_file_bytes: u64,
        opened_session_id: SessionId,
        access: ConversationScanAccess<'lease>,
    ) -> Result<Self, ConversationScanError> {
        Self::open_with_limits(
            reader,
            Some(declared_file_bytes),
            opened_session_id,
            access,
            ConversationScanLimits::V1,
        )
    }

    /// Opens a readable input when its metadata length is unavailable.
    ///
    /// This seam intentionally accepts no scan access: metadata-unavailable input is always
    /// read-only, so it cannot manufacture authorization to truncate a partial tail.
    #[allow(
        dead_code,
        reason = "M3.2 production scanner contract supports stat-unavailable read-only inputs"
    )]
    pub(crate) fn open_read_only_without_metadata(
        reader: R,
        opened_session_id: SessionId,
    ) -> Result<Self, ConversationScanError> {
        Self::open_with_limits(
            reader,
            None,
            opened_session_id,
            ConversationScanAccess::ReadOnly,
            ConversationScanLimits::V1,
        )
    }

    fn open_with_limits(
        reader: R,
        declared_file_bytes: Option<u64>,
        opened_session_id: SessionId,
        access: ConversationScanAccess<'lease>,
        limits: ConversationScanLimits,
    ) -> Result<Self, ConversationScanError> {
        if declared_file_bytes.is_some_and(|bytes| bytes > limits.file_bytes) {
            return Err(ConversationScanError::FileTooLarge);
        }
        if let Some(declared_file_bytes) = declared_file_bytes {
            access.verify(opened_session_id, declared_file_bytes)?;
        } else if !access.is_read_only() {
            return Err(ConversationScanError::InputUnavailable);
        }

        let mut scanner = Self {
            reader,
            declared_file_bytes,
            access,
            limits,
            header: None,
            header_is_canonical: false,
            chunk: [0; SCANNER_CHUNK_BYTES],
            chunk_position: 0,
            chunk_length: 0,
            read_bytes: 0,
            consumed_bytes: 0,
            last_lf_end_offset: 0,
            line_buffer: Vec::with_capacity(
                limits
                    .header_bytes
                    .checked_add(1)
                    .ok_or(ConversationScanError::CounterOverflow)?,
            ),
            next_line_number: 1,
            complete_entry_records: 0,
            finished: false,
        };

        let header_read = scanner.read_physical_line(limits.header_bytes, true)?;
        let (header_offset, header_exact_lf) = match header_read {
            PhysicalLineRead::Complete {
                offset,
                oversized,
                exact_lf,
            } => {
                if oversized {
                    return Err(scanner.header_error(ConversationCodecError::HeaderTooLarge));
                }
                (offset, exact_lf)
            }
            PhysicalLineRead::Oversized => {
                return Err(scanner.header_error(ConversationCodecError::HeaderTooLarge));
            }
            PhysicalLineRead::Partial {
                oversized: true, ..
            } => {
                return Err(scanner.header_error(ConversationCodecError::HeaderTooLarge));
            }
            PhysicalLineRead::Partial {
                oversized: false, ..
            }
            | PhysicalLineRead::End => {
                return Err(ConversationScanError::MissingHeader);
            }
        };
        if header_offset != 0 {
            return Err(ConversationScanError::InvariantViolation);
        }
        let header = ConversationLineCodec::decode_header_for_catalog(
            &scanner.line_buffer,
            opened_session_id,
        )
        .map_err(|error| scanner.header_error(error))?;
        scanner.header_is_canonical = header_exact_lf
            && ConversationLineCodec::encode_header(&header)
                .is_ok_and(|encoded| encoded == scanner.line_buffer);
        scanner.header = Some(header);
        scanner.next_line_number = 2;
        Ok(scanner)
    }

    pub(crate) fn header(&self) -> Result<&SessionHeader, ConversationScanError> {
        self.header
            .as_ref()
            .ok_or(ConversationScanError::InvariantViolation)
    }

    #[cfg(any(test, feature = "heavy-tests"))]
    pub(crate) const fn complete_entry_records(&self) -> u64 {
        self.complete_entry_records
    }

    /// Physical canonicality of the successfully decoded Header. It contains no Header fields.
    pub(crate) const fn header_is_canonical(&self) -> bool {
        self.header_is_canonical
    }

    pub(crate) fn next_event(
        &mut self,
    ) -> Result<Option<ConversationScanEvent>, ConversationScanError> {
        if self.finished {
            return Ok(None);
        }

        let line_read = self.read_physical_line(self.limits.entry_bytes, false)?;
        match line_read {
            PhysicalLineRead::End => {
                self.finished = true;
                Ok(None)
            }
            PhysicalLineRead::Partial { offset, .. } => {
                self.finished = true;
                let location = self.current_location(offset)?;
                let action = if self.access.permits_tail_truncation() {
                    ConversationPartialTailAction::TruncateTo {
                        offset: self.last_lf_end_offset,
                    }
                } else {
                    ConversationPartialTailAction::Ignore
                };
                Ok(Some(ConversationScanEvent::PartialTail {
                    location,
                    action,
                }))
            }
            PhysicalLineRead::Complete {
                offset,
                oversized,
                exact_lf,
            } => {
                let location = self.current_location(offset)?;
                self.advance_complete_entry_record()?;
                if oversized {
                    return Ok(Some(ConversationScanEvent::Fault {
                        location,
                        fault: ConversationLineFault::OversizedLine,
                    }));
                }
                if std::str::from_utf8(&self.line_buffer).is_err() {
                    return Ok(Some(ConversationScanEvent::Fault {
                        location,
                        fault: ConversationLineFault::InvalidUtf8,
                    }));
                }
                let mut facts = ConversationDecodeFacts::default();
                match ConversationLineCodec::decode_entry_for_session_with_facts(
                    &self.line_buffer,
                    self.header()?.session_id(),
                    &mut facts,
                ) {
                    Ok(entry) => {
                        let canonicality = if exact_lf
                            && ConversationLineCodec::encode_entry(&entry)
                                .is_ok_and(|encoded| encoded == self.line_buffer)
                        {
                            ConversationLineCanonicality::Canonical
                        } else {
                            ConversationLineCanonicality::NonCanonical
                        };
                        Ok(Some(ConversationScanEvent::Entry {
                            location,
                            canonicality,
                            entry: Box::new(entry),
                            decode_facts: facts,
                        }))
                    }
                    Err(error) => Ok(Some(ConversationScanEvent::Fault {
                        location,
                        fault: entry_fault(error),
                    })),
                }
            }
            PhysicalLineRead::Oversized => Err(ConversationScanError::InvariantViolation),
        }
    }

    fn header_error(&self, code: ConversationCodecError) -> ConversationScanError {
        if code == ConversationCodecError::UnsupportedFormatVersion {
            ConversationScanError::UnsupportedFormatVersion
        } else {
            ConversationScanError::HeaderCorrupt { code }
        }
    }

    fn current_location(
        &self,
        offset: u64,
    ) -> Result<ConversationPhysicalLocation, ConversationScanError> {
        if self.next_line_number == 0 {
            return Err(ConversationScanError::CounterOverflow);
        }
        Ok(ConversationPhysicalLocation::new(
            self.next_line_number,
            offset,
        ))
    }

    fn advance_complete_entry_record(&mut self) -> Result<(), ConversationScanError> {
        let next_count = self
            .complete_entry_records
            .checked_add(1)
            .ok_or(ConversationScanError::CounterOverflow)?;
        if next_count > self.limits.entry_records {
            return Err(ConversationScanError::HistoryTooLarge);
        }
        self.complete_entry_records = next_count;
        self.next_line_number = self
            .next_line_number
            .checked_add(1)
            .ok_or(ConversationScanError::CounterOverflow)?;
        Ok(())
    }

    fn read_physical_line(
        &mut self,
        maximum_content_bytes: usize,
        stop_on_oversize: bool,
    ) -> Result<PhysicalLineRead, ConversationScanError> {
        let retained_bytes = maximum_content_bytes
            .checked_add(1)
            .ok_or(ConversationScanError::CounterOverflow)?;
        if self.line_buffer.capacity() < retained_bytes {
            self.line_buffer = Vec::with_capacity(retained_bytes);
        }
        self.line_buffer.clear();
        let offset = self.consumed_bytes;
        let mut saw_non_newline_byte = false;
        let mut oversized = false;

        loop {
            let Some(byte) = self.read_byte()? else {
                return if saw_non_newline_byte {
                    Ok(PhysicalLineRead::Partial {
                        offset,
                        oversized: oversized || self.line_buffer.len() > maximum_content_bytes,
                    })
                } else {
                    Ok(PhysicalLineRead::End)
                };
            };
            if byte == b'\n' {
                self.last_lf_end_offset = self.consumed_bytes;
                let exact_lf = self.line_buffer.last() != Some(&b'\r');
                if !oversized && self.line_buffer.last() == Some(&b'\r') {
                    self.line_buffer.pop();
                }
                return Ok(PhysicalLineRead::Complete {
                    offset,
                    oversized: oversized || self.line_buffer.len() > maximum_content_bytes,
                    exact_lf,
                });
            }

            saw_non_newline_byte = true;
            if !oversized {
                if self.line_buffer.len() < retained_bytes {
                    self.line_buffer.push(byte);
                } else {
                    oversized = true;
                    if stop_on_oversize {
                        return Ok(PhysicalLineRead::Oversized);
                    }
                }
            }
        }
    }

    fn read_byte(&mut self) -> Result<Option<u8>, ConversationScanError> {
        if self.chunk_position == self.chunk_length {
            let remaining_before_file_cap = self
                .limits
                .file_bytes
                .checked_sub(self.read_bytes)
                .ok_or(ConversationScanError::InvariantViolation)?;
            let maximum_read = remaining_before_file_cap
                .checked_add(1)
                .ok_or(ConversationScanError::CounterOverflow)?
                .min(SCANNER_CHUNK_BYTES as u64);
            let read_length = usize::try_from(maximum_read)
                .map_err(|_| ConversationScanError::CounterOverflow)?;
            let read = self
                .reader
                .read(&mut self.chunk[..read_length])
                .map_err(|_| ConversationScanError::InputUnavailable)?;
            if read > read_length {
                return Err(ConversationScanError::InputUnavailable);
            }
            if read == 0 {
                return match self.declared_file_bytes {
                    Some(declared_file_bytes) if self.read_bytes != declared_file_bytes => {
                        Err(ConversationScanError::InputChanged)
                    }
                    Some(_) | None => Ok(None),
                };
            }
            self.read_bytes = self
                .read_bytes
                .checked_add(
                    u64::try_from(read).map_err(|_| ConversationScanError::CounterOverflow)?,
                )
                .ok_or(ConversationScanError::CounterOverflow)?;
            if self.read_bytes > self.limits.file_bytes {
                return Err(ConversationScanError::FileTooLarge);
            }
            self.chunk_position = 0;
            self.chunk_length = read;
        }

        let byte = self.chunk[self.chunk_position];
        self.chunk_position = self
            .chunk_position
            .checked_add(1)
            .ok_or(ConversationScanError::CounterOverflow)?;
        self.consumed_bytes = self
            .consumed_bytes
            .checked_add(1)
            .ok_or(ConversationScanError::CounterOverflow)?;
        Ok(Some(byte))
    }
}

#[cfg(feature = "heavy-tests")]
impl<'lease> ConversationJsonlScanner<'lease, File> {
    pub(crate) fn open_file(
        file: File,
        opened_session_id: SessionId,
        access: ConversationScanAccess<'lease>,
    ) -> Result<Self, ConversationScanError> {
        match file.metadata() {
            Ok(metadata) => Self::open(file, metadata.len(), opened_session_id, access),
            Err(_) if access.is_read_only() => {
                Self::open_read_only_without_metadata(file, opened_session_id)
            }
            Err(_) => Err(ConversationScanError::InputUnavailable),
        }
    }
}

fn entry_fault(error: ConversationCodecError) -> ConversationLineFault {
    match error {
        ConversationCodecError::JsonSyntax => ConversationLineFault::MalformedJson,
        ConversationCodecError::UnknownRecordVariant => ConversationLineFault::UnknownRecordVariant,
        ConversationCodecError::UnknownBodyVariant | ConversationCodecError::UnknownLeafVariant => {
            ConversationLineFault::UnknownEntryVariant
        }
        ConversationCodecError::SessionIdentityMismatch => ConversationLineFault::SessionMismatch,
        ConversationCodecError::LineTooLarge
        | ConversationCodecError::HeaderTooLarge
        | ConversationCodecError::JsonStructure
        | ConversationCodecError::InvalidShape
        | ConversationCodecError::MissingRequiredField
        | ConversationCodecError::InvalidScalar
        | ConversationCodecError::UnsupportedFormatVersion
        | ConversationCodecError::InvalidSemantic
        | ConversationCodecError::UnexpectedRecordKind => ConversationLineFault::InvalidEntry,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read};

    use serde_json::{Value, json};

    use super::*;
    use crate::wire::conversation_jsonl::ConversationRecord;

    const HEADER: &[u8] =
        include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/header-only.jsonl");
    const TOOL_EXCHANGE: &[u8] =
        include_bytes!("../../docs/fixtures/wire-v1/conversation/golden/tool-exchange.jsonl");

    type StructuralValueGenerator = fn(usize) -> Vec<u8>;

    fn entry_line() -> &'static [u8] {
        TOOL_EXCHANGE
            .split(|byte| *byte == b'\n')
            .nth(1)
            .expect("golden tool exchange has a first entry")
    }

    fn runtime_id(prefix: &str, counter: u128) -> String {
        assert_ne!(
            counter, 0,
            "generated runtime IDs must not use all-zero payloads"
        );
        format!("{prefix}_{counter:032x}")
    }

    fn canonical_root_input_user_entry_line(counter: u128) -> Vec<u8> {
        let entry_id = runtime_id("ent", counter);
        let turn_id = runtime_id("trn", counter);
        let item_id = runtime_id("itm", counter);
        format!(
            "{{\"type\":\"entry\",\"data\":{{\"entryId\":\"{entry_id}\",\"parentId\":null,\"sessionId\":\"ses_11111111111111111111111111111111\",\"turnId\":\"{turn_id}\",\"timestamp\":\"2026-07-31T12:00:01.000Z\",\"body\":{{\"type\":\"user_message\",\"data\":{{\"itemId\":\"{item_id}\",\"source\":\"input\",\"content\":{{\"parts\":[{{\"type\":\"text\",\"data\":{{\"text\":\"x\"}}}}],\"contributionStamps\":[]}}}}}}}}}}"
        )
        .into_bytes()
    }

    fn session_id() -> SessionId {
        "ses_11111111111111111111111111111111"
            .parse()
            .expect("fixture session ID is valid")
    }

    fn scanner<'lease>(
        bytes: Vec<u8>,
        access: ConversationScanAccess<'lease>,
    ) -> ConversationJsonlScanner<'lease, Cursor<Vec<u8>>> {
        let length = u64::try_from(bytes.len()).expect("test file length fits u64");
        ConversationJsonlScanner::open(Cursor::new(bytes), length, session_id(), access)
            .expect("test file must have a strict Header")
    }

    fn assert_open_error<R: Read>(
        result: Result<ConversationJsonlScanner<'_, R>, ConversationScanError>,
        expected: ConversationScanError,
    ) {
        match result {
            Err(actual) => assert_eq!(actual, expected),
            Ok(_) => panic!("scanner unexpectedly opened an invalid file"),
        }
    }

    fn drain<R: Read>(scanner: &mut ConversationJsonlScanner<'_, R>) -> Vec<ConversationScanEvent> {
        let mut events = Vec::new();
        while let Some(event) = scanner.next_event().expect("scan must not fail") {
            events.push(event);
        }
        events
    }

    fn file_with_lines(lines: &[&[u8]]) -> Vec<u8> {
        let mut bytes = HEADER.to_vec();
        for line in lines {
            bytes.extend_from_slice(line);
            bytes.push(b'\n');
        }
        bytes
    }

    fn padded_complete_line(line: &[u8], length: usize) -> Vec<u8> {
        assert!(
            line.len() <= length,
            "fixture must fit the requested physical cap"
        );
        let mut value = line.to_vec();
        value.extend(std::iter::repeat_n(b' ', length - value.len()));
        value
    }

    fn boundary_document() -> Value {
        serde_json::from_str(include_str!(
            "../../docs/fixtures/wire-v1/recipes/boundary-cases.json"
        ))
        .expect("boundary recipe document must be valid JSON")
    }

    fn boundary_case<'a>(document: &'a Value, name: &str) -> &'a Value {
        document["cases"]
            .as_array()
            .expect("boundary recipe document has cases")
            .iter()
            .find(|case| case["name"] == name)
            .unwrap_or_else(|| panic!("missing authoritative boundary recipe {name}"))
    }

    fn target_usize(case: &Value, field: &str) -> usize {
        usize::try_from(
            case[field]
                .as_u64()
                .unwrap_or_else(|| panic!("recipe {} has {field}", case["name"])),
        )
        .expect("recipe target fits usize")
    }

    fn entry_with_future_value(value: &[u8]) -> Vec<u8> {
        let entry = canonical_root_input_user_entry_line(1);
        assert!(
            entry.ends_with(b"}}"),
            "canonical entry has closing data/value braces"
        );
        let mut output = Vec::with_capacity(entry.len() + value.len() + 18);
        output.extend_from_slice(&entry[..entry.len() - 2]);
        output.extend_from_slice(b",\"futureField\":");
        output.extend_from_slice(value);
        output.extend_from_slice(b"}}");
        output
    }

    fn unknown_nested_arrays(target_depth: usize) -> Vec<u8> {
        let array_layers = target_depth
            .checked_sub(2)
            .expect("conversation root and entry data consume two depth levels");
        let mut value = Vec::with_capacity(array_layers * 2);
        value.extend(std::iter::repeat_n(b'[', array_layers));
        value.extend(std::iter::repeat_n(b']', array_layers));
        value
    }

    fn unknown_object_members(member_count: usize) -> Vec<u8> {
        let mut value = Vec::new();
        value.push(b'{');
        for index in 0..member_count {
            if index != 0 {
                value.push(b',');
            }
            value.extend_from_slice(format!("\"k{index}\":0").as_bytes());
        }
        value.push(b'}');
        value
    }

    fn unknown_array_items(item_count: usize) -> Vec<u8> {
        let mut value = Vec::with_capacity(item_count.saturating_mul(2).saturating_add(2));
        value.push(b'[');
        for index in 0..item_count {
            if index != 0 {
                value.push(b',');
            }
            value.push(b'0');
        }
        value.push(b']');
        value
    }

    fn json_node_count(value: &Value) -> usize {
        match value {
            Value::Array(values) => 1 + values.iter().map(json_node_count).sum::<usize>(),
            Value::Object(values) => 1 + values.values().map(json_node_count).sum::<usize>(),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => 1,
        }
    }

    fn unknown_tree(node_count: usize) -> Vec<u8> {
        assert_ne!(
            node_count, 0,
            "an additive JSON value has at least one node"
        );
        if node_count == 1 {
            return b"[]".to_vec();
        }

        let descendant_nodes = node_count - 1;
        let child_arrays = descendant_nodes.div_ceil(4_097);
        let scalar_nodes = descendant_nodes - child_arrays;
        assert!(
            child_arrays <= 4_096,
            "tree root remains within the array-item cap"
        );
        let mut remaining_scalars = scalar_nodes;
        let mut value = Vec::new();
        value.push(b'[');
        for child in 0..child_arrays {
            if child != 0 {
                value.push(b',');
            }
            let child_scalars = remaining_scalars.min(4_096);
            remaining_scalars -= child_scalars;
            value.push(b'[');
            for scalar in 0..child_scalars {
                if scalar != 0 {
                    value.push(b',');
                }
                value.extend_from_slice(b"null");
            }
            value.push(b']');
        }
        assert_eq!(remaining_scalars, 0, "tree distributes every declared node");
        value.push(b']');
        value
    }

    fn tool_call_assistant_line(arguments: &str) -> Vec<u8> {
        let assistant = TOOL_EXCHANGE
            .split(|byte| *byte == b'\n')
            .nth(2)
            .expect("golden tool exchange has a tool-call assistant entry");
        String::from_utf8(assistant.to_vec())
            .expect("golden entry is UTF-8")
            .replacen(
                "\"arguments\":{\"city\":\"Paris\",\"days\":1,\"units\":\"metric\"}",
                &format!("\"arguments\":{arguments}"),
                1,
            )
            .into_bytes()
    }

    fn fixture_session(bytes: &[u8]) -> SessionId {
        let line = bytes
            .split(|byte| *byte == b'\n')
            .next()
            .expect("fixture has a first line")
            .strip_suffix(b"\r")
            .unwrap_or_else(|| {
                bytes
                    .split(|byte| *byte == b'\n')
                    .next()
                    .expect("fixture has a first line")
            });
        let ConversationRecord::Header(header) = ConversationLineCodec::decode_record(line)
            .expect("successful corruption fixture has a decodable Header")
        else {
            panic!("successful corruption fixture must start with a Header");
        };
        header.session_id()
    }

    fn fixture_scanner<'lease>(
        bytes: &'static [u8],
        access: ConversationScanAccess<'lease>,
    ) -> ConversationJsonlScanner<'lease, Cursor<Vec<u8>>> {
        let session_id = fixture_session(bytes);
        let owned = bytes.to_vec();
        let length = u64::try_from(owned.len()).expect("fixture length fits u64");
        ConversationJsonlScanner::open(Cursor::new(owned), length, session_id, access)
            .expect("fixture strict Header must succeed")
    }

    #[test]
    fn scanner_owned_boundary_recipe_inventory_is_exact() {
        let document = boundary_document();
        assert_eq!(document["version"], 1);
        assert_eq!(document["lineEndingBytesExcludedFromLineCaps"], true);
        let owned_scopes = [
            "conversation_header_line",
            "conversation_entry_line",
            "conversation_file",
            "conversation_complete_entries",
            "conversation_record_structure",
        ];
        let actual = document["cases"]
            .as_array()
            .expect("boundary recipe document has cases")
            .iter()
            .filter(|case| {
                case["scope"]
                    .as_str()
                    .is_some_and(|scope| owned_scopes.contains(&scope))
            })
            .collect::<Vec<_>>();
        let names = actual
            .iter()
            .map(|case| case["name"].as_str().expect("case name"))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "header_line_boundary",
                "header_line_oversized",
                "entry_line_boundary",
                "entry_line_oversized",
                "file_boundary",
                "file_oversized",
                "complete_entry_count_boundary",
                "complete_entry_count_oversized",
                "invalid_utf8_entry",
                "record_depth_boundary",
                "record_depth_oversized",
                "record_object_members_boundary",
                "record_object_members_oversized",
                "record_array_items_boundary",
                "record_array_items_oversized",
                "record_total_nodes_boundary",
                "record_total_nodes_oversized",
                "record_string_boundary",
                "record_string_oversized",
            ]
        );

        for case in actual {
            let name = case["name"].as_str().expect("case name");
            let (scope, target_field, target, generator, expected) = match name {
                "header_line_boundary" => (
                    "conversation_header_line",
                    "targetBytesExcludingLineEnding",
                    65_536,
                    json!({"kind":"headerWithTrailingWhitespace","lineEnding":"lf"}),
                    json!({"load":"succeeds"}),
                ),
                "header_line_oversized" => (
                    "conversation_header_line",
                    "targetBytesExcludingLineEnding",
                    65_537,
                    json!({"kind":"headerWithTrailingWhitespace","lineEnding":"lf"}),
                    json!({"load":"fails","error":"HeaderCorrupt","truncate":false}),
                ),
                "entry_line_boundary" => (
                    "conversation_entry_line",
                    "targetBytesExcludingLineEnding",
                    1_048_576,
                    json!({"kind":"entryWithBoundedUnknownPadding","lineEnding":"lf"}),
                    json!({"lineAccepted":true}),
                ),
                "entry_line_oversized" => (
                    "conversation_entry_line",
                    "targetBytesExcludingLineEnding",
                    1_048_577,
                    json!({"kind":"entryWithBoundedUnknownPaddingThenCanonicalEntry","lineEnding":"lf"}),
                    json!({"lineAccepted":false,"diagnostic":"oversized_line","scanContinuesAfterLf":true,"followingCanonicalEntryAccepted":true}),
                ),
                "file_boundary" => (
                    "conversation_file",
                    "targetBytes",
                    1_073_741_824,
                    json!({"kind":"canonicalHeaderAndOneEntryThenAsciiPartialTail","exactPhysicalBytes":true}),
                    json!({"loadMayProceedToEntryCountRule":true,"tailTruncationEvaluatedAfterFileCap":true}),
                ),
                "file_oversized" => (
                    "conversation_file",
                    "targetBytes",
                    1_073_741_825,
                    json!({"kind":"canonicalHeaderAndOneEntryThenAsciiPartialTail","exactPhysicalBytes":true}),
                    json!({"load":"fails","error":"HistoryTooLarge","truncate":false}),
                ),
                "complete_entry_count_boundary" => (
                    "conversation_complete_entries",
                    "targetCount",
                    1_000_000,
                    json!({"kind":"streamCanonicalEntryChain","uniqueEntryIds":true}),
                    json!({"countAccepted":true}),
                ),
                "complete_entry_count_oversized" => (
                    "conversation_complete_entries",
                    "targetCount",
                    1_000_001,
                    json!({"kind":"streamCanonicalEntryChain","uniqueEntryIds":true}),
                    json!({"load":"fails","error":"HistoryTooLarge","failureAtEntry":1_000_001}),
                ),
                "invalid_utf8_entry" => (
                    "conversation_entry_line",
                    "",
                    0,
                    json!({"kind":"rawBytesThenCanonicalEntry","hex":"ff0a"}),
                    json!({"lineAccepted":false,"diagnostic":"invalid_utf8","scanContinuesAfterLf":true,"followingCanonicalEntryAccepted":true}),
                ),
                "record_depth_boundary" => (
                    "conversation_record_structure",
                    "targetDepth",
                    64,
                    json!({"kind":"entryUnknownAdditiveNestedArrays"}),
                    json!({"entryAccepted":true}),
                ),
                "record_depth_oversized" => (
                    "conversation_record_structure",
                    "targetDepth",
                    65,
                    json!({"kind":"entryUnknownAdditiveNestedArrays"}),
                    json!({"entryAccepted":false,"diagnostic":"invalid_entry"}),
                ),
                "record_object_members_boundary" => (
                    "conversation_record_structure",
                    "targetObjectMembers",
                    256,
                    json!({"kind":"entryUnknownAdditiveObject"}),
                    json!({"entryAccepted":true}),
                ),
                "record_object_members_oversized" => (
                    "conversation_record_structure",
                    "targetObjectMembers",
                    257,
                    json!({"kind":"entryUnknownAdditiveObject"}),
                    json!({"entryAccepted":false,"diagnostic":"invalid_entry"}),
                ),
                "record_array_items_boundary" => (
                    "conversation_record_structure",
                    "targetArrayItems",
                    4_096,
                    json!({"kind":"entryUnknownAdditiveArray"}),
                    json!({"entryAccepted":true}),
                ),
                "record_array_items_oversized" => (
                    "conversation_record_structure",
                    "targetArrayItems",
                    4_097,
                    json!({"kind":"entryUnknownAdditiveArray"}),
                    json!({"entryAccepted":false,"diagnostic":"invalid_entry"}),
                ),
                "record_total_nodes_boundary" => (
                    "conversation_record_structure",
                    "targetTotalNodes",
                    16_384,
                    json!({"kind":"entryUnknownAdditiveTree"}),
                    json!({"entryAccepted":true}),
                ),
                "record_total_nodes_oversized" => (
                    "conversation_record_structure",
                    "targetTotalNodes",
                    16_385,
                    json!({"kind":"entryUnknownAdditiveTree"}),
                    json!({"entryAccepted":false,"diagnostic":"invalid_entry"}),
                ),
                "record_string_boundary" => (
                    "conversation_record_structure",
                    "targetDecodedStringBytes",
                    524_288,
                    json!({"kind":"entryUnknownAdditiveString","fill":"x"}),
                    json!({"entryAccepted":true}),
                ),
                "record_string_oversized" => (
                    "conversation_record_structure",
                    "targetDecodedStringBytes",
                    524_289,
                    json!({"kind":"entryUnknownAdditiveString","fill":"x"}),
                    json!({"entryAccepted":false,"diagnostic":"invalid_entry"}),
                ),
                _ => panic!("unexpected scanner-owned boundary case {name}"),
            };
            assert_eq!(case["scope"], scope, "{name} scope");
            if target_field.is_empty() {
                assert!(case.get("targetBytes").is_none(), "{name} has no target");
                assert!(case.get("targetCount").is_none(), "{name} has no target");
            } else {
                assert_eq!(case[target_field], target, "{name} target");
            }
            assert_eq!(case["generator"], generator, "{name} generator");
            assert_eq!(case["expected"], expected, "{name} expected result");
        }
    }

    #[test]
    fn accepts_lf_and_crlf_without_normalizing_json_payloads() {
        let lf_events = drain(&mut scanner(
            file_with_lines(&[entry_line()]),
            ConversationScanAccess::ReadOnly,
        ));
        assert!(matches!(
            lf_events.as_slice(),
            [ConversationScanEvent::Entry { location, .. }]
                if location.line_number() == 2
                && location.offset() == u64::try_from(HEADER.len()).unwrap()
        ));

        let crlf =
            include_bytes!("../../docs/fixtures/wire-v1/conversation/corruption/crlf-input.jsonl");
        let crlf_events = drain(&mut fixture_scanner(crlf, ConversationScanAccess::ReadOnly));
        assert!(matches!(
            crlf_events.as_slice(),
            [ConversationScanEvent::Entry { location, .. }]
                if location.line_number() == 2
                && location.offset() == 264
        ));
    }

    #[test]
    fn canonical_line_facts_distinguish_lf_crlf_and_reencode_mismatches() {
        let mut canonical = scanner(
            file_with_lines(&[entry_line()]),
            ConversationScanAccess::ReadOnly,
        );
        assert!(canonical.header_is_canonical());
        assert!(matches!(
            canonical.next_event(),
            Ok(Some(ConversationScanEvent::Entry {
                canonicality: ConversationLineCanonicality::Canonical,
                ..
            }))
        ));

        // Header and Entry canonicality are independent per physical line.
        let mut mixed_line_endings = HEADER.to_vec();
        mixed_line_endings.extend_from_slice(entry_line());
        mixed_line_endings.extend_from_slice(b"\r\n");
        let mut mixed_line_endings = scanner(mixed_line_endings, ConversationScanAccess::ReadOnly);
        assert!(mixed_line_endings.header_is_canonical());
        assert!(matches!(
            mixed_line_endings.next_event(),
            Ok(Some(ConversationScanEvent::Entry {
                canonicality: ConversationLineCanonicality::NonCanonical,
                ..
            }))
        ));

        let mut crlf = HEADER
            .strip_suffix(b"\n")
            .expect("authoritative Header has LF")
            .to_vec();
        crlf.extend_from_slice(b"\r\n");
        crlf.extend_from_slice(entry_line());
        crlf.extend_from_slice(b"\r\n");
        let mut crlf = scanner(crlf, ConversationScanAccess::ReadOnly);
        assert!(!crlf.header_is_canonical());
        assert!(matches!(
            crlf.next_event(),
            Ok(Some(ConversationScanEvent::Entry {
                canonicality: ConversationLineCanonicality::NonCanonical,
                ..
            }))
        ));

        let mut additive = scanner(
            file_with_lines(&[&entry_with_future_value(b"true")]),
            ConversationScanAccess::ReadOnly,
        );
        assert!(additive.header_is_canonical());
        assert!(matches!(
            additive.next_event(),
            Ok(Some(ConversationScanEvent::Entry {
                canonicality: ConversationLineCanonicality::NonCanonical,
                ..
            }))
        ));
    }

    #[test]
    fn recovers_after_invalid_utf8_malformed_and_unknown_complete_lines() {
        let document = boundary_document();
        let utf8_recipe = boundary_case(&document, "invalid_utf8_entry");
        assert_eq!(
            utf8_recipe["generator"],
            json!({"kind":"rawBytesThenCanonicalEntry","hex":"ff0a"})
        );
        assert_eq!(utf8_recipe["expected"]["diagnostic"], "invalid_utf8");
        assert_eq!(
            utf8_recipe["expected"]["followingCanonicalEntryAccepted"],
            true
        );
        let mut invalid_utf8 = file_with_lines(&[]);
        invalid_utf8.extend_from_slice(b"\xff\n");
        invalid_utf8.extend_from_slice(&canonical_root_input_user_entry_line(1));
        invalid_utf8.push(b'\n');
        let invalid_utf8_events =
            drain(&mut scanner(invalid_utf8, ConversationScanAccess::ReadOnly));
        assert!(matches!(
            invalid_utf8_events.as_slice(),
            [
                ConversationScanEvent::Fault {
                    fault: ConversationLineFault::InvalidUtf8,
                    ..
                },
                ConversationScanEvent::Entry { .. }
            ]
        ));

        let duplicate_key = b"{\"type\":\"entry\",\"type\":\"entry\"}";
        let duplicate_key_events = drain(&mut scanner(
            file_with_lines(&[duplicate_key, entry_line()]),
            ConversationScanAccess::ReadOnly,
        ));
        assert!(matches!(
            duplicate_key_events.as_slice(),
            [
                ConversationScanEvent::Fault {
                    fault: ConversationLineFault::InvalidEntry,
                    ..
                },
                ConversationScanEvent::Entry { .. }
            ]
        ));

        let malformed = include_bytes!(
            "../../docs/fixtures/wire-v1/conversation/corruption/malformed-middle.jsonl"
        );
        let malformed_events = drain(&mut fixture_scanner(
            malformed,
            ConversationScanAccess::ReadOnly,
        ));
        assert!(matches!(
            malformed_events.as_slice(),
            [
                ConversationScanEvent::Entry { .. },
                ConversationScanEvent::Fault {
                    fault: ConversationLineFault::MalformedJson,
                    ..
                },
                ConversationScanEvent::Entry { .. }
            ]
        ));

        let unknown_record = include_bytes!(
            "../../docs/fixtures/wire-v1/conversation/corruption/unknown-record-variant.jsonl"
        );
        let unknown_record_events = drain(&mut fixture_scanner(
            unknown_record,
            ConversationScanAccess::ReadOnly,
        ));
        assert!(matches!(
            unknown_record_events.as_slice(),
            [
                ConversationScanEvent::Fault {
                    fault: ConversationLineFault::UnknownRecordVariant,
                    ..
                },
                ConversationScanEvent::Entry { .. }
            ]
        ));

        let unknown_body = include_bytes!(
            "../../docs/fixtures/wire-v1/conversation/corruption/unknown-body-variant.jsonl"
        );
        let unknown_body_events = drain(&mut fixture_scanner(
            unknown_body,
            ConversationScanAccess::ReadOnly,
        ));
        assert!(matches!(
            unknown_body_events.as_slice(),
            [
                ConversationScanEvent::Fault {
                    fault: ConversationLineFault::UnknownEntryVariant,
                    ..
                },
                ConversationScanEvent::Entry { .. }
            ]
        ));
    }

    #[test]
    fn unknown_nested_tags_and_pure_unit_leaves_are_unknown_entries() {
        let pure_unit_leaf = String::from_utf8(canonical_root_input_user_entry_line(1))
            .expect("generated canonical entry is UTF-8")
            .replacen(r#""source":"input""#, r#""source":"future_source""#, 1)
            .into_bytes();
        let nested_tag = String::from_utf8(canonical_root_input_user_entry_line(1))
            .expect("generated canonical entry is UTF-8")
            .replacen(r#""type":"text""#, r#""type":"future_part""#, 1)
            .into_bytes();
        let wrong_leaf_type = String::from_utf8(canonical_root_input_user_entry_line(1))
            .expect("generated canonical entry is UTF-8")
            .replacen(r#""source":"input""#, r#""source":null"#, 1)
            .into_bytes();
        let noncanonical_scalar = String::from_utf8(canonical_root_input_user_entry_line(1))
            .expect("generated canonical entry is UTF-8")
            .replacen(
                "ent_00000000000000000000000000000001",
                "ent_00000000000000000000000000000000",
                1,
            )
            .into_bytes();
        let events = drain(&mut scanner(
            file_with_lines(&[
                &pure_unit_leaf,
                &nested_tag,
                &wrong_leaf_type,
                &noncanonical_scalar,
                &canonical_root_input_user_entry_line(1),
            ]),
            ConversationScanAccess::ReadOnly,
        ));
        assert!(matches!(
            events.as_slice(),
            [
                ConversationScanEvent::Fault {
                    fault: ConversationLineFault::UnknownEntryVariant,
                    ..
                },
                ConversationScanEvent::Fault {
                    fault: ConversationLineFault::UnknownEntryVariant,
                    ..
                },
                ConversationScanEvent::Fault {
                    fault: ConversationLineFault::InvalidEntry,
                    ..
                },
                ConversationScanEvent::Fault {
                    fault: ConversationLineFault::InvalidEntry,
                    ..
                },
                ConversationScanEvent::Entry { .. }
            ]
        ));
    }

    #[test]
    fn malformed_or_unknown_contribution_stamps_do_not_fault_the_entry() {
        let stamp_salvage = include_bytes!(
            "../../docs/fixtures/wire-v1/conversation/corruption/contribution-stamp-salvage.jsonl"
        );
        let events = drain(&mut fixture_scanner(
            stamp_salvage,
            ConversationScanAccess::ReadOnly,
        ));
        assert!(matches!(
            events.as_slice(),
            [ConversationScanEvent::Entry { .. }]
        ));
    }

    #[test]
    fn authoritative_header_boundaries_use_the_production_scanner_seam() {
        let document = boundary_document();
        let header_boundary = boundary_case(&document, "header_line_boundary");
        let header_oversized = boundary_case(&document, "header_line_oversized");
        let header_bytes = target_usize(header_boundary, "targetBytesExcludingLineEnding");
        let header_plus_one = target_usize(header_oversized, "targetBytesExcludingLineEnding");
        assert_eq!(
            header_boundary["generator"]["kind"],
            "headerWithTrailingWhitespace"
        );
        assert_eq!(header_boundary["generator"]["lineEnding"], "lf");
        assert_eq!(header_oversized["expected"]["error"], "HeaderCorrupt");
        assert_eq!(header_plus_one, header_bytes + 1);

        let header = HEADER.strip_suffix(b"\n").unwrap();
        let exact_header = padded_complete_line(header, header_bytes);
        let mut exact_header_file = exact_header;
        exact_header_file.push(b'\n');
        let exact_header_length = u64::try_from(exact_header_file.len()).unwrap();
        let mut exact_header_scanner = ConversationJsonlScanner::open(
            Cursor::new(exact_header_file),
            exact_header_length,
            session_id(),
            ConversationScanAccess::ReadOnly,
        )
        .expect("exact Header cap must be accepted");
        assert!(exact_header_scanner.next_event().unwrap().is_none());

        let oversized_header = padded_complete_line(header, header_plus_one);
        let mut oversized_header_file = oversized_header;
        oversized_header_file.push(b'\n');
        let oversized_header_length = u64::try_from(oversized_header_file.len()).unwrap();
        assert_open_error(
            ConversationJsonlScanner::open(
                Cursor::new(oversized_header_file),
                oversized_header_length,
                session_id(),
                ConversationScanAccess::ReadOnly,
            ),
            ConversationScanError::HeaderCorrupt {
                code: ConversationCodecError::HeaderTooLarge,
            },
        );
    }

    #[test]
    fn authoritative_record_structural_boundaries_accept_exactly_and_reject_plus_one() {
        let document = boundary_document();
        let baseline_nodes = json_node_count(
            &serde_json::from_slice::<Value>(entry_line())
                .expect("canonical entry is JSON for local node accounting"),
        );
        let cases: [(&str, &str, StructuralValueGenerator); 5] = [
            (
                "record_depth",
                "targetDepth",
                unknown_nested_arrays as StructuralValueGenerator,
            ),
            (
                "record_object_members",
                "targetObjectMembers",
                unknown_object_members as fn(usize) -> Vec<u8>,
            ),
            (
                "record_array_items",
                "targetArrayItems",
                unknown_array_items as fn(usize) -> Vec<u8>,
            ),
            ("record_total_nodes", "targetTotalNodes", unknown_tree),
            ("record_string", "targetDecodedStringBytes", |bytes| {
                let mut value = Vec::with_capacity(bytes + 2);
                value.push(b'\"');
                value.extend(std::iter::repeat_n(b'x', bytes));
                value.push(b'\"');
                value
            }),
        ];

        for (prefix, target_field, generator) in cases {
            let boundary = boundary_case(&document, &format!("{prefix}_boundary"));
            let oversized = boundary_case(&document, &format!("{prefix}_oversized"));
            let target = target_usize(boundary, target_field);
            let plus_one = target_usize(oversized, target_field);
            assert_eq!(boundary["scope"], "conversation_record_structure");
            assert_eq!(oversized["scope"], "conversation_record_structure");
            assert_eq!(oversized["generator"], boundary["generator"]);
            assert_eq!(boundary["expected"]["entryAccepted"], true);
            assert_eq!(oversized["expected"]["diagnostic"], "invalid_entry");
            assert_eq!(plus_one, target + 1);

            let boundary_value = if prefix == "record_total_nodes" {
                generator(
                    target
                        .checked_sub(baseline_nodes)
                        .expect("node target exceeds canonical entry nodes"),
                )
            } else {
                generator(target)
            };
            let plus_one_value = if prefix == "record_total_nodes" {
                generator(
                    plus_one
                        .checked_sub(baseline_nodes)
                        .expect("node target exceeds canonical entry nodes"),
                )
            } else {
                generator(plus_one)
            };
            let boundary_line = entry_with_future_value(&boundary_value);
            let plus_one_line = entry_with_future_value(&plus_one_value);
            if prefix == "record_total_nodes" {
                let boundary_nodes = json_node_count(
                    &serde_json::from_slice::<Value>(&boundary_line)
                        .expect("generated boundary entry is JSON"),
                );
                let plus_one_nodes = json_node_count(
                    &serde_json::from_slice::<Value>(&plus_one_line)
                        .expect("generated plus-one entry is JSON"),
                );
                assert_eq!(boundary_nodes, target, "{prefix} boundary nodes");
                assert_eq!(plus_one_nodes, plus_one, "{prefix} plus-one nodes");
            }

            let boundary_events = drain(&mut scanner(
                file_with_lines(&[&boundary_line]),
                ConversationScanAccess::ReadOnly,
            ));
            assert!(
                matches!(
                    boundary_events.as_slice(),
                    [ConversationScanEvent::Entry { .. }]
                ),
                "{prefix} boundary must be accepted"
            );

            let plus_one_events = drain(&mut scanner(
                file_with_lines(&[&plus_one_line]),
                ConversationScanAccess::ReadOnly,
            ));
            assert!(
                matches!(
                    plus_one_events.as_slice(),
                    [ConversationScanEvent::Fault {
                        fault: ConversationLineFault::InvalidEntry,
                        ..
                    }]
                ),
                "{prefix} plus-one must be invalid_entry"
            );
        }
    }

    #[test]
    fn embedded_json_limit_failures_are_invalid_entries_not_malformed_json() {
        let raw_input_too_large = format!("{{{}}}", " ".repeat(65_535));
        let mut canonical_output_too_large = String::from("{\"v\":[");
        for outer in 0..37 {
            if outer != 0 {
                canonical_output_too_large.push(',');
            }
            canonical_output_too_large.push('[');
            for inner in 0..256 {
                if inner != 0 {
                    canonical_output_too_large.push(',');
                }
                canonical_output_too_large.push_str("1e10");
            }
            canonical_output_too_large.push(']');
        }
        canonical_output_too_large.push_str("]}");
        assert!(canonical_output_too_large.len() <= 65_536);

        for arguments in [raw_input_too_large, canonical_output_too_large] {
            let events = drain(&mut scanner(
                file_with_lines(&[&tool_call_assistant_line(&arguments), entry_line()]),
                ConversationScanAccess::ReadOnly,
            ));
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        ConversationScanEvent::Fault {
                            fault: ConversationLineFault::InvalidEntry,
                            ..
                        },
                        ConversationScanEvent::Entry { .. }
                    ]
                ),
                "embedded JSON limit events were {events:?}"
            );
        }
    }

    #[test]
    fn final_partial_tail_returns_the_exact_read_only_or_exclusive_action() {
        let partial_tail = include_bytes!(
            "../../docs/fixtures/wire-v1/conversation/corruption/partial-tail.jsonl"
        );
        let read_only_events = drain(&mut fixture_scanner(
            partial_tail,
            ConversationScanAccess::ReadOnly,
        ));
        assert!(matches!(
            read_only_events.as_slice(),
            [
                ConversationScanEvent::Entry { .. },
                ConversationScanEvent::PartialTail {
                    location,
                    action: ConversationPartialTailAction::Ignore,
                }
            ] if location.line_number() == 3 && location.offset() == 693
        ));

        let declared_file_bytes = u64::try_from(partial_tail.len()).unwrap();
        let lease = ExclusiveWritableConversationLease::for_scanner_test(
            fixture_session(partial_tail),
            declared_file_bytes,
        );
        let writable_events = drain(&mut fixture_scanner(
            partial_tail,
            ConversationScanAccess::ExclusiveWritable(&lease),
        ));
        assert!(matches!(
            writable_events.as_slice(),
            [
                ConversationScanEvent::Entry { .. },
                ConversationScanEvent::PartialTail {
                    action: ConversationPartialTailAction::TruncateTo { offset: 693 },
                    ..
                }
            ]
        ));

        let mut complete_but_unterminated = HEADER.to_vec();
        complete_but_unterminated.extend_from_slice(entry_line());
        let events = drain(&mut scanner(
            complete_but_unterminated,
            ConversationScanAccess::ReadOnly,
        ));
        assert!(matches!(
            events.as_slice(),
            [ConversationScanEvent::PartialTail {
                location,
                action: ConversationPartialTailAction::Ignore,
            }] if location.line_number() == 2
                && location.offset() == u64::try_from(HEADER.len()).unwrap()
        ));
    }

    #[test]
    fn writable_lease_must_bind_the_opened_session_and_declared_file_length() {
        let other_session_id: SessionId = "ses_99999999999999999999999999999999"
            .parse()
            .expect("alternate fixture session ID is valid");
        let session_mismatch =
            ExclusiveWritableConversationLease::for_scanner_test(other_session_id, 0);
        assert_open_error(
            ConversationJsonlScanner::open(
                PanicOnRead,
                0,
                session_id(),
                ConversationScanAccess::ExclusiveWritable(&session_mismatch),
            ),
            ConversationScanError::LeaseMismatch,
        );

        let length_mismatch = ExclusiveWritableConversationLease::for_scanner_test(session_id(), 1);
        assert_open_error(
            ConversationJsonlScanner::open(
                PanicOnRead,
                0,
                session_id(),
                ConversationScanAccess::ExclusiveWritable(&length_mismatch),
            ),
            ConversationScanError::LeaseMismatch,
        );
    }

    #[test]
    fn changed_input_is_rejected_before_a_writable_scan_can_return_a_tail_action() {
        let bytes = HEADER.to_vec();
        let shorter_declared_length = u64::try_from(bytes.len() - 1).unwrap();
        let shorter_lease = ExclusiveWritableConversationLease::for_scanner_test(
            session_id(),
            shorter_declared_length,
        );
        let mut shorter_scanner = ConversationJsonlScanner::open(
            Cursor::new(bytes),
            shorter_declared_length,
            session_id(),
            ConversationScanAccess::ExclusiveWritable(&shorter_lease),
        )
        .expect("Header can be decoded before the stale metadata is reconciled at EOF");
        assert_eq!(
            shorter_scanner.next_event(),
            Err(ConversationScanError::InputChanged)
        );

        let bytes = HEADER.to_vec();
        let longer_declared_length = u64::try_from(bytes.len() + 1).unwrap();
        let longer_lease = ExclusiveWritableConversationLease::for_scanner_test(
            session_id(),
            longer_declared_length,
        );
        let mut scanner = ConversationJsonlScanner::open(
            Cursor::new(bytes),
            longer_declared_length,
            session_id(),
            ConversationScanAccess::ExclusiveWritable(&longer_lease),
        )
        .expect("Header can be decoded before the stale metadata is reconciled at EOF");
        assert_eq!(
            scanner.next_event(),
            Err(ConversationScanError::InputChanged)
        );
    }

    struct ChunkBoundedReader {
        input: Cursor<Vec<u8>>,
    }

    impl ChunkBoundedReader {
        fn new(input: Vec<u8>) -> Self {
            Self {
                input: Cursor::new(input),
            }
        }
    }

    impl Read for ChunkBoundedReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                buffer.len() <= SCANNER_CHUNK_BYTES,
                "metadata-unavailable scans must keep read requests chunk-bounded"
            );
            self.input.read(buffer)
        }
    }

    #[test]
    fn metadata_unavailable_read_only_scans_exact_cap_and_rejects_cap_plus_one() {
        let exact_bytes = file_with_lines(&[entry_line()]);
        let exact_file_bytes = u64::try_from(exact_bytes.len()).unwrap();
        let limits = ConversationScanLimits {
            file_bytes: exact_file_bytes,
            ..ConversationScanLimits::V1
        };
        let mut exact_scanner = ConversationJsonlScanner::open_with_limits(
            ChunkBoundedReader::new(exact_bytes),
            None,
            session_id(),
            ConversationScanAccess::ReadOnly,
            limits,
        )
        .expect("metadata-unavailable input exactly at the file cap must scan");
        assert!(matches!(
            drain(&mut exact_scanner).as_slice(),
            [ConversationScanEvent::Entry { .. }]
        ));

        let mut over_cap_bytes = file_with_lines(&[entry_line()]);
        over_cap_bytes.push(b'x');
        assert_open_error(
            ConversationJsonlScanner::open_with_limits(
                ChunkBoundedReader::new(over_cap_bytes),
                None,
                session_id(),
                ConversationScanAccess::ReadOnly,
                limits,
            ),
            ConversationScanError::FileTooLarge,
        );
    }

    #[test]
    fn metadata_unavailable_read_only_scan_ignores_a_partial_tail_without_truncation() {
        let mut bytes = file_with_lines(&[entry_line()]);
        bytes.extend_from_slice(b"partial");
        let limits = ConversationScanLimits {
            file_bytes: u64::try_from(bytes.len()).unwrap(),
            ..ConversationScanLimits::V1
        };
        let mut scanner = ConversationJsonlScanner::open_with_limits(
            ChunkBoundedReader::new(bytes),
            None,
            session_id(),
            ConversationScanAccess::ReadOnly,
            limits,
        )
        .expect("metadata-unavailable read-only input must decode its Header");
        assert!(matches!(
            drain(&mut scanner).as_slice(),
            [
                ConversationScanEvent::Entry { .. },
                ConversationScanEvent::PartialTail {
                    action: ConversationPartialTailAction::Ignore,
                    ..
                }
            ]
        ));
    }

    #[test]
    fn file_cap_has_priority_over_a_mismatched_writable_lease() {
        let file_bytes = u64::try_from(HEADER.len()).unwrap();
        let limits = ConversationScanLimits {
            file_bytes,
            ..ConversationScanLimits::V1
        };
        let wrong_session: SessionId = "ses_99999999999999999999999999999999"
            .parse()
            .expect("alternate fixture session ID is valid");
        let lease =
            ExclusiveWritableConversationLease::for_scanner_test(wrong_session, file_bytes + 1);
        assert_open_error(
            ConversationJsonlScanner::open_with_limits(
                PanicOnRead,
                Some(file_bytes + 1),
                session_id(),
                ConversationScanAccess::ExclusiveWritable(&lease),
                limits,
            ),
            ConversationScanError::FileTooLarge,
        );
    }

    #[test]
    fn actual_file_cap_has_priority_over_a_shorter_declared_observation() {
        let mut bytes = HEADER.to_vec();
        bytes.push(b'x');
        let file_bytes = u64::try_from(HEADER.len()).unwrap();
        let limits = ConversationScanLimits {
            file_bytes,
            ..ConversationScanLimits::V1
        };
        assert_open_error(
            ConversationJsonlScanner::open_with_limits(
                ChunkBoundedReader::new(bytes),
                Some(file_bytes - 1),
                session_id(),
                ConversationScanAccess::ReadOnly,
                limits,
            ),
            ConversationScanError::FileTooLarge,
        );
    }

    #[test]
    fn complete_bad_last_line_is_preserved_and_never_produces_a_tail_action() {
        let events = drain(&mut scanner(
            file_with_lines(&[b"not json"]),
            ConversationScanAccess::ReadOnly,
        ));
        assert!(matches!(
            events.as_slice(),
            [ConversationScanEvent::Fault {
                fault: ConversationLineFault::MalformedJson,
                ..
            }]
        ));
    }

    #[test]
    fn strict_header_failures_stop_before_any_entry_events() {
        for (bytes, expected) in [
            (
                b"\xff\n".as_slice(),
                ConversationScanError::HeaderCorrupt {
                    code: ConversationCodecError::JsonSyntax,
                },
            ),
            (
                b"{\"type\":\"future_record\",\"data\":{}}\n".as_slice(),
                ConversationScanError::HeaderCorrupt {
                    code: ConversationCodecError::UnknownRecordVariant,
                },
            ),
        ] {
            let length = u64::try_from(bytes.len()).unwrap();
            assert_open_error(
                ConversationJsonlScanner::open(
                    Cursor::new(bytes.to_vec()),
                    length,
                    session_id(),
                    ConversationScanAccess::ReadOnly,
                ),
                expected,
            );
        }

        for (bytes, expected) in [
            (
                include_bytes!(
                    "../../docs/fixtures/wire-v1/conversation/corruption/duplicate-header-key.jsonl"
                )
                .as_slice(),
                ConversationScanError::HeaderCorrupt {
                    code: ConversationCodecError::JsonStructure,
                },
            ),
            (
                include_bytes!(
                    "../../docs/fixtures/wire-v1/conversation/corruption/unsupported-version-header.jsonl"
                )
                .as_slice(),
                ConversationScanError::UnsupportedFormatVersion,
            ),
            (
                include_bytes!(
                    "../../docs/fixtures/wire-v1/conversation/corruption/wrong-session-header.jsonl"
                )
                .as_slice(),
                ConversationScanError::HeaderCorrupt {
                    code: ConversationCodecError::SessionIdentityMismatch,
                },
            ),
        ] {
            let length = u64::try_from(bytes.len()).unwrap();
            assert_open_error(
                ConversationJsonlScanner::open(
                    Cursor::new(bytes.to_vec()),
                    length,
                    session_id(),
                    ConversationScanAccess::ReadOnly,
                ),
                expected,
            );
        }
    }

    struct PanicOnRead;

    impl Read for PanicOnRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("whole-file cap must be checked before reading")
        }
    }

    #[test]
    fn declared_file_cap_has_priority_and_never_attempts_tail_handling() {
        assert_open_error(
            ConversationJsonlScanner::open(
                PanicOnRead,
                MAX_CONVERSATION_FILE_BYTES + 1,
                session_id(),
                ConversationScanAccess::ReadOnly,
            ),
            ConversationScanError::FileTooLarge,
        );
    }

    struct StreamingOversizedLine {
        phase: u8,
        position: usize,
        spaces_remaining: usize,
    }

    impl StreamingOversizedLine {
        fn new(entry_bytes: usize) -> Self {
            Self {
                phase: 0,
                position: 0,
                spaces_remaining: entry_bytes * 2,
            }
        }
    }

    impl Read for StreamingOversizedLine {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                buffer.len() <= SCANNER_CHUNK_BYTES,
                "scanner read request must remain chunk-bounded"
            );
            loop {
                match self.phase {
                    0 => {
                        let bytes = &HEADER[self.position..];
                        let count = bytes.len().min(buffer.len());
                        buffer[..count].copy_from_slice(&bytes[..count]);
                        self.position += count;
                        if self.position == HEADER.len() {
                            self.phase = 1;
                            self.position = 0;
                        }
                        return Ok(count);
                    }
                    1 if self.spaces_remaining != 0 => {
                        let count = self.spaces_remaining.min(buffer.len());
                        buffer[..count].fill(b' ');
                        self.spaces_remaining -= count;
                        return Ok(count);
                    }
                    1 => self.phase = 2,
                    2 => {
                        buffer[0] = b'\n';
                        self.phase = 3;
                        return Ok(1);
                    }
                    3 => {
                        let bytes = &entry_line()[self.position..];
                        let count = bytes.len().min(buffer.len());
                        buffer[..count].copy_from_slice(&bytes[..count]);
                        self.position += count;
                        if self.position == entry_line().len() {
                            self.phase = 4;
                            self.position = 0;
                        }
                        return Ok(count);
                    }
                    4 => {
                        buffer[0] = b'\n';
                        self.phase = 5;
                        return Ok(1);
                    }
                    5 => return Ok(0),
                    _ => unreachable!("stream phase is closed"),
                }
            }
        }
    }

    #[test]
    fn stream_discards_a_far_oversized_complete_line_with_only_a_bounded_line_buffer() {
        let entry_bytes = entry_line().len();
        let declared_file_bytes = u64::try_from(HEADER.len())
            .unwrap()
            .checked_add(u64::try_from(entry_bytes * 2).unwrap())
            .and_then(|value| value.checked_add(1))
            .and_then(|value| value.checked_add(u64::try_from(entry_line().len()).unwrap()))
            .and_then(|value| value.checked_add(1))
            .unwrap();
        let mut scanner = ConversationJsonlScanner::open_with_limits(
            StreamingOversizedLine::new(entry_bytes),
            Some(declared_file_bytes),
            session_id(),
            ConversationScanAccess::ReadOnly,
            ConversationScanLimits {
                entry_bytes,
                ..ConversationScanLimits::V1
            },
        )
        .unwrap();
        let events = drain(&mut scanner);
        assert!(matches!(
            events.as_slice(),
            [
                ConversationScanEvent::Fault {
                    fault: ConversationLineFault::OversizedLine,
                    ..
                },
                ConversationScanEvent::Entry { .. }
            ]
        ));
    }

    #[test]
    fn complete_entry_count_uses_a_test_only_smaller_limit_and_precedes_line_decode() {
        let mut bytes = file_with_lines(&[entry_line()]);
        bytes.extend_from_slice(b"\xff\n");
        let length = u64::try_from(bytes.len()).unwrap();
        let mut scanner = ConversationJsonlScanner::open_with_limits(
            Cursor::new(bytes),
            Some(length),
            session_id(),
            ConversationScanAccess::ReadOnly,
            ConversationScanLimits {
                entry_records: 1,
                ..ConversationScanLimits::V1
            },
        )
        .unwrap();
        assert!(matches!(
            scanner.next_event(),
            Ok(Some(ConversationScanEvent::Entry { .. }))
        ));
        assert_eq!(scanner.complete_entry_records(), 1);
        assert_eq!(
            scanner.next_event(),
            Err(ConversationScanError::HistoryTooLarge)
        );
    }

    #[test]
    fn scanner_debug_and_errors_do_not_echo_line_contents_or_os_sources() {
        let secret_line = b"secret conversation text /private/path";
        let mut scanner = scanner(
            file_with_lines(&[secret_line]),
            ConversationScanAccess::ReadOnly,
        );
        let scanner_debug = format!("{scanner:?}");
        assert!(!scanner_debug.contains("secret conversation text"));
        assert!(!scanner_debug.contains("/private/path"));

        let events = drain(&mut scanner);
        let debug = format!("{events:?}");
        assert!(!debug.contains("secret conversation text"));
        assert!(!debug.contains("/private/path"));

        let error = ConversationScanError::InputUnavailable;
        assert!(!format!("{error:?}").contains("os error"));
        assert!(!error.to_string().contains("os error"));
    }
}
