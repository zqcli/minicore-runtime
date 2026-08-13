//! The closed production `fetch_url` builtin (ADR 0147): one default-off, Runtime-owned
//! Tool that fetches bounded UTF-8 text with exactly one HTTP GET from one
//! host-authorized exact origin and returns the response body as a single text part.
//!
//! The slice delivers:
//!
//! - the public redacted host config type [`FetchUrlOrigin`] and its payload-free
//!   validation error [`FetchUrlOriginError`]: one exact `https` DNS-host origin plus
//!   1..=8 host-pinned `SocketAddr`s, canonicalized by the URL parser (scheme `https`,
//!   punycoded lowercased DNS hostname, effective port), with unspecified/multicast/
//!   IPv4-broadcast addresses (including IPv4-mapped edge cases) rejected and later
//!   duplicate addresses removed preserving first order;
//! - the crate-private immutable [`FetchUrlResources`] materialization: per canonical
//!   origin one independent locked-down client built from the shared
//!   `crate::http_transport` builder plus `https_only(true)`, `pool_max_idle_per_host(0)`,
//!   fixed 10s connect / 30s request timeouts, and a reject-all DNS resolver under the
//!   exact hostname's `resolve_to_addrs` override, so no ambient DNS ever runs
//!   (ADR 0147 decisions 5, 7, 9);
//! - the crate-private same-origin authorization seam
//!   [`FetchUrlResources::authorize`]: parses and validates the call URL (safe
//!   non-empty text, at most 4,096 bytes, absolute, no userinfo/fragment; path and
//!   query allowed), classifies malformed/oversize/userinfo/fragment URLs as
//!   `InvalidUrl`, compares the canonical scheme/host/effective port exactly against
//!   the installed origins, and returns the opaque, redacted, move-only
//!   [`AuthorizedFetchUrl`] binding the exact parsed `reqwest::Url` with the exact
//!   per-origin client.  Userinfo is rejected on the raw text *before* parsing too:
//!   the WHATWG parser erases an entirely-empty userinfo (`https://@host/` and
//!   `https://:@host/` normalize to no userinfo), so the post-parse
//!   username/password check alone cannot see those forms, and a private raw
//!   username/password check alone cannot see those forms, and a fail-closed private
//!   raw hierarchical-authority validator requires the exact `scheme://non-empty-
//!   authority` form without a literal `@` before parsing.  There is no public
//!   hostname/address/port accessor, no raw origin set exposure, and no generic
//!   registry; Debug/Display are fully redacted everywhere (ADR 0147 decisions 3,
//!   4, 12).
//!
//! The target's only consuming operation is its own exact send: the crate-private
//! async [`AuthorizedFetchUrl::send`] builds exactly one GET against the bound URL
//! through the bound per-origin client with the fixed ADR 0147 headers (`Accept:
//! text/plain, application/json`, `Accept-Encoding: identity`, `Connection: close`)
//! and an empty body, so the executor can await the response but can never alter
//! the scheme, host, port, path, query, or headers, and never touches a raw
//! `reqwest::Url` or `reqwest::Client` (ADR 0147 decision 8);
//! - the closed Tool surface: exact name `fetch_url`, `Parallel` mode, the verbatim
//!   ADR 0147 description, the closed one-required-string `url` schema, the standalone
//!   [`build_tool_set`] used by focused tests, and the synchronous planner that
//!   authorizes through [`FetchUrlResources::authorize`] (invalid arguments settle
//!   `PreExecution + Failed`, no exact origin settles `PreExecution + Denied`, success
//!   carries exactly `ToolCapabilityClass::Network` and a move-only start factory);
//! - the owner-tracked executor: the whole send + response inspect + bounded body
//!   drain runs as one operation future inside the started run, with a biased outer
//!   select cancellation vs operation (a pre-cancelled token proves zero GET, an
//!   in-flight cancellation drops the exact operation state and settles Cancelled,
//!   a natural result is never rewritten after mapping), a strict 2xx-only disclosure
//!   contract with a small private Content-Type/Content-Encoding validator, known
//!   oversize rejection before streaming, a 65,537-byte cancellation-aware stream
//!   read, and the fixed ADR 0147 decision-14 model-visible texts.
//!
//! The Runtime config wiring (default-off opt-in, origin installation, materialization)
//! lives in `runtime.rs`; this module keeps the Tool surface and its authority closed.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Name, Resolve, Resolving};
use thiserror::Error;

use crate::wire::BoundedJsonObject;
use crate::wire::lexical::validate_safe_text;

use super::{
    ToolAbandonReason, ToolCancellationObserver, ToolCapabilityClass, ToolDefinition,
    ToolExecutionMode, ToolExecutionPlan, ToolExecutionRequest, ToolExecutionResult,
    ToolExecutionStart, ToolPermissionSet, ToolResultContent, ToolResultDisposition, ToolSpec,
};
#[cfg(test)]
use super::{ToolSandboxContract, ToolSet, ToolSetInner};

/// Maximum origin input bytes (ADR 0147 decision 3).
const MAX_ORIGIN_TEXT_BYTES: usize = 2048;

/// Maximum call-URL input bytes (ADR 0147 decisions 1 and 4).
const MAX_URL_TEXT_BYTES: usize = 4096;

/// Payload-free classification of one raw hierarchical-authority violation.  The
/// rejected raw text is never stored, so Debug/Display can never leak it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RawHierarchicalAuthorityError {
    /// The raw text contains no scheme colon, so it is not an absolute URL at all.
    NoSchemeColon,
    /// The scheme colon is not followed by exactly two forward slashes (the
    /// one-slash, backslash, and no-slash parser recovery spellings).
    NotHierarchical,
    /// The `scheme://` form carries an empty raw authority (three-or-more-slash
    /// spellings and a bare `scheme://`).
    EmptyAuthority,
    /// The raw authority carries a literal `@` userinfo delimiter.
    UserinfoAt,
}

/// Fail-closed validation of the raw hierarchical authority of an absolute URL.
///
/// This is the semantic pre-parse gate of ADR 0147 decision 4: the raw text must
/// be the exact `scheme://non-empty-authority` hierarchical form before any
/// parsing, so the WHATWG parser's special-scheme recovery spellings can never
/// smuggle a userinfo delimiter past the post-parse username/password check.
/// Concretely, the validator requires:
///
/// - a scheme colon in the raw text;
/// - exactly two forward slashes immediately after that colon;
/// - a non-empty raw authority between the two slashes and the first `/`, `?`,
///   `#`, or `\` (the path/query/fragment authority terminator, with `\`
///   behaving as `/` for special schemes);
/// - no literal `@` in that raw authority.
///
/// A single slash, a backslash, or no slash after the scheme colon is a
/// parser-recovery spelling, not the hierarchical form: the WHATWG parser
/// accepts `https:/@example.com/`, `https:\@example.com/`, `https:@example.com/`,
/// `https::@example.com/`, and even `https:example.com`, and normalizes each to
/// `https://example.com/` with the empty userinfo erased.  Three-or-more-slash
/// spellings (`https:///@example.com/`, `https:////@example.com/`) fold their
/// extra slashes into an empty authority and likewise parse to the clean origin.
/// Requiring the one canonical form rejects every one of these spellings without
/// enumerating whichever recovery cases the pinned parser happens to accept.
///
/// Scheme case is left to the parser's canonicalization (`HTTPS://example.com` is
/// valid), `https://example.com` without a trailing slash is valid (the parser
/// supplies the `/` path), and a literal `@` after the authority terminator (in
/// path or query) or a percent-encoded `%40` is ordinary URL content, not
/// userinfo, so it never fails here.  Inputs reaching this validator are already
/// validated safe text, so no control stripping is needed.
fn validate_raw_hierarchical_authority(raw: &str) -> Result<(), RawHierarchicalAuthorityError> {
    let Some(scheme_colon) = raw.find(':') else {
        return Err(RawHierarchicalAuthorityError::NoSchemeColon);
    };
    let Some(authority) = raw[scheme_colon + 1..].strip_prefix("//") else {
        return Err(RawHierarchicalAuthorityError::NotHierarchical);
    };
    let authority_end = authority
        .find(['/', '?', '#', '\\'])
        .unwrap_or(authority.len());
    let authority = &authority[..authority_end];
    if authority.is_empty() {
        return Err(RawHierarchicalAuthorityError::EmptyAuthority);
    }
    if authority.contains('@') {
        return Err(RawHierarchicalAuthorityError::UserinfoAt);
    }
    Ok(())
}

/// Maximum raw socket addresses per origin (ADR 0147 decision 3).
const MAX_ORIGIN_ADDRESSES: usize = 8;

/// Fixed per-origin client connect timeout (ADR 0147 decision 9); not a public knob.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Fixed whole-request/body timeout (ADR 0147 decision 9); not a public knob.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Payload-free validation error for one exact [`FetchUrlOrigin`] construction.
///
/// The rejected origin text and address details are never stored, so Debug/Display can
/// never leak them.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FetchUrlOriginError {
    #[error("fetch_url origin must be non-empty safe UTF-8 text of at most 2,048 bytes")]
    InvalidOriginText,
    #[error("fetch_url origin must be an absolute https URL")]
    InvalidOriginUrl,
    #[error("fetch_url origin host must be a non-empty DNS hostname, not an IP address literal")]
    InvalidOriginHost,
    #[error("fetch_url origin must have a path of exactly / and no userinfo, query, or fragment")]
    InvalidOriginForm,
    #[error("fetch_url origin must provide 1..=8 socket addresses")]
    InvalidAddressCount,
    #[error("every fetch_url origin socket address port must equal the origin effective port")]
    InvalidAddressPort,
    #[error(
        "fetch_url origin socket addresses must not be unspecified, multicast, or IPv4 broadcast"
    )]
    InvalidAddressIp,
}

/// One validated, fully redacted exact HTTPS origin plus its host-pinned addresses.
///
/// The canonical identity is the URL-parser-normalized `https` scheme, punycoded
/// lowercased DNS hostname, and effective port (explicit and default `:443` are the
/// same origin).  The origin URL and the addresses are stored privately: there is no
/// hostname, port, or address accessor, and [`fmt::Debug`]/[`fmt::Display`] print a
/// fixed redacted marker only (ADR 0147 decision 3).
#[derive(Clone)]
pub struct FetchUrlOrigin {
    /// The validated canonical origin URL: `https`, DNS hostname, path exactly `/`,
    /// no userinfo/query/fragment.
    origin: reqwest::Url,
    /// The canonical effective port (explicit port or the https default 443).
    effective_port: u16,
    /// The host-pinned socket addresses, deduplicated preserving first order.
    addresses: Vec<SocketAddr>,
}

impl FetchUrlOrigin {
    /// Validates one exact origin plus its pinned addresses (pure constructor, no I/O).
    ///
    /// The origin must be non-empty safe UTF-8 text of at most 2,048 bytes, an absolute
    /// `https` URL with a DNS hostname (IP literals rejected), a path of exactly `/`,
    /// and no userinfo, query, or fragment.  Between 1 and 8 raw `SocketAddr`s are
    /// accepted; every port must equal the origin effective port; unspecified,
    /// multicast, and IPv4-broadcast addresses are rejected, including their
    /// IPv4-mapped IPv6 forms; later duplicates are removed preserving first order.
    pub fn new(origin: &str, addresses: &[SocketAddr]) -> Result<Self, FetchUrlOriginError> {
        validate_safe_text(origin, MAX_ORIGIN_TEXT_BYTES, false)
            .map_err(|_| FetchUrlOriginError::InvalidOriginText)?;
        // The raw hierarchical authority is validated before parsing (ADR 0147
        // decision 4): the raw text must be the exact `scheme://non-empty-authority`
        // form with no literal `@` userinfo delimiter.  The WHATWG parser erases an
        // entirely-empty userinfo (`https://@host/`, `https://:@host/`) and recovers
        // one-slash, backslash, no-slash, and three-or-more-slash spellings to a
        // clean `https://host/`, which would otherwise defeat the post-parse
        // username/password check.
        validate_raw_hierarchical_authority(origin)
            .map_err(|_| FetchUrlOriginError::InvalidOriginForm)?;
        let url = reqwest::Url::parse(origin).map_err(|_| FetchUrlOriginError::InvalidOriginUrl)?;
        if url.scheme() != "https" {
            return Err(FetchUrlOriginError::InvalidOriginUrl);
        }
        // The WHATWG parser itself decides domain-vs-IP first: every accepted IP
        // spelling (dotted quad, one/two/three-part numbers, octal and hex forms,
        // trailing-dot IPv4, bracketed IPv6) is normalized to a canonical Ipv4/Ipv6
        // serialization, so classifying that serialized host re-classifies the
        // parser's own decision exactly.  This is not a claim that an arbitrary
        // domain string can never parse as an IP form: whatever the parser accepted
        // as an IP literal is exactly what the canonical serialization recognizes
        // (ADR 0147 decision 3: DNS hostname only).
        let Some(hostname) = url.host_str() else {
            return Err(FetchUrlOriginError::InvalidOriginHost);
        };
        if hostname.is_empty() || is_ip_literal_host(hostname) {
            return Err(FetchUrlOriginError::InvalidOriginHost);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(FetchUrlOriginError::InvalidOriginForm);
        }
        if url.query().is_some() || url.fragment().is_some() || url.path() != "/" {
            return Err(FetchUrlOriginError::InvalidOriginForm);
        }
        // `https` always has a known default port, so the closed fallback is unreachable.
        let effective_port = url.port_or_known_default().unwrap_or(443);
        if addresses.is_empty() || addresses.len() > MAX_ORIGIN_ADDRESSES {
            return Err(FetchUrlOriginError::InvalidAddressCount);
        }
        let mut deduplicated = Vec::with_capacity(addresses.len());
        for &address in addresses {
            if address.port() != effective_port {
                return Err(FetchUrlOriginError::InvalidAddressPort);
            }
            if !address_allowed(address.ip()) {
                return Err(FetchUrlOriginError::InvalidAddressIp);
            }
            if !deduplicated.contains(&address) {
                deduplicated.push(address);
            }
        }
        // A non-empty raw list always leaves at least its first address after dedupe.
        if deduplicated.is_empty() {
            return Err(FetchUrlOriginError::InvalidAddressCount);
        }
        Ok(Self {
            origin: url,
            effective_port,
            addresses: deduplicated,
        })
    }

    /// The canonical (parser-normalized, punycoded, lowercased) DNS hostname.  The
    /// validated constructor guarantees a non-empty `Host::Domain`, so this never
    /// panics with secret payload: the message is a fixed invariant string.
    fn canonical_hostname(&self) -> &str {
        self.origin
            .host_str()
            .expect("a validated fetch_url origin always has a DNS hostname")
    }
}

/// Address admission (ADR 0147 decision 3): unspecified, multicast, and the IPv4
/// limited-broadcast address are rejected; loopback, private, link-local, and
/// documentation ranges are not guessed by the Runtime.
fn address_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_multicast() && v4 != Ipv4Addr::BROADCAST,
        IpAddr::V6(v6) => !v6.is_unspecified() && !v6.is_multicast() && mapped_v4_allowed(v6),
    }
}

/// IPv4-mapped IPv6 addresses inherit the IPv4 rules: a mapped unspecified, multicast,
/// or broadcast IPv4 address is still rejected (ADR 0147 decision 3's IPv4-mapped edge
/// cases); any other IPv6 address passes.
fn mapped_v4_allowed(v6: Ipv6Addr) -> bool {
    match v6.to_ipv4_mapped() {
        Some(v4) => !v4.is_unspecified() && !v4.is_multicast() && v4 != Ipv4Addr::BROADCAST,
        None => true,
    }
}

