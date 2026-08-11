//! M12 provider-gate error-mapping matrix: structural and invariant tests.
//!
//! Consumes only the checked-in fixture `docs/fixtures/provider-gate-m12/error-mapping-v1.json`
//! (no production code is exercised). Pins the closed taxonomies (stage / reason / delivery /
//! policy / normalizedReason), the retry and compaction policy rules, the
//! `ModelCallError::from_provider` normalization contract, and the conservative provider
//! facts, so the matrix cannot drift silently.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

const FIXTURE: &str = include_str!("../docs/fixtures/provider-gate-m12/error-mapping-v1.json");

#[rustfmt::skip]
const PROTOCOLS: [&str; 2] = ["openai_responses", "anthropic_messages"];
#[rustfmt::skip]
const STAGES: [&str; 4] = ["connect", "http_response", "completed_response", "stream"];
#[rustfmt::skip]
const REASONS: [&str; 9] = ["transport_unavailable", "context_overflow", "invalid_request", "auth_rejected", "rate_limited", "quota_exceeded", "provider_unavailable", "invalid_provider_response", "timeout"];
#[rustfmt::skip]
const DELIVERIES: [&str; 5] = ["not_sent", "rejected_before_execution", "accepted_no_output", "output_started", "unknown"];
#[rustfmt::skip]
const POLICIES: [&str; 3] = ["logical_retry", "compaction", "terminal"];
#[rustfmt::skip]
const NORMALIZED_REASONS: [&str; 11] = ["transport_unavailable", "context_overflow", "invalid_request", "auth_rejected", "rate_limited", "quota_exceeded", "provider_unavailable", "invalid_provider_response", "timeout", "request_outcome_unknown", "stream_interrupted"];
#[rustfmt::skip]
const TRANSIENT_REASONS: [&str; 4] = ["timeout", "transport_unavailable", "provider_unavailable", "rate_limited"];

/// Field sets fixed by the fixture schema.
#[rustfmt::skip]
const CASE_FIELDS: [&str; 5] = ["id", "protocol", "category", "observation", "expected"];
#[rustfmt::skip]
const OBS_FIELDS: [&str; 7] = ["stage", "httpStatus", "errorType", "errorCode", "semanticOutputStarted", "terminalObserved", "retryAfterSeconds"];
#[rustfmt::skip]
const EXP_FIELDS: [&str; 4] = ["reason", "delivery", "normalizedReason", "policy"];
#[rustfmt::skip]
const OPENAI_CATEGORIES: [&str; 10] = ["transport", "invalid_request", "auth", "rate_limit", "provider_unavailable", "malformed_response", "early_eof", "context_overflow", "quota", "stream_error"];
#[rustfmt::skip]
const ANTHROPIC_CATEGORIES: [&str; 10] = ["transport", "invalid_request", "auth", "rate_limit", "provider_unavailable", "malformed_response", "early_eof", "quota", "timeout", "stream_error"];

fn matrix() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture must parse as JSON")
}

fn cases(m: &Value) -> &Vec<Value> {
    m["cases"].as_array().expect("\"cases\" must be an array")
}

fn by_id<'a>(cs: &'a [Value], wanted: &str) -> &'a Value {
    cs.iter()
        .find(|c| c["id"].as_str() == Some(wanted))
        .unwrap_or_else(|| panic!("case {wanted} is missing from the matrix"))
}

fn id(c: &Value) -> &str {
    c["id"].as_str().expect("case \"id\" must be a string")
}

fn strf<'a>(c: &'a Value, f: &str) -> &'a str {
    c[f].as_str()
        .unwrap_or_else(|| panic!("{f} must be a string"))
}

fn boolf(c: &Value, f: &str) -> bool {
    c[f].as_bool().unwrap_or_else(|| panic!("{f} not boolean"))
}

fn obs(c: &Value) -> &Value {
    &c["observation"]
}

fn exp(c: &Value) -> &Value {
    &c["expected"]
}

fn retry(c: &Value) -> Option<u64> {
    obs(c)["retryAfterSeconds"].as_u64()
}

