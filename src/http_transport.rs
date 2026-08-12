//! Crate-private, protocol-neutral shared HTTP transport owner.
//!
//! Only the one deep primitive with two real production consumers lives here: the
//! locked-down `reqwest` client builder (no redirect following, no automatic
//! retries, no ambient proxy, an explicit fixed product [`USER_AGENT`], and
//! explicit no-compression flags) that the direct provider adapters
//! (`openai_responses`, `anthropic_messages`) and the `fetch_url` builtin's
//! per-origin pinned clients both install.  Everything provider-specific — request
//! encoding, event dispatch, terminal parsers, error taxonomy, SSE and JSON
//! envelope framing — stays in the owning adapter modules under
//! `model_gateway::provider_transport`, and everything fetch_url-specific —
//! https-only enforcement, zero idle pool, fixed connect/request timeouts,
//! reject-all DNS plus the exact hostname `resolve_to_addrs` override, and the
//! same-origin authorization seam — stays in `tools::fetch_url`.  There is
//! deliberately no generic HTTP request registry, no per-request options, and no
//! shared wire policy here: the builder is the only shared surface.
//!
//! The deterministic `127.0.0.1:0` loopback contract-test harness remains shared
//! `#[cfg(test)]` infrastructure under `model_gateway::provider_transport`; the
//! fetch_url module owns its own minimal loopback seam because Tools cannot depend
//! on provider-owned parsers.

/// The fixed product `User-Agent` every shared-client request carries.  Compiled
/// from the Cargo package name/version so it can never drift from the shipped
/// artifact; stable, nonsecret, and protocol-neutral (no browser disguise, no
/// per-provider value).  Gateway/WAF policy may rely on it: a compatible HTTPS
/// gateway behind a Cloudflare-style rule rejects UA-less requests with HTTP 403
/// while this identifier passes, and it is identical for the OpenAI and Anthropic
/// adapters and for every fetch_url origin client.
pub(crate) const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// The locked-down builder deep primitive every production HTTP consumer installs.
///
/// Transport properties the builder itself guarantees on every client it builds:
///
/// - it never follows redirects: a 3xx response is returned to the caller as the
///   final response (no automatic redirect chain);
/// - it never retries automatically: the transport does not initiate an automatic
///   retry request, whatever the status or connect outcome (a single attempt's
///   connector may still probe several resolved addresses before one connection
///   succeeds; that is connect work inside one attempt, not a retry request);
/// - it never reads an ambient proxy: no environment/global proxy applies;
/// - it installs the explicit fixed product [`USER_AGENT`] on every request;
/// - it sets explicit `no_gzip`/`no_brotli`/`no_zstd`/`no_deflate` so dependency
///   feature unification can never silently enable response decompression (the
///   pinned reqwest feature set already ships no codec, and the flags keep it
///   that way by contract).
///
/// What the builder does *not* guarantee is caller behavior: it does not enforce
/// that a consumer sends exactly one request per attempt, and it does not compose
/// or verify a complete full request.  Send-once-per-attempt and complete-request
/// composition are each consumer's responsibility.
///
/// The builder carries no timeouts and no pool policy: those are per-consumer
/// contract.  The provider adapters build it unchanged (preserving the existing
/// single-attempt semantics); `tools::fetch_url` adds `https_only`, zero idle pool,
/// fixed connect/request timeouts, and the pinned reject-all DNS resolver on top.
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

