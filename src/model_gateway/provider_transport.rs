//! M14 shared production transport/framing for the direct provider adapters.
//!
//! Only protocol-neutral pieces with a real second consumer live here: the
//! locked-down reqwest client construction (no redirects, no retries, no
//! ambient proxy), the response byte limit, protocol-neutral cancellation /
//! invalid-request / invalid-provider-response / transport read / send-phase
//! classification, the cancellation-aware bounded body drain and bounded JSON
//! envelope read, numeric retry-after parsing, the event-stream content-type
//! check, and the incremental bounded SSE framing. Everything provider-specific
//! — request encoding, event dispatch, terminal parsers, typed error envelope
//! mapping, metadata/usage normalization — stays in the owning adapter module
//! (`openai_responses`, `anthropic_messages`). There is deliberately no generic
//! provider response/event parser here.
//!
//! The deterministic `127.0.0.1:0` loopback server harness is shared
//! `#[cfg(test)]` infrastructure for both adapters' contract suites.

use std::time::Duration;

use futures_util::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::model_gateway::{
    ModelCallErrorReason, ProviderAttemptError, ProviderRequestDeliveryState,
};

/// Builds the locked-down production HTTP client shared by every direct adapter:
/// redirects disabled, automatic retries disabled, ambient proxy disabled. An
/// adapter attempt therefore never sends more than one POST, and any POST it
/// sends carries the complete full request (pre-send cancellation,
/// `AuthMissing`, validation and encoding/build failures can produce zero
/// POSTs).
pub(super) fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .build()
}

pub(super) fn response_byte_limit() -> usize {
    usize::try_from(
        crate::wire::ProtocolLimits::v1_0()
            .transport
            .max_response_bytes,
    )
    .unwrap_or(usize::MAX)
}

pub(super) fn cancelled(delivery: ProviderRequestDeliveryState) -> ProviderAttemptError {
    ProviderAttemptError {
        reason: ModelCallErrorReason::Cancelled,
        retry_after: None,
        delivery,
    }
}

pub(super) fn invalid_provider_response(
    delivery: ProviderRequestDeliveryState,
) -> ProviderAttemptError {
    ProviderAttemptError {
        reason: ModelCallErrorReason::InvalidProviderResponse,
        retry_after: None,
        delivery,
    }
}

pub(super) fn invalid_request_not_sent() -> ProviderAttemptError {
    ProviderAttemptError {
        reason: ModelCallErrorReason::InvalidRequest,
        retry_after: None,
        delivery: ProviderRequestDeliveryState::NotSent,
    }
}

pub(super) fn transport_read_error(delivery: ProviderRequestDeliveryState) -> ProviderAttemptError {
    ProviderAttemptError {
        reason: ModelCallErrorReason::TransportUnavailable,
        retry_after: None,
        delivery,
    }
}

/// Send-phase transport failure: a connect failure proves nothing was sent; a send
/// timeout is typed `Timeout` (NotSent when it also proves no connect, otherwise
/// Unknown); any other send-phase error may have reached the provider, so delivery
/// is conservatively Unknown.
pub(super) fn classify_send_error(error: &reqwest::Error) -> ProviderAttemptError {
    if error.is_timeout() {
        return ProviderAttemptError {
            reason: ModelCallErrorReason::Timeout,
            retry_after: None,
            delivery: if error.is_connect() {
                ProviderRequestDeliveryState::NotSent
            } else {
                ProviderRequestDeliveryState::Unknown
            },
        };
    }
    ProviderAttemptError {
        reason: ModelCallErrorReason::TransportUnavailable,
        retry_after: None,
        delivery: if error.is_connect() {
            ProviderRequestDeliveryState::NotSent
        } else {
            ProviderRequestDeliveryState::Unknown
        },
    }
}

