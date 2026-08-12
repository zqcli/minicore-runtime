//! The `fetch_url` builtin's host-authority slice (ADR 0147).
//!
//! This slice delivers exactly the resource-level authority half of the closed
//! `fetch_url` builtin, with no Tool definition and no executor yet:
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
//! `reqwest::Url` or `reqwest::Client` (ADR 0147 decision 8).
//!
//! The executor slice (Tool definition, planner, start factory, bounded text response
//! policy) and the Runtime config wiring are later slices; this module deliberately
//! discloses no Tool.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::dns::{Name, Resolve, Resolving};
use thiserror::Error;

use crate::wire::lexical::validate_safe_text;

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
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by the adjacent fetch_url executor/Runtime wiring slice"
    )
)]
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
/// An empty set is representable (the future origin-only-undisclosed config slice);
/// duplicate canonical origins fail closed at materialization.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by the adjacent fetch_url executor/Runtime wiring slice and Tools wire tests"
    )
)]
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
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "consumed by the adjacent fetch_url executor/Runtime wiring slice"
        )
    )]
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
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "consumed by the adjacent fetch_url executor/Runtime wiring slice"
        )
    )]
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
    /// it, and it is not an HTTP fallback.
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
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by the adjacent fetch_url executor/Runtime wiring slice"
    )
)]
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
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by the adjacent fetch_url executor/Runtime wiring slice"
    )
)]
pub(crate) struct AuthorizedFetchUrl {
    url: reqwest::Url,
    client: reqwest::Client,
}

#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by the adjacent fetch_url executor/Runtime wiring slice"
    )
)]
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
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use futures_util::FutureExt;

    use super::*;

    const LOOPBACK_HOST: &str = "fetch-url.loopback.test";

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

    /// Minimal deterministic single-request loopback server, owned by this module's
    /// tests (Tools cannot depend on the provider-owned loopback parser): it accepts
    /// one connection, captures the exact request head, and answers `200 ok` with
    /// `Connection: close`.
    struct TestLoopbackServer {
        addr: SocketAddr,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestLoopbackServer {
        fn spawn() -> Self {
            use std::io::{Read, Write};

            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
            listener.set_nonblocking(true).expect("nonblocking accept");
            let addr = listener.local_addr().expect("loopback address");
            let captured = Arc::new(Mutex::new(Vec::new()));
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread_captured = Arc::clone(&captured);
            let thread_shutdown = Arc::clone(&shutdown);
            let handle = thread::spawn(move || {
                let mut buf = Vec::new();
                let mut scratch = [0u8; 4096];
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
                            let body = "ok";
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            stream
                                .write_all(response.as_bytes())
                                .expect("write scripted response");
                            let _ = stream.shutdown(std::net::Shutdown::Write);
                            return;
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

        fn captured(&self) -> Vec<CapturedRequest> {
            self.captured
                .lock()
                .expect("captured requests mutex is not poisoned")
                .clone()
        }
    }

    impl Drop for TestLoopbackServer {
        fn drop(&mut self) {
            // Always stop and join the accept loop so no thread outlives the test.
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
}