/// Builds the locked-down production HTTP client shared by every direct provider
/// adapter.  This is the historical `provider_transport::build_client` contract,
/// lifted to the shared owner unchanged: the adapters keep their exact single-attempt
/// semantics and their typed `ClientBuild` error mapping.
pub(crate) fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    client_builder().build()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use super::*;

    // `no_proxy()` (the ambient-proxy disable) is not asserted here by mutating
    // environment variables: env changes are process-global and would race with
    // every other test in the same harness, so the explicit `.no_proxy()` builder
    // call above is the production contract.  The loopback suites in this crate
    // (provider and fetch_url) only regression-test the direct behavior: a request
    // reaching the local `127.0.0.1` listener shows that no proxy was observed in
    // this environment, which sets no proxy variables.  They cannot prove how a
    // proxy-populated environment would behave, and no test here claims to prove
    // proxy-environment behavior.

    /// One scripted response.  The server serves exactly the scripted responses in
    /// order, one connection each; any request beyond the scripted count makes the
    /// server thread panic, so a test fails deterministically if the client sent a
    /// second request — no sleeps and no timeouts are ever used as evidence.
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

    /// A captured request: request line plus header names lowercased.
    #[derive(Debug)]
    struct CapturedRequest {
        request_line: String,
        headers: Vec<(String, String)>,
    }

    impl CapturedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
        }

        fn path(&self) -> &str {
            self.request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
        }
    }

    /// Minimal deterministic loopback server owned by these tests: a nonblocking
    /// `127.0.0.1:0` listener (each accepted stream is explicitly reset to blocking
    /// before its reads, so the nonblocking mode never leaks onto request I/O)
    /// served by one thread, recording the request head of every accepted connection
    /// and answering each with the next scripted response
    /// (always `Connection: close`, so no response is ever reused on a later
    /// connection).  `join` stops the accept loop, joins the thread, and propagates
    /// any server-thread panic (including the contract panic on an unexpected
    /// second request).
    struct TestServer {
        addr: SocketAddr,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn spawn(scripted: Vec<ScriptedResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
            listener.set_nonblocking(true).expect("nonblocking accept");
            let addr = listener.local_addr().expect("loopback address");
            let captured = Arc::new(Mutex::new(Vec::new()));
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread_captured = Arc::clone(&captured);
            let thread_shutdown = Arc::clone(&shutdown);
            let handle = thread::spawn(move || {
                let mut scripted = scripted.into_iter();
                loop {
                    if thread_shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((mut stream, _peer)) => {
                            // The listener is nonblocking only so the accept loop can
                            // poll the shutdown flag; the accepted stream inherits that
                            // nonblocking mode on macOS (and other platforms), which
                            // would make the blocking read loop below fail with
                            // WouldBlock.  Reset the stream to blocking before any read.
                            stream.set_nonblocking(false).expect("blocking stream");
                            let response = scripted.next().unwrap_or_else(|| {
                                panic!(
                                    "unexpected request: the client sent more than one request \
                                     against a single-response script"
                                )
                            });
                            let mut buf = Vec::new();
                            let mut scratch = [0u8; 4096];
                            let header_end = loop {
                                if let Some(pos) =
                                    buf.windows(4).position(|window| window == b"\r\n\r\n")
                                {
                                    break pos;
                                }
                                let n = stream.read(&mut scratch).expect("read request bytes");
                                if n == 0 {
                                    panic!("client closed before request headers were complete");
                                }
                                buf.extend_from_slice(&scratch[..n]);
                            };
                            let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                            let mut lines = head.split("\r\n");
                            let request_line = lines.next().expect("request line").to_string();
                            let headers = lines
                                .filter(|line| !line.is_empty())
                                .map(|line| {
                                    let (name, value) =
                                        line.split_once(':').expect("header name colon");
                                    (name.trim().to_ascii_lowercase(), value.trim().to_string())
                                })
                                .collect();
                            thread_captured
                                .lock()
                                .expect("captured requests mutex is not poisoned")
                                .push(CapturedRequest {
                                    request_line,
                                    headers,
                                });
                            let reason = match response.status {
                                200 => "OK",
                                302 => "Found",
                                500 => "Internal Server Error",
                                other => panic!("unscripted response status {other}"),
                            };
                            let mut head = format!(
                                "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                                response.status,
                                response.body.len(),
                            );
                            for (name, value) in &response.headers {
                                head.push_str(name);
                                head.push_str(": ");
                                head.push_str(value);
                                head.push_str("\r\n");
                            }
                            head.push_str("\r\n");
                            stream
                                .write_all(head.as_bytes())
                                .expect("write scripted response head");
                            stream
                                .write_all(&response.body)
                                .expect("write scripted response body");
                            let _ = stream.shutdown(Shutdown::Write);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return,
                    }
                }
            });
            Self {
                addr,
                captured,
                shutdown,
                handle: Some(handle),
            }
        }

        fn addr(&self) -> SocketAddr {
            self.addr
        }

        /// Stops the accept loop, joins the server thread, and returns the captured
        /// requests; a server-thread panic (e.g. an unexpected second request) fails
        /// the test here.
        fn join(mut self) -> Vec<CapturedRequest> {
            self.shutdown.store(true, Ordering::SeqCst);
            let handle = self.handle.take().expect("server joined once");
            handle
                .join()
                .expect("the loopback server thread must not panic");
            Arc::try_unwrap(std::mem::take(&mut self.captured))
                .expect("captured requests must have no other owners")
                .into_inner()
                .expect("capture mutex must not be poisoned")
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            // Best-effort settle if the test failed before `join`: the accept loop
            // observes the flag within one poll interval and exits.
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// A minimal valid gzip stream (header, one final stored deflate block, CRC32,
    /// ISIZE) of `payload`: a decompressing client would yield the plain payload, so
    /// byte-equality with the raw stream proves no automatic decompression ran.
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

        let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff];
        out.push(0x01); // final block, stored (uncompressed)
        let len = u16::try_from(payload.len()).expect("test payload fits a stored block");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(payload);
        out.extend_from_slice(&crc32(payload).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out
    }

    #[tokio::test(flavor = "current_thread")]
    async fn every_request_carries_the_fixed_product_user_agent() {
        let server = TestServer::spawn(vec![ScriptedResponse::plain(200, "ok")]);
        let client = build_client().expect("the shared client builds");
        let response = client
            .get(format!("http://{}/ua", server.addr()))
            .send()
            .await
            .expect("the request succeeds");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let captured = server.join();
        assert_eq!(captured.len(), 1, "exactly one request is sent");
        assert_eq!(
            captured[0].header("user-agent"),
            Some(USER_AGENT),
            "the fixed product UA is on the wire"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn redirects_are_returned_not_followed() {
        // A 302 with a relative Location is the single scripted response.  The
        // client must return the 3xx itself; a redirect-following client would open
        // a second request to the same listener, which panics the server thread
        // (no scripted response left) and fails the test via `join` — deterministic,
        // with no sleep-based "no second request" claim.
        let server = TestServer::spawn(vec![ScriptedResponse {
            status: 302,
            headers: vec![("Location".to_owned(), "/redirected".to_owned())],
            body: Vec::new(),
        }]);
        let client = build_client().expect("the shared client builds");
        let response = client
            .get(format!("http://{}/start", server.addr()))
            .send()
            .await
            .expect("the 3xx response is returned, not followed");
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        let captured = server.join();
        assert_eq!(
            captured.len(),
            1,
            "the 3xx was sent once and no redirect request ever landed"
        );
        assert_eq!(
            captured[0].path(),
            "/start",
            "only the original request reaches the wire"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gzip_bodies_stay_raw_never_decompressed() {
        let payload = b"raw payload";
        let gzip = gzip_stored(payload);
        let server = TestServer::spawn(vec![ScriptedResponse {
            status: 200,
            headers: vec![
                ("Content-Encoding".to_owned(), "gzip".to_owned()),
                ("Content-Type".to_owned(), "text/plain".to_owned()),
            ],
            body: gzip.clone(),
        }]);
        let client = build_client().expect("the shared client builds");
        let response = client
            .get(format!("http://{}/gzip", server.addr()))
            .send()
            .await
            .expect("the request succeeds");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-encoding")
                .and_then(|value| value.to_str().ok()),
            Some("gzip"),
            "no decompression ran, so the encoding header survives"
        );
        let body = response.bytes().await.expect("the body reads");
        assert_eq!(
            &body[..],
            &gzip[..],
            "the body bytes are exactly the raw gzip stream, never decompressed"
        );
        let captured = server.join();
        assert_eq!(captured.len(), 1, "exactly one request is sent");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn http_5xx_is_returned_and_never_retried() {
        // One scripted 500 is served; `retry::never()` means the transport hands it
        // back as the final response.  A retrying client would send a second
        // request, which panics the server thread and fails the test via `join`.
        let server = TestServer::spawn(vec![ScriptedResponse::plain(500, "boom")]);
        let client = build_client().expect("the shared client builds");
        let response = client
            .get(format!("http://{}/boom", server.addr()))
            .send()
            .await
            .expect("the 500 response is returned, not retried");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        );
        let captured = server.join();
        assert_eq!(
            captured.len(),
            1,
            "the 500 was received exactly once: no automatic retry"
        );
    }
}