/// Drains a chunk stream up to a byte bound while remaining cancellation-aware.
///
/// This is the production owner of bounded body drains. Cancellation wins mid-drain
/// and returns a typed `Cancelled` carrying the caller-supplied conservative
/// `delivery` — provider-declared pre-execution rejection statuses may claim
/// `RejectedBeforeExecution`, every other status is `Unknown` — without reading any
/// further. A stream item error or an over-bound body returns `Ok(None)` so the
/// caller can fall back to status-only classification. Only a complete, in-bound
/// body returns `Ok(Some(bytes))`.
pub(super) async fn drain_bounded<C, E, S>(
    stream: S,
    cancel: &CancellationToken,
    maximum: usize,
    delivery: ProviderRequestDeliveryState,
) -> Result<Option<Vec<u8>>, ProviderAttemptError>
where
    S: Stream<Item = Result<C, E>>,
    C: AsRef<[u8]>,
{
    let mut bytes = Vec::new();
    let mut stream = std::pin::pin!(stream);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(cancelled(delivery)),
            chunk = stream.next() => {
                let Some(chunk) = chunk else { return Ok(Some(bytes)) };
                // A transport failure while reading the body falls back to
                // status-only classification.
                let Ok(chunk) = chunk else { return Ok(None) };
                let chunk = chunk.as_ref();
                if chunk.len() > maximum.saturating_sub(bytes.len()) {
                    return Ok(None);
                }
                bytes.extend_from_slice(chunk);
            }
        }
    }
}

/// Reads at most `max_response_bytes` of an error body and parses it as JSON while
/// remaining cancellation-aware (see [`drain_bounded`]): cancellation wins mid-read
/// and returns `Cancelled` with the conservative caller-supplied delivery without
/// reading any further. Unparseable or oversized bodies yield `None`; classification
/// then falls back to status alone, still structurally.
pub(super) async fn read_bounded_envelope(
    response: reqwest::Response,
    cancel: &CancellationToken,
    delivery: ProviderRequestDeliveryState,
) -> Result<Option<Value>, ProviderAttemptError> {
    let body = drain_bounded(
        response.bytes_stream(),
        cancel,
        response_byte_limit(),
        delivery,
    )
    .await?;
    Ok(body.and_then(|bytes| serde_json::from_slice(&bytes).ok()))
}

pub(super) fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

pub(super) fn is_event_stream(content_type: Option<&HeaderValue>) -> bool {
    content_type
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("text/event-stream")
        })
}

// ---------------------------------------------------------------------------
// Incremental bounded SSE framing
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SseParseError {
    /// A response-byte, line, or frame bound (max_response_bytes) was exceeded.
    LimitExceeded,
    /// A data payload was not valid UTF-8.
    InvalidUtf8,
}

/// One completed SSE event frame's data payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SseEvent {
    pub(super) data: String,
}

/// Incremental SSE parser: accepts CR, LF, and CRLF line endings, multi-line `data:`
/// frames joined with `\n`, comment lines, and arbitrary byte fragmentation. Bounds the
/// cumulative response bytes, the current line, and the current frame by
/// `ProtocolLimits::transport.max_response_bytes`. A frame completes at a blank line;
/// a trailing partial line at EOF is dropped (never synthesized into an event).
pub(super) struct SseParser {
    line: Vec<u8>,
    frame: Vec<u8>,
    frame_has_data: bool,
    pending_cr: bool,
    total_bytes: usize,
    maximum: usize,
}

impl SseParser {
    pub(super) fn new(maximum: usize) -> Self {
        Self {
            line: Vec::new(),
            frame: Vec::new(),
            frame_has_data: false,
            pending_cr: false,
            total_bytes: 0,
            maximum,
        }
    }

    pub(super) fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseParseError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .ok_or(SseParseError::LimitExceeded)?;
        if self.total_bytes > self.maximum {
            return Err(SseParseError::LimitExceeded);
        }
        let mut events = Vec::new();
        for &byte in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\r' => {
                    self.pending_cr = true;
                    self.end_line(&mut events)?;
                }
                b'\n' => self.end_line(&mut events)?,
                _ => {
                    self.line.push(byte);
                    if self.line.len() > self.maximum {
                        return Err(SseParseError::LimitExceeded);
                    }
                }
            }
        }
        Ok(events)
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) -> Result<(), SseParseError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            // Blank line: frame boundary.
            if self.frame_has_data {
                let data = std::mem::take(&mut self.frame);
                self.frame_has_data = false;
                events.push(SseEvent::finish(data)?);
            }
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(()); // comment line: ignored, does not start a frame
        }
        let Some(colon) = line.iter().position(|&byte| byte == b':') else {
            return Ok(()); // field without a colon is invalid and ignored
        };
        let field = &line[..colon];
        let mut value = &line[colon + 1..];
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        if field == b"data" {
            if !self.frame_has_data {
                self.frame_has_data = true;
            }
            self.frame.extend_from_slice(value);
            self.frame.push(b'\n');
            if self.frame.len() > self.maximum {
                return Err(SseParseError::LimitExceeded);
            }
        }
        // `event`, `id`, and `retry` fields are not consumed by either adapter.
        Ok(())
    }
}