fn reason(c: &Value) -> &str {
    strf(exp(c), "reason")
}
fn delivery(c: &Value) -> &str {
    strf(exp(c), "delivery")
}
fn policy(c: &Value) -> &str {
    strf(exp(c), "policy")
}
fn normalized(c: &Value) -> &str {
    strf(exp(c), "normalizedReason")
}

fn exact_keys(v: &Value, want: &[&str], what: &str) {
    let mut got: Vec<String> = v
        .as_object()
        .unwrap_or_else(|| panic!("{what} must be an object"))
        .keys()
        .cloned()
        .collect();
    got.sort();
    let mut want: Vec<&str> = want.to_vec();
    want.sort();
    assert_eq!(got, want, "{what} must have exactly the documented fields");
}

/// Classification is structural, never a string match on human-facing error
/// text: no key mentioning "message" or "raw" may appear anywhere in a case.
fn no_message_or_raw_keys(v: &Value, case_id: &str) {
    if let Value::Object(map) = v {
        for (key, sub) in map {
            let key = key.to_ascii_lowercase();
            assert!(
                !key.contains("message") && !key.contains("raw"),
                "case {case_id}: no message/raw fields"
            );
            no_message_or_raw_keys(sub, case_id);
        }
    }
    if let Value::Array(items) = v {
        for sub in items {
            no_message_or_raw_keys(sub, case_id);
        }
    }
}

/// Mirrors `ModelCallError::from_provider` (src/model_gateway.rs): transient
/// reasons fold AcceptedNoOutput/Unknown into RequestOutcomeUnknown and
/// OutputStarted into StreamInterrupted; every other combination preserves the
/// reason.
fn normalize<'a>(reason: &'a str, delivery: &str) -> &'a str {
    if !TRANSIENT_REASONS.contains(&reason) {
        return reason;
    }
    match delivery {
        "accepted_no_output" | "unknown" => "request_outcome_unknown",
        "output_started" => "stream_interrupted",
        _ => reason,
    }
}

/// Documented `logical_retry` preconditions: safe delivery, a transient reason,
/// and any `rate_limited` hint at most 60s.
fn logical_retry_allowed(reason: &str, delivery: &str, retry: Option<u64>) -> bool {
    let safe = matches!(delivery, "not_sent" | "rejected_before_execution");
    let hint_ok = reason != "rate_limited" || retry.is_some_and(|s| s <= 60);
    TRANSIENT_REASONS.contains(&reason) && safe && hint_ok
}

#[test]
fn shape_and_taxonomies() {
    let m = matrix();
    assert_eq!(m["version"].as_u64(), Some(1), "fixture version must be 1");
    let protos: Vec<&str> = m["protocols"]
        .as_array()
        .expect("\"protocols\" must be an array")
        .iter()
        .map(|p| p.as_str().expect("protocol name must be a string"))
        .collect();
    assert_eq!(
        protos, PROTOCOLS,
        "protocols must be exactly the two documented ones"
    );
    let cs = cases(&m);
    assert_eq!(cs.len(), 26, "matrix must contain exactly 26 cases");

    let mut seen = HashSet::new();
    let mut per_protocol: HashMap<&str, usize> = HashMap::new();
    for c in cs {
        exact_keys(c, &CASE_FIELDS, "case");
        let case_id = id(c);
        assert!(seen.insert(case_id), "case id {case_id} is duplicated");
        let p = strf(c, "protocol");
        assert!(
            PROTOCOLS.contains(&p),
            "case {case_id}: unknown protocol {p}"
        );
        *per_protocol.entry(p).or_default() += 1;
        let id_prefix = match p {
            "openai_responses" => "openai",
            "anthropic_messages" => "anthropic",
            _ => unreachable!(),
        };
        assert!(
            case_id.starts_with(id_prefix),
            "case {case_id}: id must use its protocol prefix"
        );
        assert!(
            !strf(c, "category").is_empty(),
            "case {case_id}: empty category"
        );

        let o = obs(c);
        exact_keys(o, &OBS_FIELDS, "observation");
        assert!(
            STAGES.contains(&strf(o, "stage")),
            "case {case_id}: unknown stage"
        );
        for f in ["httpStatus", "retryAfterSeconds"] {
            assert!(
                o[f].is_null() || o[f].is_number(),
                "case {case_id}: {f} type"
            );
        }
        for f in ["errorType", "errorCode"] {
            assert!(
                o[f].is_null() || o[f].is_string(),
                "case {case_id}: {f} type"
            );
        }
        for f in ["semanticOutputStarted", "terminalObserved"] {
            assert!(o[f].is_boolean(), "case {case_id}: {f} type");
        }

        let e = exp(c);
        exact_keys(e, &EXP_FIELDS, "expected");
        assert!(
            REASONS.contains(&strf(e, "reason")),
            "case {case_id}: unknown reason"
        );
        assert!(
            DELIVERIES.contains(&strf(e, "delivery")),
            "case {case_id}: unknown delivery"
        );
        assert!(
            POLICIES.contains(&strf(e, "policy")),
            "case {case_id}: unknown policy"
        );
        assert!(
            NORMALIZED_REASONS.contains(&strf(e, "normalizedReason")),
            "case {case_id}: unknown normalizedReason"
        );
        no_message_or_raw_keys(c, case_id);
    }
    assert_eq!(per_protocol.len(), 2, "both protocols must have cases");
    for (p, n) in &per_protocol {
        assert_eq!(*n, 13, "protocol {p} must have exactly 13 cases");
    }
}

