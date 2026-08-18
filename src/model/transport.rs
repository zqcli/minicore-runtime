use std::time::Duration;

use futures_util::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::types::{DeliveryState, ModelError, ModelErrorKind};

pub(crate) const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
pub(crate) const MAX_RESPONSE_BYTES: usize = 8_388_608;

pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .user_agent(USER_AGENT)
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
}

fn checked_error(kind: ModelErrorKind, delivery: DeliveryState) -> ModelError {
    ModelError::detailed(kind, delivery, None).unwrap_or(ModelError::Internal)
}

pub(crate) fn cancelled(delivery: DeliveryState) -> ModelError {
    checked_error(ModelErrorKind::Cancelled, delivery)
}

pub(crate) fn invalid_provider_response(delivery: DeliveryState) -> ModelError {
    checked_error(ModelErrorKind::InvalidResponse, delivery)
}

pub(crate) fn invalid_request_not_sent() -> ModelError {
    checked_error(ModelErrorKind::InvalidRequest, DeliveryState::NotSent)
}

pub(crate) fn transport_read_error(delivery: DeliveryState) -> ModelError {
    match delivery {
        DeliveryState::NotSent | DeliveryState::RejectedBeforeExecution => {
            checked_error(ModelErrorKind::TransportUnavailable, delivery)
        }
        DeliveryState::AcceptedNoOutput | DeliveryState::Unknown => checked_error(
            ModelErrorKind::RequestOutcomeUnknown,
            DeliveryState::Unknown,
        ),
        DeliveryState::OutputStarted => checked_error(
            ModelErrorKind::StreamInterrupted,
            DeliveryState::OutputStarted,
        ),
    }
}

pub(crate) fn classify_send_phase(is_connect: bool, is_timeout: bool) -> ModelError {
    if is_connect && is_timeout {
        return checked_error(ModelErrorKind::Timeout, DeliveryState::NotSent);
    }
    if is_connect {
        return checked_error(ModelErrorKind::TransportUnavailable, DeliveryState::NotSent);
    }
    checked_error(
        ModelErrorKind::RequestOutcomeUnknown,
        DeliveryState::Unknown,
    )
}

pub(crate) fn classify_send_error(error: &reqwest::Error) -> ModelError {
    classify_send_phase(error.is_connect(), error.is_timeout())
}

pub(crate) async fn drain_bounded<C, E, S>(
    stream: S,
    cancel: &CancellationToken,
    maximum: usize,
    delivery: DeliveryState,
) -> Result<Option<Vec<u8>>, ModelError>
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

pub(crate) async fn read_bounded_envelope(
    response: reqwest::Response,
    cancel: &CancellationToken,
    delivery: DeliveryState,
) -> Result<Option<Value>, ModelError> {
    let body = drain_bounded(
        response.bytes_stream(),
        cancel,
        MAX_RESPONSE_BYTES,
        delivery,
    )
    .await?;
    Ok(body.and_then(|bytes| serde_json::from_slice(&bytes).ok()))
}

pub(crate) fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .and_then(|seconds| seconds.checked_mul(1_000))
        .map(Duration::from_millis)
}