/// Whether a serialized URL host is an IP literal: the WHATWG parser stores every
/// IP-form host as Ipv4/Ipv6, serializing IPv4 bare and IPv6 bracketed, so a domain
/// name never matches either parse.
fn is_ip_literal_host(hostname: &str) -> bool {
    if let Some(inner) = hostname.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return inner.parse::<Ipv6Addr>().is_ok();
    }
    hostname.parse::<Ipv4Addr>().is_ok()
}

impl fmt::Debug for FetchUrlOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Fully redacted: the origin, hostname, port, and addresses never print.
        formatter.write_str("FetchUrlOrigin(<redacted>)")
    }
}

impl fmt::Display for FetchUrlOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Fully redacted: the origin, hostname, port, and addresses never print.
        formatter.write_str("FetchUrlOrigin(<redacted>)")
    }
}

/// Payload-free, closed materialization error for one immutable [`FetchUrlResources`]
/// construction.  Never carries any origin, hostname, address, or endpoint detail.
/// The production private taxonomy is exactly [`FetchUrlResourcesError::DuplicateOrigin`]
/// and [`FetchUrlResourcesError::ClientBuild`]; the loopback seam's
/// `InvalidTestAuthority` rejection exists only under `#[cfg(test)]` and is absent
/// from production builds.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum FetchUrlResourcesError {
    #[error("two fetch_url origins normalized to the same canonical origin")]
    DuplicateOrigin,
    #[error("a fetch_url pinned HTTP client could not be constructed")]
    ClientBuild,
    /// Test-only loopback contract rejection, present only in `#[cfg(test)]`
    /// builds: the loopback seam returns this payload-free error for a host that
    /// is not a bare canonical DNS hostname or an address that is not
    /// loopback/port-nonzero.  It is deliberately not part of the production
    /// private taxonomy — production code can never construct it — and it never
    /// carries any hostname, address, or port detail.
    #[cfg(test)]
    #[error("a fetch_url test loopback authority violates the test-only contract")]
    InvalidTestAuthority,
}

/// One pinned per-origin client entry: the canonical scheme/host/effective port and the
/// exact locked-down client whose only resolution is that host's pinned addresses.
struct PinnedFetchUrlOrigin {
    scheme: &'static str,
    host: String,
    port: u16,
    client: reqwest::Client,
}

/// The immutable, redacted per-origin client set of one Runtime (ADR 0147 decision 15).
///
/// Every origin gets its own independent locked-down client so future connection reuse
/// can never carry origin A's address authority into origin B (ADR 0147 decision 7).
/// An empty set is representable (the origin-only-undisclosed config slice); duplicate
/// canonical origins fail closed at materialization.
pub(crate) struct FetchUrlResources {
    origins: Vec<PinnedFetchUrlOrigin>,
}

impl FetchUrlResources {
    /// Materializes one immutable client per canonical origin.  Duplicate canonical
    /// origins (same normalized host and effective port) fail closed; an empty input
    /// yields an empty authority.  Each client is built from the shared locked-down
    /// `crate::http_transport` builder with `https_only(true)`,
    /// `pool_max_idle_per_host(0)`, fixed 10s connect / 30s request timeouts, and a
    /// reject-all DNS resolver under the exact hostname's `resolve_to_addrs` override:
    /// only the host-pinned addresses can ever be connected, and no ambient DNS runs.
    pub(crate) fn materialize(
        origins: impl IntoIterator<Item = FetchUrlOrigin>,
    ) -> Result<Self, FetchUrlResourcesError> {
        let mut installed = std::collections::HashSet::new();
        let mut pinned = Vec::new();
        for origin in origins {
            let host = origin.canonical_hostname().to_owned();
            let port = origin.effective_port;
            if !installed.insert((host.clone(), port)) {
                return Err(FetchUrlResourcesError::DuplicateOrigin);
            }
            let client = pinned_client(&host, &origin.addresses, true)
                .map_err(|_| FetchUrlResourcesError::ClientBuild)?;
            pinned.push(PinnedFetchUrlOrigin {
                scheme: "https",
                host,
                port,
                client,
            });
        }
        Ok(Self { origins: pinned })
    }

    /// The same-origin authorization seam (ADR 0147 decisions 4 and 12).
    ///
    /// Parses and validates the call URL synchronously: the input must be non-empty
    /// safe UTF-8 text of at most 4,096 bytes, an absolute URL with no userinfo and no
    /// fragment (path and query allowed).  Malformed, oversize, unsafe-text,
    /// userinfo, or fragment URLs classify as [`FetchUrlAuthorizationError::InvalidUrl`].
    /// A well-formed URL whose canonical scheme/host/effective port does not exactly
    /// match an installed origin — foreign scheme, host, subdomain, or port, or an IP
    /// literal — classifies as [`FetchUrlAuthorizationError::Denied`].  Success returns
    /// the opaque, redacted, move-only [`AuthorizedFetchUrl`] binding the exact parsed
    /// URL with the exact per-origin client; the executor never re-parses the string
    /// and never sees the origin set, and the target's only consuming operation is its
    /// own fixed exact `send` (ADR 0147 decision 8).
    pub(crate) fn authorize(
        &self,
        url: &str,
    ) -> Result<AuthorizedFetchUrl, FetchUrlAuthorizationError> {
        validate_safe_text(url, MAX_URL_TEXT_BYTES, false)
            .map_err(|_| FetchUrlAuthorizationError::InvalidUrl)?;
        // The raw hierarchical authority is validated before parsing (ADR 0147
        // decision 4): the raw text must be the exact `scheme://non-empty-authority`
        // form with no literal `@` userinfo delimiter.  The WHATWG parser erases an
        // entirely-empty userinfo (`https://@host/`, `https://:@host/`) and recovers
        // one-slash, backslash, no-slash, and three-or-more-slash spellings to a
        // clean origin, which would otherwise defeat the post-parse
        // username/password check.
        validate_raw_hierarchical_authority(url)
            .map_err(|_| FetchUrlAuthorizationError::InvalidUrl)?;
        let url = reqwest::Url::parse(url).map_err(|_| FetchUrlAuthorizationError::InvalidUrl)?;
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err(FetchUrlAuthorizationError::InvalidUrl);
        }
        // Foreign scheme or an IP-literal host: well-formed, but no exact origin
        // authority can ever match it (the same WHATWG serialized-host classification
        // as the constructor).
        let scheme = url.scheme();
        if scheme != "https" && scheme != "http" {
            return Err(FetchUrlAuthorizationError::Denied);
        }
        let Some(hostname) = url.host_str() else {
            return Err(FetchUrlAuthorizationError::InvalidUrl);
        };
        if is_ip_literal_host(hostname) {
            return Err(FetchUrlAuthorizationError::Denied);
        }
        // Both matched schemes have known default ports; the fallback is closed.
        let port = url
            .port_or_known_default()
            .unwrap_or(if scheme == "https" { 443 } else { 80 });
        let client = self
            .origins
            .iter()
            .find(|origin| {
                origin.scheme == scheme && origin.host == hostname && origin.port == port
            })
            .map(|origin| origin.client.clone())
            .ok_or(FetchUrlAuthorizationError::Denied)?;
        Ok(AuthorizedFetchUrl { url, client })
    }

    /// Deterministic test-only loopback authority (ADR 0147 decision 16): one DNS test
    /// hostname pinned to one numeric loopback `SocketAddr` over plain HTTP, keeping
    /// every locked-down property — no redirect/retry/proxy/compression, zero idle
    /// pool, fixed timeouts, reject-all resolver — so wire and cancellation tests run
    /// against a local deterministic server with no ambient DNS and no TLS.  The
    /// test-only contract is enforced, not just documented: the host must parse as a
    /// bare non-empty canonical DNS hostname (never an IP literal in any WHATWG
    /// spelling; no userinfo/password, port, path beyond `/`, query, or fragment,
    /// while case/IDNA canonicalization stays allowed), and the address must be a
    /// numeric loopback IP with a nonzero port; any violation returns the payload-free
    /// [`FetchUrlResourcesError::InvalidTestAuthority`].  This is `#[cfg(test)]` only:
    /// production code, the public interface, and the Runtime config can never call
    /// it, and it is not an HTTP fallback.  `pub(crate)` so the Runtime's test-only
    /// config injection seam and the Tools composition tests can capture the same
    /// immutable authority.
    #[cfg(test)]
    pub(crate) fn loopback(
        host: &str,
        address: SocketAddr,
    ) -> Result<Self, FetchUrlResourcesError> {
        if address.port() == 0 || !address.ip().is_loopback() {
            return Err(FetchUrlResourcesError::InvalidTestAuthority);
        }
        // The host contract is verified against the real WHATWG parser: the host is
        // embedded in a test URL and must parse to a non-empty canonical DNS hostname
        // (lowercased/punycoded), not an IP literal in any spelling (the parser
        // normalizes every accepted IPv4/IPv6 form, and the canonical serialization
        // is what gets pinned).  The input must be a bare hostname: the parsed test
        // URL must carry no username and no password, no explicit port, a path of
        // exactly `/`, and no query or fragment — anything else means the host string
        // was not a bare host form and is rejected.  Case and IDNA host forms remain
        // allowed: the parser-canonicalized hostname is exactly what gets pinned.  A
        // host that fails to parse or classifies as an IP literal is not a valid DNS
        // test hostname and is rejected payload-free.
        let test_url_raw = format!("http://{host}/");
        // The raw hierarchical authority is validated before parsing (ADR 0147
        // decision 4): the constructed test URL must be the exact
        // `scheme://non-empty-authority` form with no literal `@` userinfo
        // delimiter.  The WHATWG parser erases an entirely-empty userinfo
        // (`@host` and `:@host` embed as `http://@host/`) and recovers
        // backslash- and extra-slash spellings to a clean bare host, which would
        // otherwise defeat the post-parse username/password check on the
        // constructed test URL.
        validate_raw_hierarchical_authority(&test_url_raw)
            .map_err(|_| FetchUrlResourcesError::InvalidTestAuthority)?;
        let test_url = reqwest::Url::parse(&test_url_raw)
            .map_err(|_| FetchUrlResourcesError::InvalidTestAuthority)?;
        if !test_url.username().is_empty()
            || test_url.password().is_some()
            || test_url.port().is_some()
            || test_url.path() != "/"
            || test_url.query().is_some()
            || test_url.fragment().is_some()
        {
            return Err(FetchUrlResourcesError::InvalidTestAuthority);
        }
        let canonical = test_url
            .host_str()
            .ok_or(FetchUrlResourcesError::InvalidTestAuthority)?;
        if canonical.is_empty() || is_ip_literal_host(canonical) {
            return Err(FetchUrlResourcesError::InvalidTestAuthority);
        }
        let host = canonical.to_owned();
        let client = pinned_client(&host, &[address], false)
            .map_err(|_| FetchUrlResourcesError::ClientBuild)?;
        Ok(Self {
            origins: vec![PinnedFetchUrlOrigin {
                scheme: "http",
                host,
                port: address.port(),
                client,
            }],
        })
    }
}

/// Builds one pinned per-origin client from the shared locked-down transport builder
/// (ADR 0147 decisions 5, 7, 9): https-only enforcement (relaxed only by the test-only
/// loopback seam), zero idle pool, fixed connect/request timeouts, and a reject-all
/// DNS resolver under the exact hostname's static `resolve_to_addrs` override.  The
/// override short-circuits before the resolver, so the pinned hostname resolves only
/// to the given addresses while every unmatched name fails before any socket opens.
fn pinned_client(
    host: &str,
    addresses: &[SocketAddr],
    https_only: bool,
) -> Result<reqwest::Client, reqwest::Error> {
    crate::http_transport::client_builder()
        .https_only(https_only)
        .pool_max_idle_per_host(0)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .dns_resolver(RejectAllResolver)
        .resolve_to_addrs(host, addresses)
        .build()
}

/// The DNS resolver that rejects every name (ADR 0147 decision 5): it sits under the
/// exact hostname overrides, so any name without an override fails before any socket
/// is opened and never falls back to ambient system DNS.
#[derive(Debug)]
struct RejectAllResolver;

impl Resolve for RejectAllResolver {
    fn resolve(&self, _name: Name) -> Resolving {
        Box::pin(async {
            let error = Box::new(RejectAllDnsError) as Box<dyn std::error::Error + Send + Sync>;
            Err(error)
        })
    }
}

/// Payload-free rejection marker; the error never carries the rejected name.
#[derive(Debug)]
struct RejectAllDnsError;

impl fmt::Display for RejectAllDnsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DNS resolution is rejected for this client")
    }
}

impl std::error::Error for RejectAllDnsError {}

/// Payload-free classification of one same-origin authorization attempt.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum FetchUrlAuthorizationError {
    #[error("the fetch_url URL argument is invalid")]
    InvalidUrl,
    #[error("the fetch_url URL has no exact installed origin authority")]
    Denied,
}

/// The opaque, redacted, move-only request target of one successful authorization
/// (ADR 0147 decisions 4 and 8): binds the exact parsed `reqwest::Url` with the exact
/// per-origin `reqwest::Client`.  Only the future executor consumes it, through the
/// crate-private consuming async [`AuthorizedFetchUrl::send`] — the target owns the
/// exact send, so the executor only awaits the response and can never alter the
/// scheme, host, port, path, query, or headers, and never sees a raw `reqwest::Url`
/// or `reqwest::Client`.  It is deliberately not Clone, carries no origin identity,
/// and never prints anything secret.
pub(crate) struct AuthorizedFetchUrl {
    url: reqwest::Url,
    client: reqwest::Client,
}

impl AuthorizedFetchUrl {
    /// The target's only consuming operation: builds exactly one GET against the
    /// bound URL through the bound per-origin locked-down client and sends it once
    /// (ADR 0147 decision 8).  The request adds exactly the fixed headers `Accept:
    /// text/plain, application/json`, `Accept-Encoding: identity`, and `Connection:
    /// close`, with an empty body; no other header is added by this target, and the
    /// caller cannot alter the scheme, host, port, path, query, or headers.  The
    /// client already carries the shared locked-down transport policy (no
    /// redirect/retry/proxy/compression, fixed product user-agent) plus the pinned
    /// per-origin DNS override and timeouts, so those properties continue to apply
    /// to this exact send.  The executor awaits the response (and then the body) but
    /// never re-parses the URL string and never touches the raw `reqwest::Url` or
    /// `reqwest::Client`.
    pub(crate) async fn send(self) -> Result<reqwest::Response, reqwest::Error> {
        self.client
            .get(self.url)
            .header(reqwest::header::ACCEPT, "text/plain, application/json")
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .header(reqwest::header::CONNECTION, "close")
            .send()
            .await
    }
}

/// The exact production builtin ToolName.  `pub(super)` because the composed production
/// ToolSet routes exactly this frozen name.
pub(super) const FETCH_URL_NAME: &str = "fetch_url";

/// The exact production description disclosed for the builtin (ADR 0147 decision 1,
/// verbatim).  Frozen; asserted verbatim in module tests.
const FETCH_URL_DESCRIPTION: &str = "Fetch bounded UTF-8 text with HTTP GET from one host-authorized HTTPS origin and return the response body as a single text part.";

/// The exact frozen PreExecution Failed text for every parse or semantic URL argument
/// failure (ADR 0147 decision 14).
const INVALID_ARGUMENTS_TEXT: &str = "tool arguments are invalid";

/// The exact frozen PreExecution Denied text for a well-formed URL without an exact
/// installed origin authority (ADR 0147 decision 14).
const NETWORK_DENIED_TEXT: &str = "network URL access is denied";