#[test]
fn stage_invariants() {
    for c in cases(&matrix()) {
        let o = obs(c);
        let null_status = o["httpStatus"].is_null();
        assert_eq!(null_status, strf(o, "stage") == "connect", "case {}", id(c));
        if strf(o, "stage") == "http_response" {
            assert!(boolf(o, "terminalObserved"), "case {}", id(c));
            assert!(!null_status, "case {}", id(c));
        }
        if let Some(secs) = retry(c) {
            assert!(
                matches!(reason(c), "rate_limited" | "provider_unavailable"),
                "case {}",
                id(c)
            );
            assert!(secs > 0, "case {}", id(c));
        }
    }
}

#[test]
fn policy_rules() {
    for c in cases(&matrix()) {
        let r = reason(c);
        let d = delivery(c);
        let p = policy(c);
        let rty = retry(c);
        assert_eq!(
            p == "logical_retry",
            logical_retry_allowed(r, d, rty),
            "case {}: logical_retry preconditions (reason={r}, delivery={d}, retry={rty:?})",
            id(c)
        );
        if r == "rate_limited" {
            let secs = rty.unwrap_or_else(|| {
                panic!("case {}: rate_limited requires retryAfterSeconds", id(c))
            });
            assert_eq!(
                p,
                if secs <= 60 {
                    "logical_retry"
                } else {
                    "terminal"
                },
                "case {}: rate hint policy",
                id(c)
            );
            assert_eq!(
                d,
                "rejected_before_execution",
                "case {}: rate_limited delivery",
                id(c)
            );
        }
        assert_eq!(
            p == "compaction",
            r == "context_overflow",
            "case {}: compaction only for context_overflow",
            id(c)
        );
        if r == "context_overflow" {
            assert_eq!(d, "rejected_before_execution", "case {}", id(c));
            assert_eq!(normalized(c), "context_overflow", "case {}", id(c));
            assert!(boolf(obs(c), "terminalObserved"), "case {}", id(c));
        }
    }
}

#[test]
fn normalization_contract() {
    for c in cases(&matrix()) {
        assert_eq!(
            normalized(c),
            normalize(reason(c), delivery(c)),
            "case {}",
            id(c)
        );
    }
}

#[test]
fn semantic_output_and_terminal_consistency() {
    for c in cases(&matrix()) {
        let o = obs(c);
        assert_eq!(
            delivery(c) == "output_started",
            boolf(o, "semanticOutputStarted"),
            "case {}: semanticOutputStarted and output_started must agree",
            id(c)
        );
        if !boolf(o, "terminalObserved") {
            assert_ne!(delivery(c), "rejected_before_execution", "case {}", id(c));
        }
    }
}