pub(crate) fn is_event_stream(content_type: Option<&HeaderValue>) -> bool {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SseParseError {
    LimitExceeded,
    InvalidUtf8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SseEvent {
    pub(crate) data: String,
}

pub(crate) struct SseParser {
    line: Vec<u8>,
    frame: Vec<u8>,
    frame_has_data: bool,
    pending_cr: bool,
    total_bytes: usize,
    maximum: usize,
}

impl SseParser {
    pub(crate) fn new(maximum: usize) -> Self {
        Self {
            line: Vec::new(),
            frame: Vec::new(),
            frame_has_data: false,
            pending_cr: false,
            total_bytes: 0,
            maximum,
        }
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseParseError> {
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
            if self.frame_has_data {
                let data = std::mem::take(&mut self.frame);
                self.frame_has_data = false;
                events.push(SseEvent::finish(data)?);
            }
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(());
        }
        let Some(colon) = line.iter().position(|&byte| byte == b':') else {
            return Ok(());
        };
        let field = &line[..colon];
        let mut value = &line[colon + 1..];
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        if field == b"data" {
            self.frame_has_data = true;
            self.frame.extend_from_slice(value);
            self.frame.push(b'\n');
            if self.frame.len() > self.maximum {
                return Err(SseParseError::LimitExceeded);
            }
        }
        Ok(())
    }
}

impl SseEvent {
    fn finish(mut frame: Vec<u8>) -> Result<Self, SseParseError> {
        if frame.last() == Some(&b'\n') {
            frame.pop();
        }
        Ok(Self {
            data: String::from_utf8(frame).map_err(|_| SseParseError::InvalidUtf8)?,
        })
    }
}

/// Keeps the crate-private transport seam linked while adapter migration is deferred.
/// This only references function items and constants; it does not build a client or do I/O.
pub(crate) fn ensure_linked() {
    let _ = USER_AGENT;
    let _ = MAX_RESPONSE_BYTES;
    let _ = client_builder as fn() -> reqwest::ClientBuilder;
    let _ = checked_error as fn(ModelErrorKind, DeliveryState) -> ModelError;
    let _ = cancelled as fn(DeliveryState) -> ModelError;
    let _ = invalid_provider_response as fn(DeliveryState) -> ModelError;
    let _ = invalid_request_not_sent as fn() -> ModelError;
    let _ = transport_read_error as fn(DeliveryState) -> ModelError;
    let _ = classify_send_phase as fn(bool, bool) -> ModelError;
    let _ = classify_send_error as fn(&reqwest::Error) -> ModelError;
    let _ = drain_bounded::<Vec<u8>, (), futures_util::stream::Empty<Result<Vec<u8>, ()>>>;
    let _ = read_bounded_envelope;
    let _ = parse_retry_after;
    let _ = is_event_stream;
    let _ = SseParser::new;
    let _ = SseParser::feed;
    let _ = SseEvent::finish;
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::task::Poll;
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use futures_util::stream;
    use reqwest::header::{HeaderMap, HeaderValue};
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    use super::{
        DeliveryState, ModelErrorKind, SseEvent, SseParseError, SseParser, USER_AGENT,
        classify_send_phase, client_builder, drain_bounded, invalid_provider_response,
        invalid_request_not_sent, is_event_stream, parse_retry_after, transport_read_error,
    };

    struct ScriptedResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl ScriptedResponse {
        fn plain(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.into(),
            }
        }
    }

    #[derive(Debug)]
    struct CapturedRequest {
        headers: Vec<(String, String)>,
    }

    impl CapturedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
        }
    }

    struct LoopbackServer {
        addr: SocketAddr,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        handle: Option<JoinHandle<()>>,
    }

    impl LoopbackServer {
        fn spawn(scripted: Vec<ScriptedResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
            let addr = listener.local_addr().expect("loopback address");
            let captured = Arc::new(Mutex::new(Vec::new()));
            let thread_captured = Arc::clone(&captured);
            let handle = thread::spawn(move || {
                let mut scripted = scripted.into_iter();
                loop {
                    let (mut stream, _) = listener.accept().expect("loopback accept");
                    let mut first = [0u8; 1];
                    stream.read_exact(&mut first).expect("read first byte");
                    if first[0] == 0 {
                        return;
                    }
                    let response = scripted
                        .next()
                        .unwrap_or_else(|| panic!("unexpected request"));
                    let mut buf = vec![first[0]];
                    let mut scratch = [0u8; 4096];
                    let header_end = loop {
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos;
                        }
                        let n = stream.read(&mut scratch).expect("read request");
                        assert!(n > 0, "request ended before headers");
                        buf.extend_from_slice(&scratch[..n]);
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]);
                    let mut lines = head.split("\r\n");
                    let _request_line = lines.next().expect("request line");
                    let headers = lines
                        .filter(|line| !line.is_empty())
                        .map(|line| {
                            let (name, value) = line.split_once(':').expect("header colon");
                            (name.trim().to_ascii_lowercase(), value.trim().to_owned())
                        })
                        .collect();
                    thread_captured
                        .lock()
                        .expect("capture mutex")
                        .push(CapturedRequest { headers });
                    let reason = match response.status {
                        200 => "OK",
                        302 => "Found",
                        500 => "Internal Server Error",
                        other => panic!("unexpected status {other}"),
                    };
                    let mut response_head = format!(
                        "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        response.status,
                        response.body.len()
                    );
                    for (name, value) in response.headers {
                        response_head.push_str(&name);
                        response_head.push_str(": ");
                        response_head.push_str(&value);
                        response_head.push_str("\r\n");
                    }
                    response_head.push_str("\r\n");
                    stream
                        .write_all(response_head.as_bytes())
                        .expect("write response head");
                    stream
                        .write_all(&response.body)
                        .expect("write response body");
                    let _ = stream.shutdown(Shutdown::Write);
                }
            });
            Self {
                addr,
                captured,
                handle: Some(handle),
            }
        }

        fn join(mut self) -> Vec<CapturedRequest> {
            if let Ok(mut poison) = TcpStream::connect(self.addr) {
                let _ = poison.write_all(&[0]);
            }
            self.handle
                .take()
                .expect("server join once")
                .join()
                .expect("server thread must not panic");
            Arc::try_unwrap(std::mem::take(&mut self.captured))
                .expect("capture owners")
                .into_inner()
                .expect("capture mutex")
        }
    }

    impl Drop for LoopbackServer {
        fn drop(&mut self) {
            if let Ok(mut poison) = TcpStream::connect(self.addr) {
                let _ = poison.write_all(&[0]);
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn gzip_stored(payload: &[u8]) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = 0xffff_ffffu32;
            for &byte in bytes {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xedb8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        let mut output = vec![0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0xff, 0x01];
        let length = u16::try_from(payload.len()).expect("stored test block bound");
        output.extend_from_slice(&length.to_le_bytes());
        output.extend_from_slice(&(!length).to_le_bytes());
        output.extend_from_slice(payload);
        output.extend_from_slice(&crc32(payload).to_le_bytes());
        output.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        output
    }

    #[tokio::test(flavor = "current_thread")]
    async fn client_policy_has_fixed_user_agent_redirect_and_no_retry_or_decompression() {
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 302,
            headers: vec![("Location".to_owned(), "/next".to_owned())],
            body: Vec::new(),
        }]);
        let client = client_builder().build().expect("client builds");
        let response = client
            .get(format!("http://{}/start", server.addr))
            .send()
            .await
            .expect("redirect response");
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        let captured = server.join();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].header("user-agent"), Some(USER_AGENT));

        let server = LoopbackServer::spawn(vec![ScriptedResponse::plain(500, "boom")]);
        let response = client
            .get(format!("http://{}/retry", server.addr))
            .send()
            .await
            .expect("5xx response");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(server.join().len(), 1);

        let payload = b"raw gzip";
        let gzip = gzip_stored(payload);
        let server = LoopbackServer::spawn(vec![ScriptedResponse {
            status: 200,
            headers: vec![("Content-Encoding".to_owned(), "gzip".to_owned())],
            body: gzip.clone(),
        }]);
        let response = client
            .get(format!("http://{}/gzip", server.addr))
            .send()
            .await
            .expect("gzip response");
        assert_eq!(response.bytes().await.unwrap().as_ref(), gzip.as_slice());
        assert_eq!(server.join().len(), 1);
    }

    #[test]
    fn retry_after_and_event_stream_content_type_are_strict() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("12"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(12)));
        headers.insert("retry-after", HeaderValue::from_static("12.5"));
        assert_eq!(parse_retry_after(&headers), None);
        headers.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&headers), None);
        let maximum_seconds = u64::MAX / 1_000;
        headers.insert(
            "retry-after",
            maximum_seconds.to_string().parse().expect("valid header"),
        );
        assert_eq!(
            parse_retry_after(&headers),
            Some(Duration::from_millis(maximum_seconds * 1_000))
        );
        headers.insert(
            "retry-after",
            (maximum_seconds + 1)
                .to_string()
                .parse()
                .expect("valid header"),
        );
        assert_eq!(parse_retry_after(&headers), None);
        assert!(is_event_stream(Some(&HeaderValue::from_static(
            "text/event-stream"
        ))));
        assert!(is_event_stream(Some(&HeaderValue::from_static(
            "Text/Event-Stream; charset=utf-8",
        ))));
        assert!(!is_event_stream(Some(&HeaderValue::from_static(
            "application/json"
        ))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_drain_handles_exact_bound_errors_and_cancellation() {
        let cancel = CancellationToken::new();
        assert_eq!(
            drain_bounded(
                stream::iter(vec![Ok::<_, ()>(b"ab".to_vec()), Ok(b"cd".to_vec())]),
                &cancel,
                4,
                DeliveryState::AcceptedNoOutput,
            )
            .await
            .unwrap(),
            Some(b"abcd".to_vec())
        );
        assert_eq!(
            drain_bounded(
                stream::iter(vec![Ok::<_, ()>(b"abcde".to_vec())]),
                &cancel,
                4,
                DeliveryState::AcceptedNoOutput,
            )
            .await
            .unwrap(),
            None
        );
        assert_eq!(
            drain_bounded(
                stream::iter(vec![Ok::<_, ()>(b"ok".to_vec()), Err(())]),
                &cancel,
                4,
                DeliveryState::AcceptedNoOutput,
            )
            .await
            .unwrap(),
            None
        );

        let started = Arc::new(Notify::new());
        let stream_started = Arc::clone(&started);
        let mut yielded = false;
        let stream = futures_util::stream::poll_fn(move |_cx| {
            if !yielded {
                yielded = true;
                Poll::Ready(Some(Ok::<_, ()>(b"first".to_vec())))
            } else {
                stream_started.notify_one();
                Poll::Pending
            }
        });
        let cancel = CancellationToken::new();
        let mut drain = Box::pin(drain_bounded(
            stream,
            &cancel,
            64,
            DeliveryState::OutputStarted,
        ));
        let started_wait = started.notified();
        tokio::select! {
            biased;
            result = &mut drain => panic!("drain completed unexpectedly: {result:?}"),
            _ = started_wait => {
                cancel.cancel();
                let error = drain.await.unwrap_err();
                assert_eq!(error.kind(), ModelErrorKind::Cancelled);
                assert_eq!(error.delivery(), DeliveryState::OutputStarted);
            }
        }
    }

    #[test]
    fn send_and_read_error_normalization_uses_conservative_matrix() {
        assert_eq!(
            classify_send_phase(true, true).kind(),
            ModelErrorKind::Timeout
        );
        assert_eq!(
            classify_send_phase(true, true).delivery(),
            DeliveryState::NotSent
        );
        assert_eq!(
            classify_send_phase(true, false).kind(),
            ModelErrorKind::TransportUnavailable
        );
        assert_eq!(
            classify_send_phase(true, false).delivery(),
            DeliveryState::NotSent
        );
        assert_eq!(
            classify_send_phase(false, true).kind(),
            ModelErrorKind::RequestOutcomeUnknown
        );
        assert_eq!(
            classify_send_phase(false, false).delivery(),
            DeliveryState::Unknown
        );
        for (delivery, expected_kind, expected_delivery) in [
            (
                DeliveryState::NotSent,
                ModelErrorKind::TransportUnavailable,
                DeliveryState::NotSent,
            ),
            (
                DeliveryState::RejectedBeforeExecution,
                ModelErrorKind::TransportUnavailable,
                DeliveryState::RejectedBeforeExecution,
            ),
            (
                DeliveryState::AcceptedNoOutput,
                ModelErrorKind::RequestOutcomeUnknown,
                DeliveryState::Unknown,
            ),
            (
                DeliveryState::OutputStarted,
                ModelErrorKind::StreamInterrupted,
                DeliveryState::OutputStarted,
            ),
            (
                DeliveryState::Unknown,
                ModelErrorKind::RequestOutcomeUnknown,
                DeliveryState::Unknown,
            ),
        ] {
            let error = transport_read_error(delivery);
            assert_eq!(error.kind(), expected_kind);
            assert_eq!(error.delivery(), expected_delivery);
        }
        assert_eq!(
            invalid_provider_response(DeliveryState::OutputStarted).kind(),
            ModelErrorKind::InvalidResponse
        );
        assert_eq!(
            invalid_request_not_sent().delivery(),
            DeliveryState::NotSent
        );
    }

    #[test]
    fn sse_parser_preserves_fragmentation_comments_multiline_and_eof_rules() {
        let mut parser = SseParser::new(128);
        assert!(
            parser
                .feed(b": comment\r\ndata: one\r\ndata: two\n\npart")
                .unwrap()
                .len()
                == 1
        );
        assert_eq!(parser.feed(b"ial\r\n\n").unwrap(), Vec::<SseEvent>::new());
        assert_eq!(
            parser.feed(b"data: final\r\n\r\n").unwrap(),
            vec![SseEvent {
                data: "final".to_owned()
            }]
        );

        let mut fragmented = SseParser::new(128);
        let mut events = Vec::new();
        for chunk in [
            b"data: a\r".as_slice(),
            b"\ndata: b\r\n\r".as_slice(),
            b"\n".as_slice(),
        ] {
            events.extend(fragmented.feed(chunk).unwrap());
        }
        assert_eq!(
            events,
            vec![SseEvent {
                data: "a\nb".into()
            }]
        );
    }

    #[test]
    fn sse_parser_rejects_utf8_and_all_bounds() {
        let mut invalid = SseParser::new(64);
        assert_eq!(
            invalid.feed(b"data: \xff\n\n"),
            Err(SseParseError::InvalidUtf8)
        );
        let mut cumulative = SseParser::new(5);
        assert_eq!(cumulative.feed(b"x"), Ok(Vec::new()));
        assert_eq!(cumulative.feed(b"yyyyy"), Err(SseParseError::LimitExceeded));
        let mut line = SseParser::new(4);
        assert_eq!(line.feed(b"abcde"), Err(SseParseError::LimitExceeded));
        let mut frame = SseParser::new(8);
        assert_eq!(
            frame.feed(b"data: abc\n"),
            Err(SseParseError::LimitExceeded)
        );
        let mut partial = SseParser::new(64);
        assert!(partial.feed(b"data: partial\n").unwrap().is_empty());
    }
}
