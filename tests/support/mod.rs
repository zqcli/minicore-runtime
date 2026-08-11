//! Shared loopback HTTP/1.1 test server for the M12 rig integration tests.
//!
//! A test-owned blocking [`TcpListener`] on `127.0.0.1:0` serves one scripted
//! `(status, body)` response per accepted connection, in order. Every
//! response carries `Content-Length` and `Connection: close`, so each request
//! arrives on a fresh connection and no keep-alive reuse can hide a second
//! request. The test settles the thread through a dedicated poison connection
//! and joins it before asserting, so nothing here sleeps, spins, or times out.
//!
//! Responses are served either as JSON ([`LoopbackServer::spawn`]) or as an
//! SSE stream ([`LoopbackServer::spawn_sse`], `Content-Type:
//! text/event-stream`); both share the identical blocking, no-sleep,
//! no-timeout lifecycle.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde_json::Value;

/// A captured client request: request line, headers (names lowercased), and
/// the exact body bytes read per `Content-Length`.
#[derive(Debug)]
pub struct CapturedRequest {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
    }

    pub fn method(&self) -> &str {
        self.request_line
            .split_whitespace()
            .next()
            .unwrap_or_default()
    }

    pub fn path(&self) -> &str {
        self.request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
    }

    pub fn json_body(&self) -> Value {
        serde_json::from_slice(&self.body).expect("captured request body must be JSON")
    }

    /// Bytes actually read for the body, exactly `Content-Length` of them.
    pub fn body_len(&self) -> usize {
        self.body.len()
    }
}

/// Byte marking the test-owned shutdown connection. Real requests always
/// start with a method letter, so the first byte unambiguously distinguishes
/// them from the poison signal.
const POISON_BYTE: u8 = 0x00;

const MAX_HEADER_BYTES: usize = 64 * 1024;

/// The wire content type a scripted response is served as.
///
/// Each integration-test binary compiles this shared module standalone and
/// uses only one of the two constructors, so both variants are `#[allow(dead_code)]`.
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum ResponseKind {
    /// `Content-Type: application/json` (the original contract).
    Json,
    /// `Content-Type: text/event-stream` for SSE streaming probes.
    Sse,
}

/// One scripted response: status, wire content type, and exact body.
struct ScriptedResponse {
    status: u16,
    kind: ResponseKind,
    body: String,
}

/// Serves one scripted response per accepted connection, in order, until the
/// test opens a poison connection carrying [`POISON_BYTE`]. A real request
/// with no scripted response left (a protocol violation) panics the thread,
/// which surfaces through [`LoopbackServer::join`].
pub struct LoopbackServer {
    base_url: String,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    handle: Option<JoinHandle<()>>,
}

impl LoopbackServer {
    /// Serve one JSON response per accepted connection, in order.
    #[allow(dead_code)] // unused by the SSE-only test binary
    pub fn spawn(scripted: &[(u16, &str)]) -> Self {
        let scripted = scripted
            .iter()
            .map(|(status, body)| ScriptedResponse {
                status: *status,
                kind: ResponseKind::Json,
                body: body.to_string(),
            })
            .collect();
        Self::spawn_scripted(scripted)
    }

    /// Same contract as [`Self::spawn`], but every response is served with
    /// `Content-Type: text/event-stream` so rig's reqwest SSE event source
    /// accepts the body (its `check_response` requires status 200 and the
    /// `text/event-stream` mime type). Everything else — exact
    /// `Content-Length`, `Connection: close`, one blocking write per
    /// connection, the poison/join lifecycle — is identical, and there is no
    /// sleep, timeout, yield, or polling.
    #[allow(dead_code)] // unused by the JSON-only test binaries
    pub fn spawn_sse(scripted: &[(u16, &str)]) -> Self {
        let scripted = scripted
            .iter()
            .map(|(status, body)| ScriptedResponse {
                status: *status,
                kind: ResponseKind::Sse,
                body: body.to_string(),
            })
            .collect();
        Self::spawn_scripted(scripted)
    }

    fn spawn_scripted(scripted: Vec<ScriptedResponse>) -> Self {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("loopback server must bind an ephemeral port");
        let base_url = format!(
            "http://{}",
            listener.local_addr().expect("loopback server address")
        );

        let captured = Arc::new(Mutex::new(Vec::new()));
        let thread_captured = Arc::clone(&captured);

        let handle = thread::spawn(move || {
            let mut scripted = scripted.into_iter();
            loop {
                let (mut stream, _peer) = listener.accept().expect("loopback accept must succeed");
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
                serve_one(&mut stream, first[0], &scripted, thread_captured.as_ref());
            }
        });

        Self {
            base_url,
            captured,
            handle: Some(handle),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Open the poison connection so the thread exits, join it, and return
    /// the captured requests. Always called before assertions, so the thread
    /// can never outlive the test whatever the completion outcome.
    pub fn join(mut self) -> Vec<CapturedRequest> {
        let handle = self.handle.take().expect("server must be joined once");
        // If the thread already panicked and exited, join below reports it.
        if let Ok(mut stream) = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap()) {
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
        // Best-effort settle if the test failed before `join`, so a panicking
        // test cannot leave a thread blocking the test binary.
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

/// Read one request off the accepted stream (whose first byte was already
/// consumed) and respond with the scripted body.
fn serve_one(
    stream: &mut TcpStream,
    first_byte: u8,
    scripted: &ScriptedResponse,
    captured: &Mutex<Vec<CapturedRequest>>,
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

    // Body bytes already buffered past the header terminator, then the exact
    // Content-Length remainder.
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
        500 => "Internal Server Error",
        other => panic!("unscripted response status {other}"),
    };
    let content_type = match scripted.kind {
        ResponseKind::Json => "application/json",
        ResponseKind::Sse => "text/event-stream",
    };
    let response = format!(
        "HTTP/1.1 {} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        scripted.status,
        scripted.body.len(),
        scripted.body
    );
    stream
        .write_all(response.as_bytes())
        .expect("write scripted response");
    let _ = stream.shutdown(Shutdown::Write);
}