impl SseEvent {
    fn finish(mut frame: Vec<u8>) -> Result<Self, SseParseError> {
        // The spec joins data lines with a trailing newline; strip exactly one so the
        // payload is the clean joined text.
        if frame.last() == Some(&b'\n') {
            frame.pop();
        }
        Ok(Self {
            data: String::from_utf8(frame).map_err(|_| SseParseError::InvalidUtf8)?,
        })
    }
}

// ---------------------------------------------------------------------------
// Shared deterministic loopback contract-test harness (test-only)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod loopback {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};

    use serde_json::Value;

    /// A captured client request: request line, headers (names lowercased), and the
    /// exact body bytes read per `Content-Length`.
    #[derive(Debug)]
    pub(crate) struct CapturedRequest {
        request_line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl CapturedRequest {
        pub(crate) fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
        }

        pub(crate) fn method(&self) -> &str {
            self.request_line
                .split_whitespace()
                .next()
                .unwrap_or_default()
        }

        pub(crate) fn path(&self) -> &str {
            self.request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
        }

        pub(crate) fn json_body(&self) -> Value {
            serde_json::from_slice(&self.body).expect("captured request body must be JSON")
        }

        /// The exact body bytes read per `Content-Length`, for byte-equality
        /// assertions between attempts.
        pub(crate) fn body_bytes(&self) -> &[u8] {
            &self.body
        }

        pub(crate) fn body_len(&self) -> usize {
            self.body.len()
        }
    }

    /// Byte marking the test-owned shutdown connection. Real requests always start with
    /// a method letter, so the first byte unambiguously distinguishes them.
    const POISON_BYTE: u8 = 0x00;

    const MAX_HEADER_BYTES: usize = 64 * 1024;

    /// One scripted response. `gate` is the number of complete events written before
    /// the server thread holds until the test releases it (deterministic cancellation
    /// probes); `0` writes the whole body immediately. Writes after the release may
    /// fail because the client already aborted; they are intentionally ignored.
    pub(crate) struct ScriptedResponse {
        pub(crate) status: u16,
        pub(crate) content_type: &'static str,
        pub(crate) headers: Vec<(String, String)>,
        pub(crate) body: String,
        pub(crate) gate: usize,
    }

    pub(crate) struct LoopbackServer {
        base_url: String,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        release: Option<Sender<()>>,
        handle: Option<JoinHandle<()>>,
    }

    impl LoopbackServer {
        pub(crate) fn spawn(scripted: Vec<ScriptedResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
            let base_url = format!("http://{}", listener.local_addr().expect("loopback addr"));
            let captured = Arc::new(Mutex::new(Vec::new()));
            let thread_captured = Arc::clone(&captured);
            let (release_tx, release_rx) = channel::<()>();
            let handle = thread::spawn(move || {
                let mut scripted = scripted.into_iter();
                loop {
                    let (mut stream, _peer) = listener.accept().expect("loopback accept");
                    let mut first = [0u8; 1];
                    let n = stream.read(&mut first).expect("read first request byte");
                    if n == 1 && first[0] == POISON_BYTE {
                        return; // test-owned shutdown signal
                    }
                    if n == 0 {
                        panic!("connection closed before any request bytes");
                    }
                    let scripted = scripted.next().unwrap_or_else(|| {
                        panic!("unexpected HTTP request: no scripted response left")
                    });
                    serve_one(
                        &mut stream,
                        first[0],
                        &scripted,
                        thread_captured.as_ref(),
                        &release_rx,
                    );
                }
            });
            Self {
                base_url,
                captured,
                release: Some(release_tx),
                handle: Some(handle),
            }
        }

        /// The full `/responses` endpoint exercised by the OpenAI adapter in every
        /// loopback test: the production constructor stores the endpoint exactly as
        /// given, and the wire contract is the OpenAI path, so requests must land on
        /// `/responses`.
        pub(crate) fn responses_endpoint(&self) -> String {
            format!("{}/responses", self.base_url)
        }

        /// The full `/v1/messages` endpoint exercised by the Anthropic adapter in
        /// every loopback test (the production constructor stores the endpoint
        /// exactly as given).
        pub(crate) fn messages_endpoint(&self) -> String {
            format!("{}/v1/messages", self.base_url)
        }

        pub(crate) fn release(&self) -> &Sender<()> {
            self.release.as_ref().expect("release channel present")
        }

        /// Release any gate (idempotent), poison the thread, join it, and return the
        /// captured requests. Always called before assertions so the thread can never
        /// outlive the test whatever the outcome.
        pub(crate) fn join(mut self) -> Vec<CapturedRequest> {
            if let Some(release) = &self.release {
                let _ = release.send(());
            }
            let handle = self.handle.take().expect("server must be joined once");
            if let Ok(mut stream) =
                TcpStream::connect(self.base_url.strip_prefix("http://").unwrap())
            {
                let _ = stream.write_all(&[POISON_BYTE]);
            }
            handle
                .join()
                .expect("loopback server thread must not panic");
            Arc::try_unwrap(std::mem::take(&mut self.captured))
                .expect("captured requests must have no other owners")
                .into_inner()
                .expect("capture mutex must not be poisoned")
        }
    }

    impl Drop for LoopbackServer {
        fn drop(&mut self) {
            // Best-effort settle if the test failed before `join`.
            if let Some(release) = &self.release {
                let _ = release.send(());
            }
            if let Some(handle) = self.handle.take() {
                if let Ok(mut stream) =
                    TcpStream::connect(self.base_url.strip_prefix("http://").unwrap())
                {
                    let _ = stream.write_all(&[POISON_BYTE]);
                }
                let _ = handle.join();
            }
        }
    }

    /// Read one request off the accepted stream (whose first byte was already consumed)
    /// and respond with the scripted body.
    fn serve_one(
        stream: &mut TcpStream,
        first_byte: u8,
        scripted: &ScriptedResponse,
        captured: &Mutex<Vec<CapturedRequest>>,
        release_rx: &Receiver<()>,
    ) {
        let mut buf = vec![first_byte];
        let mut scratch = [0u8; 4096];
        let header_end = loop {
            assert!(
                buf.len() <= MAX_HEADER_BYTES,
                "request headers exceed {MAX_HEADER_BYTES} bytes"
            );
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
            let n = stream.read(&mut scratch).expect("read request bytes");
            if n == 0 {
                panic!("connection closed before request headers were complete");
            }
            buf.extend_from_slice(&scratch[..n]);
        };

        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().expect("request line").to_string();
        let headers: Vec<(String, String)> = lines
            .filter(|line| !line.is_empty())
            .map(|line| {
                let (name, value) = line
                    .split_once(':')
                    .unwrap_or_else(|| panic!("malformed header line: {line:?}"));
                (name.trim().to_ascii_lowercase(), value.trim().to_string())
            })
            .collect();
        let content_length: usize = headers
            .iter()
            .find_map(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_else(|| panic!("request missing numeric Content-Length: {headers:?}"));

        let mut body = buf[header_end + 4..].to_vec();
        assert!(
            body.len() <= content_length,
            "request body exceeds declared Content-Length"
        );
        let mut rest = vec![0u8; content_length - body.len()];
        stream.read_exact(&mut rest).expect("read request body");
        body.extend_from_slice(&rest);

        captured
            .lock()
            .expect("captured requests mutex")
            .push(CapturedRequest {
                request_line,
                headers,
                body,
            });

        let reason = match scripted.status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            402 => "Payment Required",
            403 => "Forbidden",
            413 => "Content Too Large",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            529 => "Service Overloaded",
            status if (400..=499).contains(&status) => "Client Error",
            status if (500..=599).contains(&status) => "Server Error",
            other => panic!("unscripted response status {other}"),
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n",
            scripted.status,
            scripted.content_type,
            scripted.body.len(),
        );
        for (name, value) in &scripted.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .expect("write scripted response head");
        if scripted.gate > 0 {
            let mut boundaries = Vec::new();
            let mut search_from = 0;
            while boundaries.len() < scripted.gate {
                match scripted.body[search_from..].find("\n\n") {
                    Some(offset) => {
                        let boundary = search_from + offset;
                        boundaries.push(boundary);
                        search_from = boundary + 2;
                    }
                    None => panic!(
                        "gated body must contain at least {} event boundaries",
                        scripted.gate
                    ),
                }
            }
            let last_boundary = boundaries[scripted.gate - 1];
            let (written, rest) = scripted.body.split_at(last_boundary + 2);
            stream
                .write_all(written.as_bytes())
                .expect("write gated events");
            let _ = release_rx.recv();
            // The client may have aborted after cancellation: best-effort only.
            let _ = stream.write_all(rest.as_bytes());
        } else {
            stream
                .write_all(scripted.body.as_bytes())
                .expect("write scripted response body");
        }
        let _ = stream.shutdown(Shutdown::Write);
    }
}