/// The exact frozen Completed Cancelled text for a cancellation that wins the started
/// executor's biased select before the operation completes (ADR 0147 decision 14).
const FETCH_CANCELLED_TEXT: &str = "URL fetch was cancelled";

/// The exact frozen Completed Failed text for connect/send/timeout/body-stream failures
/// and every non-2xx status (ADR 0147 decision 14).
const FETCH_FAILED_TEXT: &str = "URL could not be fetched";

/// The exact frozen Completed Failed text for a missing/duplicate/malformed/unsupported
/// Content-Type or a non-identity Content-Encoding (ADR 0147 decision 14).
const UNSUPPORTED_RESPONSE_TEXT: &str = "URL response type is unsupported";

/// The exact frozen Completed Failed text for a body beyond 65,536 bytes (ADR 0147
/// decision 14).
const TOO_LARGE_TEXT: &str = "URL response is too large";

/// The exact frozen Completed Failed text for a body that is not valid UTF-8 or not
/// safe Text (ADR 0147 decision 14).
const NOT_VALID_TEXT: &str = "URL response is not valid text";

/// The closed response body bound: exactly one Text part of at most 65,536 bytes
/// (ADR 0147 decision 11).
const MAX_RESPONSE_BYTES: usize = 65_536;

/// One byte beyond the body bound: reading at most 65,537 bytes detects oversize without
/// unbounded allocation (ADR 0147 decision 11).
const MAX_READ_BYTES: usize = MAX_RESPONSE_BYTES + 1;

/// The closed input schema disclosed for the builtin (ADR 0147 decision 1): one required
/// string `url` capped at 4,096 bytes, `additionalProperties: false`.  Structural
/// guidance only: the semantic URL gate (safe non-empty text, at most 4,096 bytes,
/// absolute, no userinfo/fragment) is enforced by [`FetchUrlResources::authorize`],
/// never by this schema.
const FETCH_URL_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "url": {
      "type": "string",
      "maxLength": 4096
    }
  },
  "required": ["url"],
  "additionalProperties": false
}"#;

/// The exact frozen production definition/spec pair: the single source shared by the
/// focused-test standalone ToolSet and the composed production ToolSet, so the disclosed
/// definition and spec are byte-identical in both selections.
pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        spec: ToolSpec {
            name: FETCH_URL_NAME
                .parse()
                .expect("the frozen fetch_url ToolName is valid"),
            description: Arc::from(FETCH_URL_DESCRIPTION),
            input_schema: FETCH_URL_SCHEMA
                .parse()
                .expect("the frozen fetch_url schema is valid"),
        },
        // One bounded GET per call; the definition does not impose Serial execution
        // semantics on unrelated operations in the composed production ToolSet.
        mode: ToolExecutionMode::Parallel,
    }
}

/// The focused-test sandbox contract of the fetch_url builtin: available exactly for
/// `Network`, matching the composed production ToolSet's admission ceiling.
#[cfg(test)]
pub(super) fn sandbox() -> ToolSandboxContract {
    ToolSandboxContract::available([ToolCapabilityClass::Network])
}

/// Builds the focused-test standalone `fetch_url` ToolSet from the same definition and
/// planner the closed production composer uses.
#[cfg(test)]
pub(super) fn build_tool_set(resources: Arc<FetchUrlResources>) -> Arc<ToolSet> {
    let definition = definition();
    let specs: Arc<[ToolSpec]> = Arc::from([definition.spec.clone()]);
    let definitions: Arc<[ToolDefinition]> = Arc::from([definition]);
    let planner: Arc<super::ToolPlanner> = Arc::new(move |request| plan(&resources, request));
    Arc::new(ToolSet {
        inner: Arc::new(ToolSetInner {
            definitions,
            specs,
            planner: Some(planner),
            sandbox: sandbox(),
        }),
    })
}

/// The synchronous pre-start plan for one exact `fetch_url` call (ADR 0147 decisions 1,
/// 4, 12, 14): the planner only authorizes and constructs the move-only start factory —
/// it never constructs or sends any request, and the target's exact send stays owned by
/// the executor.  Every parse or semantic URL failure (malformed, oversize,
/// unsafe-text, empty, userinfo, fragment) settles the frozen `PreExecution + Failed`
/// text `tool arguments are invalid`; a well-formed URL with no exact installed origin
/// settles the frozen `PreExecution + Denied` text `network URL access is denied`; a
/// valid, authorized call plans the Execute shape carrying exactly
/// `ToolCapabilityClass::Network` and a move-only start factory consuming the exact
/// [`AuthorizedFetchUrl`].  `pub(super)` because the composed production ToolSet routes
/// exactly this frozen planner.
pub(super) fn plan(
    resources: &FetchUrlResources,
    request: ToolExecutionRequest,
) -> ToolExecutionPlan {
    let arguments = match parse_arguments(request.call().arguments()) {
        Ok(arguments) => arguments,
        Err(()) => return invalid_arguments_plan(),
    };
    // The semantic URL gate (safe non-empty text, at most 4,096 bytes, absolute, no
    // userinfo/fragment) and the same-origin authority comparison both live in the one
    // authorization seam; the executor never re-parses the string and never sees the
    // origin set.
    let target = match resources.authorize(&arguments.url) {
        Ok(target) => target,
        Err(FetchUrlAuthorizationError::InvalidUrl) => return invalid_arguments_plan(),
        Err(FetchUrlAuthorizationError::Denied) => return denied_plan(),
    };
    ToolExecutionPlan::Execute {
        permissions: ToolPermissionSet::new([ToolCapabilityClass::Network]),
        start: ToolExecutionStart::new(move |observer| Box::pin(execute_fetch(target, observer))),
    }
}

/// The strict private serde mirror of the closed arguments object: unknown fields are
/// rejected, and the semantic `FetchUrlResources::authorize` gate stays the URL
/// authority (the schema maxLength is guidance only).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchUrlArguments {
    url: String,
}

fn parse_arguments(arguments: &BoundedJsonObject) -> Result<FetchUrlArguments, ()> {
    serde_json::from_str(arguments.canonical_json()).map_err(|_| ())
}

fn invalid_arguments_plan() -> ToolExecutionPlan {
    ToolExecutionPlan::PreExecution(pre_execution_failed(INVALID_ARGUMENTS_TEXT))
}

fn denied_plan() -> ToolExecutionPlan {
    ToolExecutionPlan::PreExecution(ToolExecutionResult::PreExecution {
        disposition: ToolResultDisposition::Denied,
        content: ToolResultContent::from_text_parts(vec![NETWORK_DENIED_TEXT.to_owned()])
            .expect("the frozen denied text is a valid bounded part"),
    })
}

fn pre_execution_failed(text: &str) -> ToolExecutionResult {
    ToolExecutionResult::PreExecution {
        disposition: ToolResultDisposition::Failed,
        content: ToolResultContent::from_text_parts(vec![text.to_owned()])
            .expect("the frozen failure texts are valid bounded parts"),
    }
}

/// The bounded outcome of one fetch operation after the operation future settled; the
/// cancellation outcome is owned by the executor's outer select, never this enum.
#[derive(Clone)]
enum FetchOperationOutcome {
    /// A complete 2xx body: valid UTF-8 and safe Text, at most 65,536 bytes.
    Content(Arc<str>),
    /// Connect/send/timeout/body-stream failure or any non-2xx status.
    CouldNotFetch,
    /// Missing/duplicate/malformed/unsupported Content-Type or non-identity
    /// Content-Encoding.
    UnsupportedResponse,
    /// Body beyond 65,536 bytes (known Content-Length or streamed).
    TooLarge,
    /// Body that is not valid UTF-8 or not safe Text.
    NotValidText,
}

/// The owner-tracked executor for one started fetch (ADR 0147 decisions 9, 11, 13).
///
/// The whole operation — exact send, response inspect, bounded body drain — runs as one
/// operation future polled inside this executor's future; nothing is spawned and no
/// blocking job exists.  The biased outer select checks cancellation first: a token
/// already cancelled before the executor is polled wins without ever polling the
/// operation, so zero GET is provable; a cancellation arriving while the send waits for
/// headers, while the response is inspected, or while the body streams drops the exact
/// in-flight operation state (the send future or the response stream, together with its
/// operation-local request state) in the same owner future and settles the frozen
/// `Completed + Cancelled` text.  A simultaneously-ready cancellation deterministically
/// wins the biased tie (cancellation is never silently ignored), while a natural result
/// is mapped after the select and is never rewritten by a later cancellation.
///
/// Transport errors, the fixed 10s connect / 30s request timeouts, body-stream errors,
/// and every non-2xx status settle the frozen `Completed + Failed` text `URL could not
/// be fetched`; only an owner invariant (a validated success the owner's own result
/// contract refuses) settles `Abandoned { RuntimeFailure }`.
async fn execute_fetch(
    target: AuthorizedFetchUrl,
    observer: ToolCancellationObserver,
) -> ToolExecutionResult {
    let outcome = tokio::select! {
        biased;
        _ = observer.cancelled() => {
            return ToolExecutionResult::Completed {
                disposition: ToolResultDisposition::Cancelled,
                content: ToolResultContent::from_text_parts(vec![FETCH_CANCELLED_TEXT.to_owned()])
                    .expect("the frozen cancelled text is a valid bounded part"),
            };
        }
        outcome = fetch_operation(target) => outcome,
    };
    bind_fetch_outcome(outcome)
}

/// The one operation future: exactly one `AuthorizedFetchUrl::send`, then the strict
/// response policy, then the bounded body drain.  Constructing this future performs no
/// I/O; the exact GET happens only when the future is polled.
async fn fetch_operation(target: AuthorizedFetchUrl) -> FetchOperationOutcome {
    let response = match target.send().await {
        Ok(response) => response,
        Err(_) => return FetchOperationOutcome::CouldNotFetch,
    };
    fetch_response_outcome(response).await
}

/// The response policy (ADR 0147 decisions 10 and 11): only a 2xx status reads or
/// discloses anything; every non-2xx status drops the response without reading its body,
/// status, or headers and settles the frozen could-not-fetch text.  A 2xx response must
/// carry exactly one parseable Content-Type field with a case-insensitive base media
/// type of exactly `text/plain` or `application/json` (parameters never transcode; the
/// bytes must still be UTF-8), and Content-Encoding must be absent or exactly one
/// trim/case-insensitive `identity` field.  A known Content-Length above 65,536 bytes is
/// rejected before any streaming; otherwise the body streams at most 65,537
/// cancellation-aware bytes, stopping at the bound, and a stream error settles
/// could-not-fetch.
async fn fetch_response_outcome(mut response: reqwest::Response) -> FetchOperationOutcome {
    if !response.status().is_success() {
        // Non-2xx: the response is dropped unread — no body, status, or header is ever
        // disclosed.  `Connection: close` plus the zero idle pool keep the connection
        // from becoming a later-call resource.
        return FetchOperationOutcome::CouldNotFetch;
    }
    if validate_content_type(&response).is_none() {
        return FetchOperationOutcome::UnsupportedResponse;
    }
    if !validate_content_encoding(&response) {
        return FetchOperationOutcome::UnsupportedResponse;
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        // Known oversize: rejected before any streaming.
        return FetchOperationOutcome::TooLarge;
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(MAX_READ_BYTES);
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => return FetchOperationOutcome::CouldNotFetch,
        };
        let remaining = MAX_READ_BYTES - bytes.len();
        let take = remaining.min(chunk.len());
        bytes.extend_from_slice(&chunk[..take]);
        if bytes.len() > MAX_RESPONSE_BYTES {
            return FetchOperationOutcome::TooLarge;
        }
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return FetchOperationOutcome::NotValidText,
    };
    if validate_safe_text(&text, MAX_RESPONSE_BYTES, true).is_err() {
        return FetchOperationOutcome::NotValidText;
    }
    FetchOperationOutcome::Content(text.into())
}

fn bind_fetch_outcome(outcome: FetchOperationOutcome) -> ToolExecutionResult {
    match outcome {
        FetchOperationOutcome::Content(text) => {
            // The content already passed the owner's safe-Text and byte gates above, so
            // a failure here is an owner invariant that fails closed.
            match ToolResultContent::from_text_parts(vec![text.as_ref().to_owned()]) {
                Ok(content) => ToolExecutionResult::Completed {
                    disposition: ToolResultDisposition::Succeeded,
                    content,
                },
                Err(_) => ToolExecutionResult::Abandoned {
                    reason: ToolAbandonReason::RuntimeFailure,
                },
            }
        }
        FetchOperationOutcome::CouldNotFetch => completed_failed(FETCH_FAILED_TEXT),
        FetchOperationOutcome::UnsupportedResponse => completed_failed(UNSUPPORTED_RESPONSE_TEXT),
        FetchOperationOutcome::TooLarge => completed_failed(TOO_LARGE_TEXT),
        FetchOperationOutcome::NotValidText => completed_failed(NOT_VALID_TEXT),
    }
}

fn completed_failed(text: &str) -> ToolExecutionResult {
    ToolExecutionResult::Completed {
        disposition: ToolResultDisposition::Failed,
        content: ToolResultContent::from_text_parts(vec![text.to_owned()])
            .expect("the frozen failure texts are valid bounded parts"),
    }
}

/// One accepted Content-Type base media type (ADR 0147 decision 10): exactly
/// `text/plain` or `application/json`, case-insensitive.  Parameters never transcode;
/// the body bytes must still be valid UTF-8, so the kind is informational only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContentTypeKind {
    TextPlain,
    ApplicationJson,
}

/// Validates the response's Content-Type contract (ADR 0147 decision 10): exactly one
/// field whose value is valid visible-ASCII header text and parses as
/// `type "/" subtype` with a case-insensitive base of exactly `text/plain` or
/// `application/json`, optionally followed by `;`-separated parameters, each a nonempty
/// `token=token-or-quoted-string` (space/tab OWS allowed around each segment).  A
/// quoted-string value supports visible ASCII plus space/tab with backslash escapes and
/// rejects controls, an unclosed quote, and trailing junk after the closing quote.
fn validate_content_type(response: &reqwest::Response) -> Option<ContentTypeKind> {
    let mut fields = response
        .headers()
        .get_all(reqwest::header::CONTENT_TYPE)
        .iter();
    let field = match (fields.next(), fields.next()) {
        (Some(field), None) => field,
        // Missing (zero fields) and duplicate fields are both unsupported.
        _ => return None,
    };
    let value = field.to_str().ok()?;
    let (media_type, parameters) = match value.find(';') {
        Some(index) => (&value[..index], &value[index + 1..]),
        None => (value, ""),
    };
    // The base media type is exactly `token "/" token` (no OWS): a token cannot
    // contain space or tab, so any whitespace in the base part is rejected.
    let (type_name, subtype) = media_type.split_once('/')?;
    if !is_token(type_name) || !is_token(subtype) {
        return None;
    }
    let kind = if type_name.eq_ignore_ascii_case("text") && subtype.eq_ignore_ascii_case("plain") {
        ContentTypeKind::TextPlain
    } else if type_name.eq_ignore_ascii_case("application") && subtype.eq_ignore_ascii_case("json")
    {
        ContentTypeKind::ApplicationJson
    } else {
        return None;
    };
    // A present `;` starts the parameter list, and every segment of it (including an
    // empty trailing segment) must validate: `text/plain;` is malformed.
    if value.contains(';') && !validate_content_type_parameters(parameters) {
        return None;
    }
    Some(kind)
}