#[test]
fn conservative_provider_facts() {
    let m = matrix();
    let cs = cases(&m);
    // OpenAI 500/503 and Anthropic 500/504: outcome unknown + terminal, never retried.
    for (case_id, expected_reason) in [
        ("openai_internal_server_error", "provider_unavailable"),
        ("openai_service_unavailable", "provider_unavailable"),
        ("anthropic_internal_server_error", "provider_unavailable"),
        ("anthropic_processing_timeout", "timeout"),
    ] {
        let c = by_id(cs, case_id);
        assert_eq!(reason(c), expected_reason, "{case_id}");
        assert_eq!(delivery(c), "unknown", "{case_id}");
        assert_eq!(normalized(c), "request_outcome_unknown", "{case_id}");
        assert_eq!(policy(c), "terminal", "{case_id}");
        assert!(boolf(obs(c), "terminalObserved"), "{case_id}");
    }
    // Anthropic 529 typed overloaded: rejected_before_execution + logical_retry.
    let c = by_id(cs, "anthropic_overloaded");
    assert_eq!(delivery(c), "rejected_before_execution");
    assert_eq!(policy(c), "logical_retry");
    assert_eq!(normalized(c), "provider_unavailable");
    assert_eq!(retry(c), Some(3));
    // OpenAI context_length_exceeded alone => context_overflow + compaction.
    let c = by_id(cs, "openai_context_length_exceeded");
    assert_eq!(reason(c), "context_overflow");
    assert_eq!(normalized(c), "context_overflow");
    assert_eq!(policy(c), "compaction");
    // Anthropic invalid_request stays invalid_request (no human-message parsing).
    let c = by_id(cs, "anthropic_invalid_request");
    assert_eq!(reason(c), "invalid_request");
    assert_eq!(normalized(c), "invalid_request");
    assert_eq!(delivery(c), "rejected_before_execution");
    assert_eq!(policy(c), "terminal");
    // Early EOF: before output => accepted_no_output / request_outcome_unknown,
    // after output => output_started / stream_interrupted, for both protocols.
    for (case_id, d, n) in [
        (
            "openai_early_eof_before_output",
            "accepted_no_output",
            "request_outcome_unknown",
        ),
        (
            "openai_early_eof_after_output",
            "output_started",
            "stream_interrupted",
        ),
        (
            "anthropic_early_eof_before_output",
            "accepted_no_output",
            "request_outcome_unknown",
        ),
        (
            "anthropic_early_eof_after_output",
            "output_started",
            "stream_interrupted",
        ),
    ] {
        let c = by_id(cs, case_id);
        assert_eq!(delivery(c), d, "{case_id}");
        assert_eq!(normalized(c), n, "{case_id}");
    }
    // Rate hints: 61s is terminal, <=60s is retried.
    let c = by_id(cs, "openai_rate_limit_hint_too_long");
    assert_eq!(retry(c), Some(61));
    assert_eq!(policy(c), "terminal");
    for case_id in [
        "openai_temporary_rate_limit",
        "anthropic_temporary_rate_limit",
    ] {
        let c = by_id(cs, case_id);
        assert!(
            retry(c).expect("rate hint must be present") <= 60,
            "{case_id}"
        );
        assert_eq!(policy(c), "logical_retry", "{case_id}");
    }
}

#[test]
fn category_coverage_per_protocol() {
    let m = matrix();
    for (p, required) in [
        ("openai_responses", OPENAI_CATEGORIES.as_slice()),
        ("anthropic_messages", ANTHROPIC_CATEGORIES.as_slice()),
    ] {
        let present: HashSet<&str> = cases(&m)
            .iter()
            .filter(|c| c["protocol"] == p)
            .map(|c| strf(c, "category"))
            .collect();
        assert_eq!(
            present.len(),
            required.len(),
            "{p}: category set must match the matrix exactly"
        );
        for category in required {
            assert!(
                present.contains(category),
                "{p}: missing required category {category}"
            );
        }
    }
}