/// Scans the `;`-separated parameter list quote-aware (a quoted-string value may
/// contain `;`) and validates every segment as a nonempty `token=token-or-quoted-
/// string` with space/tab OWS trimmed around it.
fn validate_content_type_parameters(parameters: &str) -> bool {
    let bytes = parameters.as_bytes();
    let mut start = 0;
    let mut index = 0;
    while index <= bytes.len() {
        if index == bytes.len() || bytes[index] == b';' {
            let segment = parameters[start..index].trim_matches([' ', '\t']);
            if !validate_parameter_segment(segment) {
                return false;
            }
            if index == bytes.len() {
                return true;
            }
            start = index + 1;
            index += 1;
        } else if bytes[index] == b'"' {
            // Skip the quoted string (honoring backslash escapes) so a `;` inside it is
            // not a segment boundary; the segment itself is validated wholesale below.
            index += 1;
            loop {
                if index >= bytes.len() {
                    return false;
                }
                match bytes[index] {
                    b'\\' => {
                        index += 2;
                        if index > bytes.len() {
                            return false;
                        }
                    }
                    b'"' => {
                        index += 1;
                        break;
                    }
                    _ => index += 1,
                }
            }
        } else {
            index += 1;
        }
    }
    false
}

/// One nonempty `token=token-or-quoted-string` parameter segment (after OWS trim).
fn validate_parameter_segment(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    let Some((name, value)) = segment.split_once('=') else {
        return false;
    };
    is_token(name) && is_parameter_value(value)
}

fn is_parameter_value(value: &str) -> bool {
    is_token(value) || is_quoted_string(value)
}

/// Whether `value` is a full quoted-string: opening `"`, then visible ASCII plus
/// space/tab (never controls, `"`, or raw `\`) with `\`-escaped pairs (backslash plus
/// visible ASCII or space/tab), then a closing `"` as the very last character — an
/// unclosed quote and trailing junk after the closing quote are rejected.
fn is_quoted_string(value: &str) -> bool {
    let Some(rest) = value.strip_prefix('"') else {
        return false;
    };
    let bytes = rest.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => return index == bytes.len() - 1,
            b'\\' => {
                let Some(&escaped) = bytes.get(index + 1) else {
                    return false;
                };
                if !(escaped == b'\t' || escaped == b' ' || (0x21..=0x7e).contains(&escaped)) {
                    return false;
                }
                index += 2;
            }
            b'\t' | b' ' | 0x21 | 0x23..=0x5b | 0x5d..=0x7e => index += 1,
            _ => return false,
        }
    }
    // The closing quote never arrived.
    false
}

/// The RFC 7230 token character set; a token is non-empty and all `tchar`.
fn is_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(is_tchar)
}

fn is_tchar(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
            | b'0'..=b'9'
            | b'a'..=b'z'
            | b'A'..=b'Z'
    )
}

/// Validates the response's Content-Encoding contract (ADR 0147 decision 10): zero
/// fields are allowed; exactly one field is allowed only when its value trimmed and
/// case-insensitively equals `identity`; duplicates, a comma list, an empty value, and
/// anything else are unsupported (the client never decompresses).
fn validate_content_encoding(response: &reqwest::Response) -> bool {
    let mut fields = response
        .headers()
        .get_all(reqwest::header::CONTENT_ENCODING)
        .iter();
    match (fields.next(), fields.next()) {
        (None, _) => true,
        (Some(field), None) => field
            .to_str()
            .is_ok_and(|value| value.trim().eq_ignore_ascii_case("identity")),
        (Some(_), Some(_)) => false,
    }
}

impl fmt::Debug for FetchOperationOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Content(_) => formatter.write_str("FetchOperationOutcome::Content(..)"),
            Self::CouldNotFetch => formatter.write_str("FetchOperationOutcome::CouldNotFetch"),
            Self::UnsupportedResponse => {
                formatter.write_str("FetchOperationOutcome::UnsupportedResponse")
            }
            Self::TooLarge => formatter.write_str("FetchOperationOutcome::TooLarge"),
            Self::NotValidText => formatter.write_str("FetchOperationOutcome::NotValidText"),
        }
    }
}

impl fmt::Debug for AuthorizedFetchUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Fully redacted: the bound URL, hostname, port, and client never print.
        formatter.write_str("AuthorizedFetchUrl(<redacted>)")
    }
}

impl fmt::Debug for FetchUrlResources {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Fully redacted: no origin, hostname, port, address, or client detail prints.
        formatter.write_str("FetchUrlResources(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use futures_util::FutureExt;

    use crate::tools::{
        ToolCall, ToolCancellationHandle, ToolCapabilityClass, ToolExecutionOutcome,
        ToolExecutionPlan, ToolExecutionRequest, ToolExecutionRun, ToolExecutionStart,
        ToolOutcomeSource, ToolPermissionSet, ToolResultContent, ToolResultDisposition,
        ToolSandboxAdmissionError, ToolSandboxContract, ToolSet, ToolStartGate,
    };

    use super::*;

    const LOOPBACK_HOST: &str = "fetch-url.loopback.test";
    const ITEM_ID: &str = "itm_00000000000000000000000000000001";

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn v6(segments: [u16; 8]) -> IpAddr {
        IpAddr::V6(Ipv6Addr::from(segments))
    }

    fn addr443(ip: IpAddr) -> SocketAddr {
        SocketAddr::new(ip, 443)
    }

    fn example_origin(ip: IpAddr) -> FetchUrlOrigin {
        FetchUrlOrigin::new("https://example.com/", &[addr443(ip)])
            .expect("the example origin is valid")
    }

    fn example_resources() -> FetchUrlResources {
        FetchUrlResources::materialize([example_origin(v4(127, 0, 0, 1))])
            .expect("the example resources materialize")
    }

    // ---------- Tool surface and planning helpers ----------

    fn request_for(arguments: &str) -> ToolExecutionRequest {
        ToolExecutionRequest::new(
            ITEM_ID.parse().unwrap(),
            ToolCall::new(
                "call_fetch".parse().unwrap(),
                FETCH_URL_NAME.parse().unwrap(),
                arguments.parse().unwrap(),
                0,
            ),
        )
    }

    /// Plans one call and returns the frozen PreExecution result; panics on any other shape.
    fn plan_failure(set: &ToolSet, request: &ToolExecutionRequest) -> ToolExecutionResult {
        match set.plan(request) {
            Some(ToolExecutionPlan::PreExecution(result)) => result,
            _plan => panic!(
                "expected a PreExecution plan for arguments {}",
                request.call().arguments().canonical_json()
            ),
        }
    }

    /// Plans one call and panics unless it produces an Execute plan with exactly the
    /// `Network` permission set.
    fn assert_plans_execute(set: &ToolSet, request: &ToolExecutionRequest) {
        match set.plan(request) {
            Some(ToolExecutionPlan::Execute { permissions, .. }) => {
                assert_eq!(
                    permissions,
                    ToolPermissionSet::new([ToolCapabilityClass::Network])
                );
            }
            _plan => panic!(
                "expected an Execute plan for arguments {}",
                request.call().arguments().canonical_json()
            ),
        }
    }

    /// Plans one call and returns its move-only start factory; panics on any other shape.
    fn plan_start(set: &ToolSet, request: &ToolExecutionRequest) -> ToolExecutionStart {
        match set.plan(request) {
            Some(ToolExecutionPlan::Execute { start, .. }) => start,
            Some(ToolExecutionPlan::PreExecution(result)) => {
                panic!("expected an Execute plan, got {result:?}")
            }
            _plan => panic!("expected an Execute plan"),
        }
    }

    /// Drives one Execute plan through the exact proof path to its identity-bound outcome,
    /// isolated by one spawn like the Session Execution slot's consuming drive.  For
    /// natural-completion response tests only (the spawned run is driven by the runtime
    /// while the scripted server answers independently).
    async fn execute(set: Arc<ToolSet>, request: ToolExecutionRequest) -> ToolExecutionOutcome {
        let start = plan_start(&set, &request);
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (_handle, observer) = ToolCancellationHandle::new();
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");
        tokio::spawn(run).await.expect("the started run settles")
    }

    /// Awaits one positive server event while the run future is polled in parallel, which
    /// is what drives the exact send and the body drain forward.  The executor must stay
    /// pending until the event arrives; it settles first only if the server handshake
    /// broke, which fails the test with a precise message.  The event is the exit
    /// condition — never a poll count, sleep, or timeout.
    async fn await_server_event(
        run: &mut Pin<Box<ToolExecutionRun>>,
        event: impl std::future::Future<Output = ()>,
        what: &'static str,
    ) {
        tokio::select! {
            _ = event => {}
            _ = run.as_mut() => panic!("the executor settled before {what}"),
        }
    }

    /// One loopback authority over a scripted server: the canonical test host pinned to
    /// the server's exact address.
    fn loopback_resources(server: &TestLoopbackServer) -> FetchUrlResources {
        FetchUrlResources::loopback(LOOPBACK_HOST, server.addr())
            .expect("the loopback authority materializes")
    }

    /// The exact same-origin URL for one loopback server and path.
    fn loopback_url(server: &TestLoopbackServer, path: &str) -> String {
        format!("http://{LOOPBACK_HOST}:{}/{path}", server.addr().port())
    }

    fn outcome_content(outcome: &ToolExecutionOutcome) -> &ToolResultContent {
        match outcome {
            ToolExecutionOutcome::Completed { content, .. } => content,
            _ => panic!("expected a Completed outcome"),
        }
    }

    /// The exact frozen PreExecution result for one text.
    fn preexecution_failed_text(text: &str) -> ToolExecutionResult {
        ToolExecutionResult::PreExecution {
            disposition: ToolResultDisposition::Failed,
            content: ToolResultContent::from_text_parts(vec![text.to_owned()]).unwrap(),
        }
    }

    fn preexecution_denied_text(text: &str) -> ToolExecutionResult {
        ToolExecutionResult::PreExecution {
            disposition: ToolResultDisposition::Denied,
            content: ToolResultContent::from_text_parts(vec![text.to_owned()]).unwrap(),
        }
    }

    #[test]
    fn origin_canonicalizes_default_port_host_case_and_idna() {
        // Explicit and default :443 are one canonical effective port.
        let explicit =
            FetchUrlOrigin::new("https://example.com:443/", &[addr443(v4(127, 0, 0, 1))])
                .expect("explicit-port origin is valid");
        let default = example_origin(v4(127, 0, 0, 1));
        assert_eq!(explicit.effective_port, 443);
        assert_eq!(explicit.effective_port, default.effective_port);
        assert_eq!(
            explicit.canonical_hostname(),
            default.canonical_hostname(),
            "explicit and default :443 are one canonical origin"
        );

        // Host case folds to lowercase.
        let upper = FetchUrlOrigin::new("https://EXAMPLE.com/", &[addr443(v4(127, 0, 0, 1))])
            .expect("uppercase-host origin is valid");
        assert_eq!(upper.canonical_hostname(), "example.com");

        // IDNA: the unicode form and its explicit punycode form are one canonical host.
        let unicode = FetchUrlOrigin::new("https://例え.テスト/", &[addr443(v4(127, 0, 0, 1))])
            .expect("IDN origin is valid");
        let expected = reqwest::Url::parse("https://例え.テスト/")
            .expect("the IDN URL parses")
            .host_str()
            .expect("the IDN URL has a host")
            .to_owned();
        assert_eq!(expected, "xn--r8jz45g.xn--zckzah");
        assert_eq!(unicode.canonical_hostname(), expected);
        let punycode = FetchUrlOrigin::new(
            &format!("https://{expected}/"),
            &[addr443(v4(127, 0, 0, 1))],
        )
        .expect("punycode origin is valid");
        assert_eq!(
            unicode.canonical_hostname(),
            punycode.canonical_hostname(),
            "IDNA normalization is canonical"
        );
    }

    #[test]
    fn origin_rejects_every_whatwg_ip_literal_spelling() {
        // The WHATWG parser accepts many IPv4 spellings (one/two/three-part numbers,
        // decimal, octal and hex forms, trailing-dot IPv4) and normalizes each to a
        // canonical dotted quad; every such spelling must classify as an IP literal
        // and be rejected, exactly as a bare `127.0.0.1` is.  These are the actual
        // parser-accepted forms, probed against the pinned url parser, not a guessed
        // grammar.
        let good = [addr443(v4(127, 0, 0, 1))];
        for host in [
            "127.0.0.1",
            "127.1",
            "127",
            "127.0.1",
            "0x7f000001",
            "0x7f.1",
            "0177.0.0.1",
            "0177.0.1",
            "2130706433",
            "127.0.0.1.",
        ] {
            let origin = format!("https://{host}/");
            assert_eq!(
                FetchUrlOrigin::new(&origin, &good).expect_err("must be rejected"),
                FetchUrlOriginError::InvalidOriginHost,
                "{origin:?} is a WHATWG IPv4 form and must be rejected"
            );
        }
        for host in ["[::1]", "[0:0:0:0:0:0:0:1]", "[2001:db8::1]"] {
            let origin = format!("https://{host}/");
            assert_eq!(
                FetchUrlOrigin::new(&origin, &good).expect_err("must be rejected"),
                FetchUrlOriginError::InvalidOriginHost,
                "{origin:?} is an IPv6 literal and must be rejected"
            );
        }
    }

    #[test]
    fn origin_rejects_every_invalid_origin_form() {
        let good = [addr443(v4(127, 0, 0, 1))];
        let cases: &[(&str, FetchUrlOriginError)] = &[
            ("", FetchUrlOriginError::InvalidOriginText),
            (
                "https://example.com/\u{1}",
                FetchUrlOriginError::InvalidOriginText,
            ),
            ("not a url", FetchUrlOriginError::InvalidOriginForm),
            ("http://example.com/", FetchUrlOriginError::InvalidOriginUrl),
            ("ftp://example.com/", FetchUrlOriginError::InvalidOriginUrl),
            ("https://127.0.0.1/", FetchUrlOriginError::InvalidOriginHost),
            ("https://[::1]/", FetchUrlOriginError::InvalidOriginHost),
            (
                "https://example.com/api",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https://example.com/?q=1",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https://example.com/#frag",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https://user@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https://user:pass@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https://:pass@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https://@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https://:@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            // The WHATWG parser recovery spellings: each one parses and normalizes
            // to `https://example.com/` with the empty userinfo erased, so only the
            // raw pre-parse hierarchical-form validation can reject them as form
            // violations.
            (
                "https:/@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https:\\@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https:@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https::@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            ("https:example.com", FetchUrlOriginError::InvalidOriginForm),
            (
                "https:///@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
            (
                "https:////@example.com/",
                FetchUrlOriginError::InvalidOriginForm,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                FetchUrlOrigin::new(input, &good).expect_err("must be rejected"),
                *expected,
                "origin {input:?} must be rejected"
            );
        }
        // The erased forms are exactly the parser-recovery gap: the WHATWG parser
        // normalizes each to `https://example.com/` with no username/password, so
        // only the raw pre-parse hierarchical-form validation can reject them.
        for erased in [
            "https://@example.com/",
            "https://:@example.com/",
            "https:/@example.com/",
            "https:\\@example.com/",
            "https:@example.com/",
            "https::@example.com/",
            "https:example.com",
            "https:///@example.com/",
            "https:////@example.com/",
        ] {
            let normalized =
                reqwest::Url::parse(erased).expect("the erased form parses (that is the gap)");
            assert!(
                normalized.username().is_empty() && normalized.password().is_none(),
                "the parser erases the empty userinfo"
            );
            assert_eq!(
                normalized.host_str(),
                Some("example.com"),
                "the erased form recovers to the clean example.com origin"
            );
        }
        // Oversize origin text (> 2,048 bytes) fails before any URL parsing.
        let long = format!("https://example.com/{}", "a".repeat(3000));
        assert_eq!(
            FetchUrlOrigin::new(&long, &good).expect_err("must be rejected"),
            FetchUrlOriginError::InvalidOriginText
        );
    }

    #[test]
    fn origin_rejects_every_invalid_address_and_allows_host_explicit_ips() {
        let origin = "https://example.com/";
        // Wrong port and out-of-range counts fail closed.
        assert_eq!(
            FetchUrlOrigin::new(origin, &[SocketAddr::new(v4(127, 0, 0, 1), 8443)])
                .expect_err("must be rejected"),
            FetchUrlOriginError::InvalidAddressPort
        );
        assert_eq!(
            FetchUrlOrigin::new(origin, &[]).expect_err("must be rejected"),
            FetchUrlOriginError::InvalidAddressCount
        );
        let nine: Vec<SocketAddr> = (1..=9)
            .map(|octet| SocketAddr::new(v4(10, 0, 0, octet), 443))
            .collect();
        assert_eq!(
            FetchUrlOrigin::new(origin, &nine).expect_err("must be rejected"),
            FetchUrlOriginError::InvalidAddressCount,
            "9 raw addresses exceed the 1..=8 bound even before dedupe"
        );
        // Unspecified, multicast, and IPv4-broadcast, including IPv4-mapped edges.
        let rejected: &[IpAddr] = &[
            v4(0, 0, 0, 0),
            v4(224, 0, 0, 1),
            v4(255, 255, 255, 255),
            v6([0, 0, 0, 0, 0, 0, 0, 0]),
            v6([0xff02, 0, 0, 0, 0, 0, 0, 1]),
            v6([0, 0, 0, 0, 0, 0xffff, 0x0000, 0x0000]), // ::ffff:0.0.0.0 (mapped unspecified)
            v6([0, 0, 0, 0, 0, 0xffff, 0xe000, 0x0001]), // ::ffff:224.0.0.1 (mapped multicast)
            v6([0, 0, 0, 0, 0, 0xffff, 0xffff, 0xffff]), // ::ffff:255.255.255.255 (mapped broadcast)
        ];
        for ip in rejected {
            assert_eq!(
                FetchUrlOrigin::new(origin, &[SocketAddr::new(*ip, 443)])
                    .expect_err("must be rejected"),
                FetchUrlOriginError::InvalidAddressIp,
                "address {ip} must be rejected"
            );
        }
        // The Runtime never guesses a purpose for public/private/loopback/link-local/
        // documentation ranges: any host-explicit exact address is authority.
        let allowed: &[IpAddr] = &[
            v4(127, 0, 0, 1),
            v4(192, 168, 1, 5),
            v4(8, 8, 8, 8),
            v6([0, 0, 0, 0, 0, 0, 0, 1]),
            v6([0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001]), // ::ffff:127.0.0.1 (mapped loopback)
        ];
        for ip in allowed {
            assert!(
                FetchUrlOrigin::new(origin, &[SocketAddr::new(*ip, 443)]).is_ok(),
                "address {ip} must be allowed"
            );
        }
    }

    #[test]
    fn origin_deduplicates_addresses_preserving_first_order() {
        let a = SocketAddr::new(v4(10, 0, 0, 1), 443);
        let b = SocketAddr::new(v4(10, 0, 0, 2), 443);
        let c = SocketAddr::new(v4(10, 0, 0, 3), 443);
        let origin = FetchUrlOrigin::new("https://example.com/", &[a, b, a, c, b])
            .expect("deduped origin is valid");
        assert_eq!(
            origin.addresses,
            vec![a, b, c],
            "first occurrence order wins"
        );
        let collapsed =
            FetchUrlOrigin::new("https://example.com/", &[a, a, a]).expect("duplicates collapse");
        assert_eq!(collapsed.addresses, vec![a]);
    }

    #[test]
    fn origin_accepts_explicit_nondefault_port() {
        let origin = FetchUrlOrigin::new(
            "https://example.com:8443/",
            &[SocketAddr::new(v4(10, 0, 0, 1), 8443)],
        )
        .expect("non-default-port origin is valid");
        assert_eq!(origin.effective_port, 8443);
    }

    #[test]
    fn materialize_rejects_duplicate_canonical_origins_and_accepts_empty() {
        let empty = FetchUrlResources::materialize([]).expect("empty authority is representable");
        assert!(empty.origins.is_empty(), "no origin means no pinned client");

        let a = example_origin(v4(127, 0, 0, 1));
        let b = FetchUrlOrigin::new("https://EXAMPLE.com:443/", &[addr443(v4(10, 0, 0, 1))])
            .expect("case-folded duplicate origin is valid");
        assert_eq!(
            FetchUrlResources::materialize([a, b]).expect_err("must be rejected"),
            FetchUrlResourcesError::DuplicateOrigin,
            "canonical duplicates fail closed"
        );

        // Distinct effective ports are distinct canonical origins.
        let c = FetchUrlOrigin::new(
            "https://example.com:8443/",
            &[SocketAddr::new(v4(10, 0, 0, 1), 8443)],
        )
        .expect("second-port origin is valid");
        let d = example_origin(v4(127, 0, 0, 2));
        assert!(
            FetchUrlResources::materialize([c, d]).is_ok(),
            "same host on distinct ports is two origins"
        );
    }

    #[test]
    fn raw_hierarchical_authority_rejects_every_recovery_spelling_and_userinfo() {
        // A literal `@` inside the raw hierarchical authority is a userinfo
        // delimiter, including the entirely-empty userinfo forms the WHATWG parser
        // erases.
        for raw in [
            "https://@example.com/",
            "https://:@example.com/",
            "https://user@example.com/",
            "https://user:pass@example.com/",
            "https://:pass@example.com/",
            "https://exa@mple.com/",
            "https://example.com@evil.example/",
            "http://@example.com/",
        ] {
            assert_eq!(
                validate_raw_hierarchical_authority(raw),
                Err(RawHierarchicalAuthorityError::UserinfoAt),
                "{raw:?} carries a literal authority @"
            );
        }
        // One-slash, backslash, and no-slash spellings are not the exact
        // `scheme://` hierarchical form at all, whatever the parser recovers them
        // to.
        for raw in [
            "https:/@example.com/",
            "https:\\@example.com/",
            "https:\\\\@example.com/",
            "https:@example.com/",
            "https::@example.com/",
            "https:example.com",
            "http:/@example.com/",
            "http:\\@example.com/",
            "http:@example.com/",
            "http::@example.com/",
            "http:example.com",
            "mailto:user@example.com",
        ] {
            assert_eq!(
                validate_raw_hierarchical_authority(raw),
                Err(RawHierarchicalAuthorityError::NotHierarchical),
                "{raw:?} is not the exact scheme:// hierarchical form"
            );
        }
        // Three-or-more-slash spellings (and a backslash where the extra slash
        // would go) fold into an empty raw authority.
        for raw in [
            "https:///@example.com/",
            "https:////@example.com/",
            "http:///@example.com/",
            "http:////@example.com/",
            "http://\\@example.com/",
            "http://\\\\@example.com/",
            "http://\\example.com/",
        ] {
            assert_eq!(
                validate_raw_hierarchical_authority(raw),
                Err(RawHierarchicalAuthorityError::EmptyAuthority),
                "{raw:?} has an empty raw authority"
            );
        }
        // Without a scheme colon the raw text is not an absolute URL at all.
        for raw in ["not a url", "//example.com/@x"] {
            assert_eq!(
                validate_raw_hierarchical_authority(raw),
                Err(RawHierarchicalAuthorityError::NoSchemeColon),
                "{raw:?} has no scheme colon"
            );
        }
        // The recovery spellings are exactly the parser-recovery gap the validator
        // closes: the WHATWG parser accepts each one and normalizes it to the clean
        // origin with no username/password, so only the raw pre-parse form
        // validation can reject them.
        for erased in [
            "https:/@example.com/",
            "https:\\@example.com/",
            "https:@example.com/",
            "https::@example.com/",
            "https:example.com",
            "https:///@example.com/",
            "https:////@example.com/",
            "http:/@example.com/",
            "http:\\@example.com/",
            "http:@example.com/",
            "http::@example.com/",
            "http:///@example.com/",
            "http:////@example.com/",
        ] {
            let normalized = reqwest::Url::parse(erased)
                .expect("the recovery spelling parses (that is the gap)");
            assert!(
                normalized.username().is_empty() && normalized.password().is_none(),
                "the parser erases the empty userinfo: {erased:?}"
            );
            assert_eq!(
                normalized.host_str(),
                Some("example.com"),
                "the recovery spelling lands on the clean example.com origin"
            );
        }
    }

    #[test]
    fn raw_hierarchical_authority_accepts_canonical_forms_and_path_query_content() {
        // Scheme case is left to the parser's canonicalization, a missing trailing
        // slash is the parser's `/` path, an explicit port is ordinary authority
        // content, and a literal `@` after the authority terminator (path/query) or
        // a percent-encoded `%40` is ordinary URL content, not userinfo.  A `\`
        // terminates the authority exactly like `/` for special schemes.
        for raw in [
            "HTTPS://example.com",
            "https://example.com",
            "https://example.com/",
            "https://example.com:443/",
            "http://example.com/",
            "https://example.com/@user",
            "https://example.com/path?q=@x",
            "https://example.com/a%40b?q=%40",
            "https://example.com\\@foo",
            "https://example.com\\path@foo",
        ] {
            assert!(
                validate_raw_hierarchical_authority(raw).is_ok(),
                "{raw:?} is the exact hierarchical form with at most path/query @/%40"
            );
        }
    }

    #[test]
    fn authorize_accepts_same_origin_path_and_query() {
        let resources = example_resources();
        // Literal `@` and percent-encoded `%40` in path/query stay ordinary URL
        // content: they are not userinfo and remain authorized once the origin
        // matches.  The target is opaque, so the seam only asserts success: the raw
        // URL and client are never borrowed out of it.
        for url in [
            "https://example.com/",
            "https://example.com:443/",
            "https://EXAMPLE.com/",
            "https://example.com/some/path",
            "https://example.com/some/path?q=1&r=2",
            "https://example.com/?q=1",
            "https://example.com/@user",
            "https://example.com/a%40b?q=%40",
            "https://example.com/path?q=@x",
        ] {
            assert!(resources.authorize(url).is_ok(), "{url} must be authorized");
        }
    }

    #[test]
    fn authorize_denies_foreign_scheme_host_subdomain_port_and_ip_literals() {
        let resources = example_resources();
        for url in [
            "http://example.com/",
            "https://example.org/",
            "https://sub.example.com/",
            "https://example.com:8443/",
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://127.1/",
            "https://0x7f000001/",
        ] {
            assert_eq!(
                resources.authorize(url).expect_err("must be denied"),
                FetchUrlAuthorizationError::Denied,
                "{url} must be denied"
            );
        }
    }

    #[test]
    fn authorize_classifies_invalid_urls() {
        let resources = example_resources();
        for url in [
            "",
            "not a url",
            "https://user@example.com/",
            "https://user:pass@example.com/",
            "https://@example.com/",
            "https://:@example.com/",
            // The WHATWG parser recovery spellings: each one parses and normalizes
            // to the clean `example.com` origin with the empty userinfo erased, so
            // only the raw pre-parse hierarchical-form validation can classify
            // them as invalid.
            "https:/@example.com/",
            "https:\\@example.com/",
            "https:@example.com/",
            "https::@example.com/",
            "https:example.com",
            "https:///@example.com/",
            "https:////@example.com/",
            "http:/@example.com/",
            "http:\\@example.com/",
            "http:@example.com/",
            "http::@example.com/",
            "http:example.com",
            "http:///@example.com/",
            "http:////@example.com/",
            "https://example.com/#frag",
            "https://example.com/path#frag",
            "https://example.com/\u{1}",
        ] {
            assert_eq!(
                resources
                    .authorize(url)
                    .expect_err("must classify as invalid"),
                FetchUrlAuthorizationError::InvalidUrl,
                "{url:?} must classify as invalid"
            );
        }
        let long = format!("https://example.com/{}", "a".repeat(4200));
        assert_eq!(
            resources
                .authorize(&long)
                .expect_err("must classify as invalid"),
            FetchUrlAuthorizationError::InvalidUrl,
            "a URL over 4,096 bytes is invalid"
        );
    }

    #[test]
    fn debug_and_display_are_fully_redacted() {
        let origin = FetchUrlOrigin::new(
            "https://example.com/",
            &[addr443(v4(127, 0, 0, 1)), addr443(v4(10, 0, 0, 1))],
        )
        .expect("redaction origin is valid");
        let origin_debug = format!("{origin:?}");
        let origin_display = format!("{origin}");
        let resources = FetchUrlResources::materialize([origin]).expect("resources materialize");
        let target = resources
            .authorize("https://example.com/secret/path?q=1")
            .expect("the redaction URL authorizes");

        for rendered in [
            origin_debug,
            origin_display,
            format!("{resources:?}"),
            format!("{target:?}"),
        ] {
            for secret in ["example.com", "127.0.0.1", "10.0.0.1", "443", "secret"] {
                assert!(
                    !rendered.contains(secret),
                    "Debug/Display leaked {secret:?}: {rendered}"
                );
            }
        }
    }

    #[test]
    fn reject_all_resolver_rejects_every_name() {
        let name: reqwest::dns::Name = "unmatched.invalid".parse().expect("test name parses");
        let resolved = RejectAllResolver.resolve(name).now_or_never();
        assert!(
            matches!(resolved, Some(Err(_))),
            "the reject-all resolver must fail closed without touching any resolver"
        );
    }

    // ---------- Tool surface and planning ----------

    #[test]
    fn builtin_defines_exactly_fetch_url_with_the_frozen_description_and_closed_schema() {
        let resources = example_resources();
        let set = build_tool_set(Arc::new(resources));

        let definitions = set.definitions();
        assert_eq!(definitions.len(), 1);
        let definition = &definitions[0];
        assert_eq!(definition.name().as_str(), FETCH_URL_NAME);
        assert_eq!(definition.mode(), ToolExecutionMode::Parallel);
        // The frozen description is documented by this assertion: any edit to the disclosed
        // description must be reflected here deliberately (ADR 0147 decision 1, verbatim).
        assert_eq!(definition.spec.description.as_ref(), FETCH_URL_DESCRIPTION);

        // The prompt view discloses exactly the same single spec (name, description,
        // closed schema); planner and sandbox internals never enter the model context.
        let view = set.prompt_view();
        assert!(!view.is_empty());
        assert_eq!(view.specs().len(), 1);
        assert_eq!(view.specs()[0].name().as_str(), FETCH_URL_NAME);
        assert_eq!(view.specs()[0].description(), FETCH_URL_DESCRIPTION);

        // The disclosed schema is exactly the frozen schema: canonical bytes round-trip to
        // the same semantic value and stay within the bounded schema limit.
        let schema = view.specs()[0].input_schema();
        assert_eq!(
            schema.canonical_json(),
            FETCH_URL_SCHEMA
                .parse::<crate::wire::BoundedJsonSchema>()
                .unwrap()
                .canonical_json()
        );
        assert!(
            schema.canonical_bytes().len()
                <= crate::wire::ProtocolLimits::v1_0()
                    .embedded_json
                    .schema
                    .max_encoded_bytes as usize
        );

        // The canonical disclosure is a closed object with exactly one required `url`
        // string capped at 4,096 bytes (the semantic authorize gate is the authority).
        let canonical: serde_json::Value =
            serde_json::from_str(schema.canonical_json()).expect("the schema is valid JSON");
        let root = canonical.as_object().expect("the schema root is an object");
        assert_eq!(
            root.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(root.get("required"), Some(&serde_json::json!(["url"])));
        let properties = root
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("the schema discloses properties");
        assert_eq!(
            properties.len(),
            1,
            "the builtin discloses exactly one property"
        );
        assert_eq!(
            properties.get("url"),
            Some(&serde_json::json!({"type": "string", "maxLength": 4096}))
        );
    }

    #[test]
    fn fetch_url_declares_exactly_network_parallel_mode_and_an_exact_sandbox_contract() {
        let resources = example_resources();
        let set = build_tool_set(Arc::new(resources));

        assert_eq!(set.definitions()[0].mode(), ToolExecutionMode::Parallel);

        // The Execute plan's final permission set is exactly Network.
        let request = request_for(r#"{"url":"https://example.com/some/path"}"#);
        match set.plan(&request) {
            Some(ToolExecutionPlan::Execute { permissions, .. }) => {
                assert_eq!(
                    permissions,
                    ToolPermissionSet::new([ToolCapabilityClass::Network])
                );
                assert!(permissions.contains(ToolCapabilityClass::Network));
                assert!(!permissions.contains(ToolCapabilityClass::FilesystemRead));
                assert!(!permissions.contains(ToolCapabilityClass::FilesystemWrite));
                assert!(!permissions.contains(ToolCapabilityClass::Process));
            }
            _ => panic!("the valid call plans an Execute shape"),
        }

        // The captured sandbox contract is available exactly for Network, so the
        // planner's own admission passes and every other class fails closed.
        let sandbox = &set.inner.sandbox;
        assert_eq!(
            *sandbox,
            ToolSandboxContract::available([ToolCapabilityClass::Network])
        );
        assert!(
            sandbox
                .admit(ToolPermissionSet::new([ToolCapabilityClass::Network]))
                .is_ok()
        );
        assert!(matches!(
            sandbox.admit(ToolPermissionSet::new([
                ToolCapabilityClass::FilesystemRead
            ])),
            Err(ToolSandboxAdmissionError::CapabilityGap { .. })
        ));
        assert!(
            set.plan(&request)
                .is_some_and(|plan| matches!(plan, ToolExecutionPlan::Execute { .. }))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn parse_and_semantic_failures_settle_the_frozen_preexecution_failed_result() {
        let server = TestLoopbackServer::spawn();
        let resources = loopback_resources(&server);
        let set = build_tool_set(Arc::new(resources));
        let invalid = vec![
            // Structural parse failures at every layer.
            "{}".to_owned(),
            r#"{"url":"https://example.com/","extra":1}"#.to_owned(),
            r#"{"Url":"https://example.com/"}"#.to_owned(),
            r#"{"url":null}"#.to_owned(),
            r#"{"url":1}"#.to_owned(),
            r#"{"url":true}"#.to_owned(),
            r#"{"url":[]}"#.to_owned(),
            r#"{"url":{}}"#.to_owned(),
            // The semantic authorize gate: empty, non-absolute, userinfo, fragment, and
            // the WHATWG parser-recovery spellings are all invalid arguments, never a
            // denial (ADR 0147 decisions 1 and 4).
            r#"{"url":""}"#.to_owned(),
            r#"{"url":"not a url"}"#.to_owned(),
            r#"{"url":"https://user@example.com/"}"#.to_owned(),
            r#"{"url":"https://user:pass@example.com/"}"#.to_owned(),
            r#"{"url":"https://@example.com/"}"#.to_owned(),
            r#"{"url":"https://:@example.com/"}"#.to_owned(),
            r#"{"url":"https:/@example.com/"}"#.to_owned(),
            r#"{"url":"https:example.com"}"#.to_owned(),
            r#"{"url":"https://example.com/#frag"}"#.to_owned(),
            r#"{"url":"https://example.com/path#frag"}"#.to_owned(),
            r#"{"url":"https://example.com/\u0001"}"#.to_owned(),
            format!(r#"{{"url":"https://example.com/{}"}}"#, "a".repeat(4200)),
        ];

        for (index, arguments) in invalid.iter().enumerate() {
            let request = request_for(arguments);
            let result = plan_failure(&set, &request);
            assert_eq!(
                result,
                preexecution_failed_text(INVALID_ARGUMENTS_TEXT),
                "arguments #{index} {arguments:?} must settle the frozen failed pre-execution result"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_exact_origin_authority_settles_the_frozen_preexecution_denied_result() {
        let server = TestLoopbackServer::spawn();
        let port = server.addr().port();
        let resources = loopback_resources(&server);
        let set = build_tool_set(Arc::new(resources));
        for (arguments, reason) in [
            (
                r#"{"url":"https://example.com/"}"#.to_owned(),
                "foreign host",
            ),
            (
                format!(r#"{{"url":"http://{LOOPBACK_HOST}:{}/"}}"#, port + 1),
                "foreign port",
            ),
            (
                format!(r#"{{"url":"https://{LOOPBACK_HOST}:{}/"}}"#, port),
                "foreign scheme",
            ),
            (
                format!(r#"{{"url":"http://sub.{LOOPBACK_HOST}:{}/"}}"#, port),
                "subdomain",
            ),
            (
                format!(r#"{{"url":"http://127.0.0.1:{}/"}}"#, port),
                "IP literal",
            ),
        ] {
            let request = request_for(&arguments);
            let result = plan_failure(&set, &request);
            assert_eq!(
                result,
                preexecution_denied_text(NETWORK_DENIED_TEXT),
                "{reason} ({arguments}) must settle the frozen denied pre-execution result"
            );
        }

        // A denied URL never produces a start factory: the exact request's gate still
        // accepts its single reservation and start exactly like a never-touched gate.
        let denied = request_for(&format!(
            r#"{{"url":"http://{LOOPBACK_HOST}:{}/"}}"#,
            port + 1
        ));
        let gate = ToolStartGate::new(denied.clone());
        assert!(gate.reserve(&denied).unwrap().start().is_ok());

        // The same loopback origin authorizes its exact path/query (case-folded host,
        // explicit port): denial is exact per origin, never per path.
        assert_plans_execute(
            &set,
            &request_for(&format!(
                r#"{{"url":"{}"}}"#,
                loopback_url(&server, "some/path?q=1")
            )),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn valid_authorized_plan_executes_exactly_network_and_the_executor_never_sends_at_plan_time()
     {
        let server = TestLoopbackServer::spawn();
        let resources = loopback_resources(&server);
        let set = build_tool_set(Arc::new(resources));
        let request = request_for(&format!(
            r#"{{"url":"{}"}}"#,
            loopback_url(&server, "plan-only/path")
        ));

        // Planning authorizes and constructs the move-only start factory; the exact send
        // is owned by the executor and no connection exists yet.
        assert_plans_execute(&set, &request);
        let _start = plan_start(&set, &request);
        assert!(
            server.captured().is_empty(),
            "the planner must never send: zero requests reach the wire at plan time"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reject_all_resolver_client_fails_closed_without_ambient_dns() {
        let client = crate::http_transport::client_builder()
            .dns_resolver(RejectAllResolver)
            .build()
            .expect("the reject-all test client builds");
        let error = client
            .get("http://unmatched.invalid/")
            .send()
            .await
            .expect_err("an unmatched name must fail before any socket opens");
        assert!(
            error.is_connect(),
            "the resolution rejection is a connect-phase failure: {error}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_pinned_client_uses_pinned_address_and_canonical_host() {
        let server = TestLoopbackServer::spawn();
        let port = server.addr().port();
        let resources = FetchUrlResources::loopback(LOOPBACK_HOST, server.addr())
            .expect("the loopback authority materializes");

        let target = resources
            .authorize(&format!("http://{LOOPBACK_HOST}:{port}/some/path?q=1"))
            .expect("the loopback URL is exactly same-origin");

        // The .test TLD never resolves in ambient DNS, and this client's only resolver
        // rejects everything: the request succeeding proves the resolve_to_addrs
        // override pinned the hostname to the loopback address with no ambient DNS.
        // The target owns the exact send: exactly one GET reaches the wire with the
        // bound path/query, the canonical Host, the fixed product user-agent, the
        // fixed ADR 0147 headers, and no body framing.
        let response = target
            .send()
            .await
            .expect("the pinned loopback request must succeed without ambient DNS");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.text().await.expect("the test body reads"),
            "ok",
            "the response must be the exact loopback body"
        );

        let captured = server.captured();
        assert_eq!(
            captured.len(),
            1,
            "exactly one GET lands on the loopback server"
        );
        assert_eq!(
            captured[0].request_line, "GET /some/path?q=1 HTTP/1.1",
            "the exact path and query reach the wire"
        );
        assert_eq!(
            captured[0].header("host"),
            Some(format!("{LOOPBACK_HOST}:{port}").as_str()),
            "the canonical hostname (plus explicit port) is the HTTP Host"
        );
        assert_eq!(
            captured[0].header("user-agent"),
            Some(crate::http_transport::USER_AGENT),
            "the fixed product user-agent is on the wire"
        );
        assert_eq!(
            captured[0].header("accept"),
            Some("text/plain, application/json"),
            "the fixed ADR 0147 Accept is on the wire"
        );
        assert_eq!(
            captured[0].header("accept-encoding"),
            Some("identity"),
            "the fixed ADR 0147 identity encoding is on the wire"
        );
        assert_eq!(
            captured[0].header("connection"),
            Some("close"),
            "the fixed ADR 0147 Connection: close is on the wire"
        );
        assert!(
            captured[0].header("content-length").is_none(),
            "a bodyless GET carries no Content-Length framing"
        );
        assert!(
            captured[0].header("transfer-encoding").is_none(),
            "a bodyless GET carries no Transfer-Encoding framing"
        );
    }

    #[test]
    fn loopback_rejects_nonloopback_address_zero_port_and_bad_hosts() {
        // The test-only contract is enforced, not documented: a non-loopback address,
        // a zero port, an IP-literal host in any WHATWG spelling, a host with a path,
        // query, fragment, or userinfo, and a host that is not a valid canonical DNS
        // hostname all fail closed payload-free.
        let bad_addresses = [
            SocketAddr::new(v4(10, 0, 0, 1), 8080),
            SocketAddr::new(v4(127, 0, 0, 1), 0), // loopback IP, zero port
            SocketAddr::new(v4(8, 8, 8, 8), 8080),
        ];
        for address in bad_addresses {
            assert_eq!(
                FetchUrlResources::loopback(LOOPBACK_HOST, address).expect_err("must be rejected"),
                FetchUrlResourcesError::InvalidTestAuthority,
                "address {address} must be rejected by the test-only contract"
            );
        }
        let loopback = SocketAddr::new(v4(127, 0, 0, 1), 8080);
        let bad_hosts = [
            "",
            "exa mple.com",
            "example.com:8080",
            "example.com/path",
            "example.com?x=1",
            "example.com#frag",
            "user@example.com",
            "user:pass@example.com",
            "@example.com",
            ":@example.com",
            // The parser-recovery spellings: these embed as `http://\@example.com/`,
            // `http:///@example.com/`, `http:////@example.com/`, and
            // `http:///:@example.com/`, each of which the WHATWG parser recovers to
            // a clean bare host with the empty userinfo erased, so only the raw
            // pre-parse hierarchical-form validation can reject them as bare-host
            // violations.
            "\\@example.com",
            "/@example.com",
            "//@example.com",
            "/:@example.com",
            "//:@example.com",
            "\\example.com",
            "//example.com",
            "127.0.0.1",
            "127.1",
            "0x7f000001",
            "0177.0.0.1",
            "[::1]",
            "::1",
        ];
        for host in bad_hosts {
            assert_eq!(
                FetchUrlResources::loopback(host, loopback).expect_err("must be rejected"),
                FetchUrlResourcesError::InvalidTestAuthority,
                "host {host:?} must be rejected by the test-only contract"
            );
        }
        // The parser-erasure gap: `@example.com` and `:@example.com` embed as
        // `http://@example.com/` / `http://:@example.com/`, and the recovery
        // spellings embed as `http://\@example.com/`, `http:///@example.com/`,
        // `http:////@example.com/`, and `http:///:@example.com/`; the WHATWG parser
        // normalizes each to a clean bare host with no userinfo, so only the raw
        // pre-parse hierarchical-form validation can reject them as bare-host
        // violations.
        for erased in [
            "@example.com",
            ":@example.com",
            "\\@example.com",
            "/@example.com",
            "//@example.com",
            "/:@example.com",
            "//:@example.com",
            "\\example.com",
            "//example.com",
        ] {
            let embedded = format!("http://{erased}/");
            let normalized =
                reqwest::Url::parse(&embedded).expect("the erased form parses (that is the gap)");
            assert!(
                normalized.username().is_empty() && normalized.password().is_none(),
                "the parser erases the empty userinfo"
            );
            assert_eq!(
                normalized.host_str(),
                Some("example.com"),
                "the erased form recovers to the clean example.com host"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_pins_only_the_parser_canonical_host() {
        // The parser-canonicalized host (lowercased) is what gets pinned and what the
        // request's Host header carries, exactly like the production origin path.
        let server = TestLoopbackServer::spawn();
        let port = server.addr().port();
        let resources = FetchUrlResources::loopback("FETCH-URL.LOOPBACK.TEST", server.addr())
            .expect("the loopback authority materializes");

        let target = resources
            .authorize(&format!("http://FETCH-URL.LOOPBACK.TEST:{port}/"))
            .expect("the case-folded loopback URL is exactly same-origin");
        let response = target
            .send()
            .await
            .expect("the pinned loopback request succeeds");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let captured = server.captured();
        assert_eq!(
            captured[0].header("host"),
            Some(format!("{LOOPBACK_HOST}:{port}").as_str()),
            "the canonical lowercased hostname is the HTTP Host"
        );
    }

    // ---------- Response policy matrix ----------

    /// One 200 response head with the given extra header lines and Content-Length.
    fn ok_head(extra_headers: &str, body_len: usize) -> String {
        format!(
            "HTTP/1.1 200 OK\r\n{extra_headers}Content-Length: {body_len}\r\nConnection: close\r\n\r\n"
        )
    }

    /// A 200 response head with the given extra header lines and chunked framing.
    fn chunked_head(extra_headers: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\n{extra_headers}Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
    }

    /// Encodes `body` as HTTP/1.1 chunked framing in 8,192-byte chunks, ending with the
    /// terminal zero chunk.
    fn chunked_body(body: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(body.len() + 64);
        for chunk in body.chunks(8192) {
            encoded.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            encoded.extend_from_slice(chunk);
            encoded.extend_from_slice(b"\r\n");
        }
        encoded.extend_from_slice(b"0\r\n\r\n");
        encoded
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exact_2xx_text_plain_and_application_json_bodies_are_returned_verbatim() {
        let cases: &[(&str, &[u8])] = &[
            // text/plain with parameters (charset never transcodes; bytes stay UTF-8).
            ("Content-Type: text/plain\r\n", b"hello world"),
            (
                "Content-Type: text/plain; charset=utf-8\r\n",
                "plain é".as_bytes(),
            ),
            ("Content-Type: TEXT/PLAIN;charset=utf-8\r\n", b"case folded"),
            ("Content-Type: application/json\r\n", b"{\"a\":1}"),
            (
                "Content-Type: application/json; charset=UTF-8\r\n",
                b"[1,2,3]",
            ),
            (
                "Content-Type: text/plain; note=\"a;b\"; charset=utf-8\r\n",
                b"quoted semicolon",
            ),
            (
                "Content-Type: text/plain; boundary=xyz; x=1\r\n",
                b"multiple params",
            ),
            // Empty body: exactly one empty Text part.
            ("Content-Type: text/plain\r\n", b""),
        ];
        for (index, (content_type, body)) in cases.iter().enumerate() {
            let mut server = TestLoopbackServer::spawn_with(ServerScript::Respond {
                head: ok_head(content_type, body.len()),
                body: body.to_vec(),
            });
            let set = build_tool_set(Arc::new(loopback_resources(&server)));
            let outcome = execute(
                set,
                request_for(&format!(
                    r#"{{"url":"{}"}}"#,
                    loopback_url(&server, "exact")
                )),
            )
            .await;
            assert!(
                matches!(
                    outcome,
                    ToolExecutionOutcome::Completed {
                        source: ToolOutcomeSource::Executed,
                        disposition: ToolResultDisposition::Succeeded,
                        ref content,
                        ..
                    } if content.parts().len() == 1
                        && content.parts()[0].as_text().as_bytes() == *body
                ),
                "case #{index} must return the exact body bytes as one Text part: {outcome:?}"
            );
            server.wait_finished().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn content_type_matrix_settles_the_frozen_unsupported_text() {
        // Missing, duplicate, malformed, and unsupported Content-Type fields all settle
        // the same frozen Completed + Failed text; the response body is never read or
        // disclosed, so each case can carry a secret body that must never surface.
        let secret_body = b"SECRET-CONTENT-TYPE-BODY";
        let cases: Vec<String> = vec![
            // Missing field entirely.
            "".to_owned(),
            // Duplicate fields.
            "Content-Type: text/plain\r\nContent-Type: text/plain\r\n".to_owned(),
            // Unsupported base media types.
            "Content-Type: text/html\r\n".to_owned(),
            "Content-Type: application/pdf\r\n".to_owned(),
            "Content-Type: text/plain+extra\r\n".to_owned(),
            "Content-Type: text\r\n".to_owned(),
            "Content-Type: text/plain/extra\r\n".to_owned(),
            "Content-Type: /plain\r\n".to_owned(),
            "Content-Type: text/\r\n".to_owned(),
            "Content-Type: text /plain\r\n".to_owned(),
            // Malformed parameter lists.
            "Content-Type: text/plain;\r\n".to_owned(),
            "Content-Type: text/plain; charset\r\n".to_owned(),
            "Content-Type: text/plain; =utf-8\r\n".to_owned(),
            "Content-Type: text/plain; charset=\r\n".to_owned(),
            "Content-Type: text/plain; charset=\"unclosed\r\n".to_owned(),
            "Content-Type: text/plain; charset=utf-8 junk\r\n".to_owned(),
            "Content-Type: text/plain; charset=\"a\"x\r\n".to_owned(),
            // A raw control byte in the header value (e.g. the backspace 0x08) is
            // rejected by hyper's HTTP/1 parser before reqwest ever builds a Response,
            // so it is a transport failure, not a Content-Type validator case: see
            // `a_raw_control_byte_in_a_response_header_maps_to_could_not_fetch`.
        ];
        for (index, content_type) in cases.iter().enumerate() {
            let head = ok_head(content_type, secret_body.len());
            let mut server = TestLoopbackServer::spawn_with(ServerScript::Respond {
                head,
                body: secret_body.to_vec(),
            });
            let set = build_tool_set(Arc::new(loopback_resources(&server)));
            let outcome = execute(
                set,
                request_for(&format!(r#"{{"url":"{}"}}"#, loopback_url(&server, "type"))),
            )
            .await;
            assert!(
                matches!(
                    outcome,
                    ToolExecutionOutcome::Completed {
                        source: ToolOutcomeSource::Executed,
                        disposition: ToolResultDisposition::Failed,
                        ref content,
                        ..
                    } if content.parts().len() == 1
                        && content.parts()[0].as_text() == UNSUPPORTED_RESPONSE_TEXT
                ),
                "Content-Type case #{index} {content_type:?} must settle the frozen unsupported result"
            );
            assert!(
                !outcome_content(&outcome).parts()[0]
                    .as_text()
                    .contains("SECRET"),
                "case #{index} must never disclose the unread body"
            );
            server.wait_finished().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn content_encoding_matrix_settles_identity_or_the_frozen_unsupported_text() {
        // Absent and exactly-one identity (trim, case-insensitive) succeed; duplicates, a
        // comma list, an empty value, and any real coding settle the frozen unsupported
        // text without reading the body.
        let body = b"encoding body";
        let succeeded: &[&str] = &[
            "",
            "Content-Encoding: identity\r\n",
            "Content-Encoding: Identity\r\n",
            "Content-Encoding:  identity  \r\n",
        ];
        for (index, encoding) in succeeded.iter().enumerate() {
            let mut server = TestLoopbackServer::spawn_with(ServerScript::Respond {
                head: ok_head(
                    &format!("Content-Type: text/plain\r\n{encoding}"),
                    body.len(),
                ),
                body: body.to_vec(),
            });
            let set = build_tool_set(Arc::new(loopback_resources(&server)));
            let outcome = execute(
                set,
                request_for(&format!(
                    r#"{{"url":"{}"}}"#,
                    loopback_url(&server, "encoding")
                )),
            )
            .await;
            assert!(
                matches!(
                    outcome,
                    ToolExecutionOutcome::Completed {
                        source: ToolOutcomeSource::Executed,
                        disposition: ToolResultDisposition::Succeeded,
                        ref content,
                        ..
                    } if content.parts().len() == 1
                        && content.parts()[0].as_text() == "encoding body"
                ),
                "encoding case #{index} {encoding:?} must succeed"
            );
            server.wait_finished().await;
        }
        let rejected: &[&str] = &[
            "Content-Encoding: gzip\r\n",
            "Content-Encoding: br\r\n",
            "Content-Encoding: identity\r\nContent-Encoding: identity\r\n",
            "Content-Encoding: gzip\r\nContent-Encoding: identity\r\n",
            "Content-Encoding: identity, gzip\r\n",
            "Content-Encoding: \r\n",
        ];
        for (index, encoding) in rejected.iter().enumerate() {
            let mut server = TestLoopbackServer::spawn_with(ServerScript::Respond {
                head: ok_head(
                    &format!("Content-Type: text/plain\r\n{encoding}"),
                    body.len(),
                ),
                body: body.to_vec(),
            });
            let set = build_tool_set(Arc::new(loopback_resources(&server)));
            let outcome = execute(
                set,
                request_for(&format!(
                    r#"{{"url":"{}"}}"#,
                    loopback_url(&server, "encoding")
                )),
            )
            .await;
            assert!(
                matches!(
                    outcome,
                    ToolExecutionOutcome::Completed {
                        source: ToolOutcomeSource::Executed,
                        disposition: ToolResultDisposition::Failed,
                        ref content,
                        ..
                    } if content.parts().len() == 1
                        && content.parts()[0].as_text() == UNSUPPORTED_RESPONSE_TEXT
                ),
                "encoding case #{index} {encoding:?} must settle the frozen unsupported result"
            );
            server.wait_finished().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_2xx_responses_never_disclose_body_status_or_headers() {
        // A 404 with a secret body, a 500, and a 301 with a secret Location all settle the
        // one frozen could-not-fetch text; the body, status, and headers are never read
        // into any model-visible result.
        let cases: &[&str] = &[
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 13\r\n\r\n",
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n",
            "HTTP/1.1 301 Moved Permanently\r\nLocation: https://secret-evil.example/\r\nContent-Length: 13\r\n\r\n",
        ];
        for (index, head) in cases.iter().enumerate() {
            let mut server = TestLoopbackServer::spawn_with(ServerScript::Respond {
                head: (*head).to_owned(),
                body: b"SECRET-404-BODY".to_vec(),
            });
            let set = build_tool_set(Arc::new(loopback_resources(&server)));
            let outcome = execute(
                set,
                request_for(&format!(
                    r#"{{"url":"{}"}}"#,
                    loopback_url(&server, "missing")
                )),
            )
            .await;
            assert!(
                matches!(
                    outcome,
                    ToolExecutionOutcome::Completed {
                        source: ToolOutcomeSource::Executed,
                        disposition: ToolResultDisposition::Failed,
                        ref content,
                        ..
                    } if content.parts().len() == 1
                        && content.parts()[0].as_text() == FETCH_FAILED_TEXT
                ),
                "non-2xx case #{index} must settle the frozen could-not-fetch result"
            );
            let disclosed = outcome_content(&outcome).parts()[0].as_text();
            assert!(
                !disclosed.contains("SECRET")
                    && !disclosed.contains("404")
                    && !disclosed.contains("secret-evil"),
                "case #{index} must never disclose body/status/header content"
            );
            server.wait_finished().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_65536_byte_boundary_succeeds_and_65537_is_too_large() {
        // Exactly 65,536 bytes (declared and streamed) succeeds as one Text part.
        let boundary = "x".repeat(MAX_RESPONSE_BYTES);
        let mut server = TestLoopbackServer::spawn_with(ServerScript::Respond {
            head: ok_head("Content-Type: text/plain\r\n", boundary.len()),
            body: boundary.as_bytes().to_vec(),
        });
        let set = build_tool_set(Arc::new(loopback_resources(&server)));
        let outcome = execute(
            set,
            request_for(&format!(
                r#"{{"url":"{}"}}"#,
                loopback_url(&server, "boundary")
            )),
        )
        .await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
                ..
            } if content.parts().len() == 1 && content.parts()[0].as_text() == boundary
        ));
        server.wait_finished().await;

        // 65,537 bytes without a known Content-Length (chunked framing) is stopped at the
        // bound while streaming: the frozen too-large text.
        let oversized = "y".repeat(MAX_READ_BYTES);
        let mut server = TestLoopbackServer::spawn_with(ServerScript::Respond {
            head: chunked_head("Content-Type: text/plain\r\n"),
            body: chunked_body(oversized.as_bytes()),
        });
        let set = build_tool_set(Arc::new(loopback_resources(&server)));
        let outcome = execute(
            set,
            request_for(&format!(
                r#"{{"url":"{}"}}"#,
                loopback_url(&server, "oversized-stream")
            )),
        )
        .await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                ref content,
                ..
            } if content.parts().len() == 1 && content.parts()[0].as_text() == TOO_LARGE_TEXT
        ));
        server.wait_finished().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_known_content_length_above_the_bound_is_rejected_before_any_streaming() {
        // The server declares Content-Length: 65537 and then sends zero body bytes while
        // keeping the connection open: the executor settles the frozen too-large result
        // from the headers alone.  If it tried to stream, it would wait forever for body
        // bytes that never arrive, so the prompt settlement itself is the proof of the
        // reject-before-streaming rule (deterministic, no timeout).
        let mut server = TestLoopbackServer::spawn_with(ServerScript::RespondThenStall {
            head: ok_head("Content-Type: text/plain\r\n", MAX_READ_BYTES),
            body_prefix: Vec::new(),
        });
        let set = build_tool_set(Arc::new(loopback_resources(&server)));
        let outcome = execute(
            set,
            request_for(&format!(
                r#"{{"url":"{}"}}"#,
                loopback_url(&server, "known-oversize")
            )),
        )
        .await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                ref content,
                ..
            } if content.parts().len() == 1 && content.parts()[0].as_text() == TOO_LARGE_TEXT
        ));
        server.release();
        server.wait_finished().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_utf8_and_unsafe_text_settle_the_frozen_not_valid_text() {
        let cases: &[&[u8]] = &[&[0xc3, 0x28, 0xff, 0xfe], b"control \x01 byte".as_slice()];
        for (index, body) in cases.iter().enumerate() {
            let mut server = TestLoopbackServer::spawn_with(ServerScript::Respond {
                head: ok_head("Content-Type: text/plain\r\n", body.len()),
                body: body.to_vec(),
            });
            let set = build_tool_set(Arc::new(loopback_resources(&server)));
            let outcome = execute(
                set,
                request_for(&format!(r#"{{"url":"{}"}}"#, loopback_url(&server, "text"))),
            )
            .await;
            assert!(
                matches!(
                    outcome,
                    ToolExecutionOutcome::Completed {
                        source: ToolOutcomeSource::Executed,
                        disposition: ToolResultDisposition::Failed,
                        ref content,
                        ..
                    } if content.parts().len() == 1
                        && content.parts()[0].as_text() == NOT_VALID_TEXT
                ),
                "body case #{index} must settle the frozen not-valid-text result"
            );
            server.wait_finished().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_early_fin_body_stream_error_settles_the_frozen_could_not_fetch_text() {
        // The server declares Content-Length: 100, writes 10 bytes, and closes: the body
        // stream errors mid-read and settles the frozen could-not-fetch text.
        let mut server = TestLoopbackServer::spawn_with(ServerScript::Abort {
            head: ok_head("Content-Type: text/plain\r\n", 100),
            body_prefix: b"partial body".to_vec(),
        });
        let set = build_tool_set(Arc::new(loopback_resources(&server)));
        let outcome = execute(
            set,
            request_for(&format!(
                r#"{{"url":"{}"}}"#,
                loopback_url(&server, "abort")
            )),
        )
        .await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                ref content,
                ..
            } if content.parts().len() == 1 && content.parts()[0].as_text() == FETCH_FAILED_TEXT
        ));
        server.wait_finished().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_raw_control_byte_in_a_response_header_maps_to_could_not_fetch() {
        // The server writes a header value containing a raw backspace byte (0x08):
        // hyper's HTTP/1 parser rejects the invalid header value while parsing the
        // response head — before reqwest ever builds a Response — so the failure
        // surfaces from the send/stream and maps to the frozen could-not-fetch text
        // (transport), never to the Content-Type validator (which only ever sees
        // parsed, valid header values).
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=ut\u{8}-8\r\nContent-Length: 4\r\nConnection: close\r\n\r\n"
            .to_owned();
        let mut server = TestLoopbackServer::spawn_with(ServerScript::Respond {
            head,
            body: b"body".to_vec(),
        });
        let set = build_tool_set(Arc::new(loopback_resources(&server)));
        let outcome = execute(
            set,
            request_for(&format!(
                r#"{{"url":"{}"}}"#,
                loopback_url(&server, "bad-header")
            )),
        )
        .await;
        assert!(
            matches!(
                outcome,
                ToolExecutionOutcome::Completed {
                    source: ToolOutcomeSource::Executed,
                    disposition: ToolResultDisposition::Failed,
                    ref content,
                    ..
                } if content.parts().len() == 1
                    && content.parts()[0].as_text() == FETCH_FAILED_TEXT
            ),
            "a raw control byte in a response header is a transport failure: {outcome:?}"
        );
        server.wait_finished().await;
    }

    // ---------- Cancellation and lifecycle ----------

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_before_the_send_is_polled_proves_zero_get() {
        let server = TestLoopbackServer::spawn();
        let set = build_tool_set(Arc::new(loopback_resources(&server)));
        let request = request_for(&format!(
            r#"{{"url":"{}"}}"#,
            loopback_url(&server, "never-sent")
        ));

        let start = plan_start(&set, &request);
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        // The operation's own pair is already cancelled before the executor future is
        // ever constructed: the biased select must win with the exact frozen Cancelled
        // text without ever polling the operation future, so zero GET is provable.
        let (handle, observer) = ToolCancellationHandle::new();
        handle.cancel();
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");
        let outcome = tokio::spawn(run).await.expect("the run settles");

        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Cancelled,
                ref content,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
                && content.parts().len() == 1
                && content.parts()[0].as_text() == FETCH_CANCELLED_TEXT
        ));
        assert!(
            server.captured().is_empty(),
            "a pre-cancelled token must never poll the operation: zero GET reaches the wire"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_mid_headers_drops_the_exact_send_state_and_closes_the_connection() {
        // The server captures the request and then writes nothing until released: the
        // executor is mid-send (headers never arrive) when the cancellation arrives.
        let mut server = TestLoopbackServer::spawn_with(ServerScript::Stall);
        let set = build_tool_set(Arc::new(loopback_resources(&server)));
        let request = request_for(&format!(
            r#"{{"url":"{}"}}"#,
            loopback_url(&server, "stall")
        ));
        let start = plan_start(&set, &request);
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (handle, observer) = ToolCancellationHandle::new();
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");
        let mut run = Box::pin(run);

        // Deterministic handshake: poll the run until the server's request-captured
        // event fires (the connection is established and the send awaits response
        // headers; the server wrote nothing yet, so the executor cannot settle).
        await_server_event(
            &mut run,
            server.capture_event(),
            "the server captured the request",
        )
        .await;
        assert_eq!(server.captured().len(), 1);
        handle.cancel();
        let outcome = run.await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Cancelled,
                ref content,
                ..
            } if content.parts().len() == 1 && content.parts()[0].as_text() == FETCH_CANCELLED_TEXT
        ));

        // Cleanup evidence is the positive EOF/closed event: releasing the stall lets
        // the server observe the peer.  The dropped send future tears down hyper's
        // connection task (Connection: close plus the zero idle pool forbid handing it
        // to a later call), so the server must observe the client closed the
        // connection, and the finished event only resolves after that observation.  The
        // guardrail timeout is a failure fence only — it never supplies evidence.
        server.release();
        server.wait_finished().await;
        assert!(
            server.eof_observed(),
            "the cancelled send must close the connection"
        );
        assert_eq!(
            server.captured().len(),
            1,
            "no second request may ever be sent"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_mid_body_drops_the_response_stream_and_closes_the_connection() {
        // The server writes the full headers plus a 10-byte body prefix and then stalls:
        // the body-prefix-written event proves the response head and the first body
        // bytes are on the wire, so the cancellation arrives while the executor is
        // mid-operation — still awaiting the response head or inside the bounded body
        // drain — and it can never settle (the declared body is incomplete and the
        // connection stays open until released).
        let mut server = TestLoopbackServer::spawn_with(ServerScript::RespondThenStall {
            head: ok_head("Content-Type: text/plain\r\n", 100),
            body_prefix: b"0123456789".to_vec(),
        });
        let set = build_tool_set(Arc::new(loopback_resources(&server)));
        let request = request_for(&format!(
            r#"{{"url":"{}"}}"#,
            loopback_url(&server, "stall-body")
        ));
        let start = plan_start(&set, &request);
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (handle, observer) = ToolCancellationHandle::new();
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");
        let mut run = Box::pin(run);

        // Deterministic handshake: the request-captured event proves the send is on the
        // wire, and the body-prefix-written event proves the response head and the first
        // body bytes are on the wire too.  The executor cannot settle before the events
        // arrive (the server writes nothing before capture, and the body stays
        // incomplete), so the events pin the cancellation to the mid-operation phase.
        await_server_event(
            &mut run,
            server.capture_event(),
            "the server captured the request",
        )
        .await;
        assert_eq!(server.captured().len(), 1);
        await_server_event(
            &mut run,
            server.body_prefix_event(),
            "the server wrote the body prefix",
        )
        .await;
        handle.cancel();
        let outcome = run.await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Cancelled,
                ref content,
                ..
            } if content.parts().len() == 1 && content.parts()[0].as_text() == FETCH_CANCELLED_TEXT
        ));

        // Cleanup evidence is the positive EOF/closed event, exactly as in the
        // mid-headers test: releasing the stall lets the server observe the peer, and
        // the finished event only resolves after the dropped response stream closed the
        // connection.
        server.release();
        server.wait_finished().await;
        assert!(
            server.eof_observed(),
            "the dropped response stream must close the connection"
        );
        assert_eq!(
            server.captured().len(),
            1,
            "no second request may ever be sent"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_natural_result_is_never_rewritten_by_a_late_cancellation() {
        // The scripted body is exactly 16 bytes and the head declares Content-Length: 16,
        // so the natural completion reads the full declared body (a mismatched length
        // would surface as a body-stream error instead of the natural success).
        let mut server = TestLoopbackServer::spawn_with(ServerScript::Respond {
            head: ok_head("Content-Type: text/plain\r\n", 16),
            body: b"late-cancel-body".to_vec(),
        });
        let set = build_tool_set(Arc::new(loopback_resources(&server)));
        let request = request_for(&format!(
            r#"{{"url":"{}"}}"#,
            loopback_url(&server, "natural")
        ));
        let start = plan_start(&set, &request);
        let proof = ToolStartGate::new(request.clone())
            .reserve(&request)
            .expect("the exact request reserves its gate")
            .start()
            .expect("the reserved gate starts");
        let (handle, observer) = ToolCancellationHandle::new();
        let run = set
            .run_started_execution(&request, proof, start, observer)
            .expect("the exact proof revalidates and the factory constructs the run");
        let run = Box::pin(run);

        // Drive the natural completion to its terminal outcome first, then cancel: the
        // mapped natural result must stay untouched by the late cancellation.
        let outcome = run.await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
                ..
            } if content.parts().len() == 1 && content.parts()[0].as_text() == "late-cancel-body"
        ));
        handle.cancel();
        assert!(
            matches!(
                outcome,
                ToolExecutionOutcome::Completed {
                    disposition: ToolResultDisposition::Succeeded,
                    ref content,
                    ..
                } if content.parts().len() == 1
                    && content.parts()[0].as_text() == "late-cancel-body"
            ),
            "the late cancellation must never rewrite the mapped natural result"
        );
        server.wait_finished().await;
    }

    #[test]
    fn the_fixed_transport_timeouts_are_the_frozen_adr_0147_constants() {
        // The fixed 10s connect / 30s request timeouts are frozen constants (ADR 0147
        // decision 9).  The timeout behavior itself is not wall-clock-tested: a
        // transport failure already has focused wire evidence in
        // `transport_error_before_any_server_contact_maps_to_could_not_fetch`, and a
        // paused tokio clock cannot drive hyper's real socket/timer machinery
        // deterministically.
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(30));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transport_error_before_any_server_contact_maps_to_could_not_fetch() {
        // A pinned client whose address has nothing listening fails at connect: the
        // transport error maps to the frozen could-not-fetch text, and no retry happens
        // (exactly one refused connection attempt).
        let resources =
            FetchUrlResources::loopback(LOOPBACK_HOST, SocketAddr::new(v4(127, 0, 0, 1), 1))
                .expect("the loopback authority materializes");
        let set = build_tool_set(Arc::new(resources));
        let request = request_for(&format!(r#"{{"url":"http://{LOOPBACK_HOST}:1/refused"}}"#));
        let outcome = execute(set, request).await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::Executed,
                disposition: ToolResultDisposition::Failed,
                ref content,
                ..
            } if content.parts().len() == 1 && content.parts()[0].as_text() == FETCH_FAILED_TEXT
        ));
    }

    /// One captured client request line plus lowercased headers.
    #[derive(Clone, Debug)]
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
    }

    /// One scripted loopback response/behavior for one accepted connection.  The
    /// response bytes are fixed at spawn; the only dynamic control is the stall release
    /// (see [`TestLoopbackServer::release`]) and the positive tokio events the server
    /// emits (capture, body prefix, finished).
    enum ServerScript {
        /// Write the exact response head and body, then close the write side.
        Respond { head: String, body: Vec<u8> },
        /// Write the exact head and the first body bytes, signal the body-prefix-written
        /// event, then wait for a release before closing without writing the rest: the
        /// connection stays open with an incomplete body (mid-body cancellation and
        /// known-oversize rejection observe this).
        RespondThenStall { head: String, body_prefix: Vec<u8> },
        /// Write nothing; wait for a release before closing: the response headers never
        /// arrive (mid-headers cancellation observes this).
        Stall,
        /// Write the exact head and the first body bytes, then close immediately without
        /// the declared full body: an early-fin body-stream error.
        Abort { head: String, body_prefix: Vec<u8> },
    }

    /// Minimal deterministic single-request loopback server, owned by this module's tests
    /// (Tools cannot depend on the provider-owned loopback parser): it accepts one
    /// connection, captures the exact request head, executes the fixed script, and emits
    /// the positive tokio events the tests await — the request-captured event, the
    /// body-prefix-written event, and the finished event, which for the stall phases only
    /// resolves after the server observed whether the client closed the connection
    /// (`Connection: close` plus the zero idle pool forbid handing the connection to a
    /// later call).
    struct TestLoopbackServer {
        addr: SocketAddr,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        eof_observed: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
        release: Option<std::sync::mpsc::Sender<()>>,
        capture_event: Option<tokio::sync::oneshot::Receiver<()>>,
        body_prefix_event: Option<tokio::sync::oneshot::Receiver<()>>,
        finished: Option<tokio::sync::oneshot::Receiver<()>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    /// Failure fences only: a regression that never closes the connection must fail
    /// instead of hanging.  They never supply evidence — the positive EOF/reset and
    /// finished events do.
    const CONNECTION_CLOSE_GUARDRAIL: Duration = Duration::from_secs(2);
    const FINISHED_EVENT_GUARDRAIL: Duration = Duration::from_secs(3);
    const POISON_BYTE: u8 = 0;

    impl TestLoopbackServer {
        fn spawn() -> Self {
            Self::spawn_with(ServerScript::Respond {
                head: "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n"
                    .to_owned(),
                body: b"ok".to_vec(),
            })
        }

        fn spawn_with(script: ServerScript) -> Self {
            use std::io::{Read, Write};

            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
            let addr = listener.local_addr().expect("loopback address");
            let captured = Arc::new(Mutex::new(Vec::new()));
            let eof_observed = Arc::new(AtomicBool::new(false));
            let shutdown = Arc::new(AtomicBool::new(false));
            let (release_sender, release_receiver) = std::sync::mpsc::channel::<()>();
            let (capture_sender, capture_receiver) = tokio::sync::oneshot::channel::<()>();
            let (prefix_sender, prefix_receiver) = tokio::sync::oneshot::channel::<()>();
            let (finished_sender, finished_receiver) = tokio::sync::oneshot::channel::<()>();
            let thread_captured = Arc::clone(&captured);
            let thread_eof = Arc::clone(&eof_observed);
            let handle = thread::spawn(move || {
                let Ok((mut stream, _peer)) = listener.accept() else {
                    let _ = finished_sender.send(());
                    return;
                };
                let mut first = [0u8; 1];
                if stream.read_exact(&mut first).is_err() || first[0] == POISON_BYTE {
                    let _ = finished_sender.send(());
                    return;
                }
                let mut buf = vec![first[0]];
                let mut scratch = [0u8; 4096];
                let header_end = loop {
                    if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
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
                        let (name, value) = line.split_once(':').expect("header name colon");
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
                // Arm the failure guardrail while the peer is known live and before
                // publishing the capture event that allows cancellation.  On macOS,
                // setting SO_RCVTIMEO after the peer has already reset can return
                // EINVAL even though that reset is the positive close evidence this
                // test is about.
                stream
                    .set_read_timeout(Some(CONNECTION_CLOSE_GUARDRAIL))
                    .expect("connection-close guardrail");
                let _ = capture_sender.send(());
                match script {
                    ServerScript::Respond { head, body } => {
                        stream
                            .write_all(head.as_bytes())
                            .expect("write scripted head");
                        stream.write_all(&body).expect("write scripted body");
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                    }
                    ServerScript::RespondThenStall { head, body_prefix } => {
                        stream
                            .write_all(head.as_bytes())
                            .expect("write scripted head");
                        stream
                            .write_all(&body_prefix)
                            .expect("write scripted body prefix");
                        let _ = prefix_sender.send(());
                        let _ = release_receiver.recv();
                        if observe_peer_close(&mut stream) {
                            thread_eof.store(true, Ordering::SeqCst);
                        }
                    }
                    ServerScript::Stall => {
                        let _ = release_receiver.recv();
                        if observe_peer_close(&mut stream) {
                            thread_eof.store(true, Ordering::SeqCst);
                        }
                    }
                    ServerScript::Abort { head, body_prefix } => {
                        stream
                            .write_all(head.as_bytes())
                            .expect("write scripted head");
                        stream
                            .write_all(&body_prefix)
                            .expect("write scripted body prefix");
                        let _ = stream.shutdown(std::net::Shutdown::Write);
                    }
                }
                let _ = finished_sender.send(());
            });
            Self {
                addr,
                captured,
                eof_observed,
                shutdown,
                release: Some(release_sender),
                capture_event: Some(capture_receiver),
                body_prefix_event: Some(prefix_receiver),
                finished: Some(finished_receiver),
                handle: Some(handle),
            }
        }

        fn addr(&self) -> SocketAddr {
            self.addr
        }

        fn captured(&self) -> Vec<CapturedRequest> {
            self.captured
                .lock()
                .expect("captured requests mutex is not poisoned")
                .clone()
        }

        /// Whether the stall-phase observation saw the client close the connection.
        fn eof_observed(&self) -> bool {
            self.eof_observed.load(Ordering::SeqCst)
        }

        /// Releases a stall-phase server: the server then observes the client connection
        /// (a closed connection records `eof_observed`) and finishes.
        fn release(&self) {
            if let Some(release) = &self.release {
                let _ = release.send(());
            }
        }

        /// The positive request-captured event: resolves when the server thread has
        /// captured the exact request head (consumed at most once per server).
        async fn capture_event(&mut self) {
            self.capture_event
                .take()
                .expect("the capture event is consumed once")
                .await
                .expect("the server captured the request");
        }

        /// The positive body-prefix-written event: resolves when the server thread has
        /// written the response head and the scripted body prefix (consumed at most once
        /// per server).
        async fn body_prefix_event(&mut self) {
            self.body_prefix_event
                .take()
                .expect("the body-prefix event is consumed once")
                .await
                .expect("the server wrote the body prefix");
        }

        /// The positive finished event: resolves only after the server thread executed
        /// its script and, for the stall phases, observed whether the client closed the
        /// connection.  The guardrail timeout is a failure fence only — the event itself
        /// is the evidence, and a regression that never closes the connection fails the
        /// test instead of hanging it (the shutdown flag keeps the server joinable).
        async fn wait_finished(&mut self) {
            let finished = self
                .finished
                .take()
                .expect("the finished event is consumed once");
            tokio::time::timeout(FINISHED_EVENT_GUARDRAIL, finished)
                .await
                .expect("failure fence: the scripted server never finished")
                .expect("the scripted server finished");
        }
    }

    impl Drop for TestLoopbackServer {
        fn drop(&mut self) {
            // Always stop and join the server so no thread outlives the test.  Release
            // any stall first, then poison a still-blocked accept with one non-HTTP byte.
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(release) = self.release.take() {
                let _ = release.send(());
            }
            if let Some(handle) = self.handle.take() {
                if let Ok(mut stream) = std::net::TcpStream::connect(self.addr) {
                    use std::io::Write;
                    let _ = stream.write_all(&[POISON_BYTE]);
                }
                let _ = handle.join();
            }
        }
    }

    /// Reads the peer connection to completion and reports positive close evidence: a
    /// clean FIN or a connection-reset/aborted error.  The read timeout is only a failure
    /// guardrail; it never counts as close evidence.
    fn observe_peer_close(stream: &mut std::net::TcpStream) -> bool {
        let mut one = [0u8; 1];
        loop {
            match stream.read(&mut one) {
                Ok(0) => return true,
                Ok(_) => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::NotConnected
                            | std::io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    return true;
                }
                Err(_) => return false,
            }
        }
    }
}
