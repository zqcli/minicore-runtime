use std::time::Duration;

use minicore_runtime::error::{DiagnosticCategory, DiagnosticCode, DiagnosticSummary};
use minicore_runtime::model::{DeliveryState, ModelError, ModelErrorKind, RetryHint};
use minicore_runtime::value::BoundedText;
use serde_json::json;

#[test]
fn model_errors_enforce_delivery_retry_and_panic_invariants() {
    let diag_unretryable = DiagnosticSummary::new(
        DiagnosticCode::ModelUnavailable,
        DiagnosticCategory::Model,
        BoundedText::new("rate limited").unwrap(),
        false,
    );
    let diag_retryable = DiagnosticSummary::new(
        DiagnosticCode::ModelUnavailable,
        DiagnosticCategory::Model,
        BoundedText::new("rate limited").unwrap(),
        true,
    );

    let not_started_retry = ModelError::not_started(
        ModelErrorKind::RateLimited,
        Some(Duration::from_secs(2)),
        diag_unretryable.clone(),
    );
    assert_eq!(not_started_retry.kind(), ModelErrorKind::RateLimited);
    assert_eq!(not_started_retry.delivery(), DeliveryState::NotStarted);
    assert_eq!(
        not_started_retry.retry_hint(),
        &RetryHint::Retryable {
            retry_after: Some(Duration::from_secs(2)),
        }
    );
    assert!(not_started_retry.diagnostic().retryable);

    let not_started_no_delay = ModelError::not_started(
        ModelErrorKind::ProviderUnavailable,
        None,
        diag_unretryable.clone(),
    );
    assert_eq!(
        not_started_no_delay.kind(),
        ModelErrorKind::ProviderUnavailable,
    );
    assert_eq!(not_started_no_delay.delivery(), DeliveryState::NotStarted);
    assert_eq!(
        not_started_no_delay.retry_hint(),
        &RetryHint::Retryable { retry_after: None },
    );
    assert!(not_started_no_delay.diagnostic().retryable);

    let started_err =
        ModelError::started(ModelErrorKind::StreamInterrupted, diag_retryable.clone());
    assert_eq!(started_err.kind(), ModelErrorKind::StreamInterrupted);
    assert_eq!(started_err.delivery(), DeliveryState::Started);
    assert_eq!(started_err.retry_hint(), &RetryHint::Never);
    assert!(!started_err.diagnostic().retryable);

    let unknown_err = ModelError::unknown(ModelErrorKind::Timeout, diag_retryable.clone());
    assert_eq!(unknown_err.kind(), ModelErrorKind::Timeout);
    assert_eq!(unknown_err.delivery(), DeliveryState::Unknown);
    assert_eq!(unknown_err.retry_hint(), &RetryHint::Never);
    assert!(!unknown_err.diagnostic().retryable);

    let permanent_err = ModelError::permanent(
        ModelErrorKind::AuthRejected,
        DeliveryState::NotStarted,
        diag_retryable.clone(),
    );
    assert_eq!(permanent_err.kind(), ModelErrorKind::AuthRejected);
    assert_eq!(permanent_err.delivery(), DeliveryState::NotStarted);
    assert_eq!(permanent_err.retry_hint(), &RetryHint::Never);
    assert!(!permanent_err.diagnostic().retryable);

    for err in [
        &not_started_retry,
        &not_started_no_delay,
        &started_err,
        &unknown_err,
        &permanent_err,
    ] {
        let serialized = serde_json::to_string(err).unwrap();
        let deserialized: ModelError = serde_json::from_str(&serialized).unwrap();
        assert_eq!(err, &deserialized);
    }

    let unknown_field = json!({
        "kind": "rate_limited",
        "delivery": "not_started",
        "retry_hint": {
            "retryable": {
                "retry_after": { "secs": 1, "nanos": 0 },
            },
        },
        "diagnostic": diag_unretryable,
        "unexpected": "disallowed",
    });
    assert!(serde_json::from_value::<ModelError>(unknown_field).is_err());

    let unknown_nested_hint = json!({
        "kind": "rate_limited",
        "delivery": "not_started",
        "retry_hint": {
            "retryable": {
                "retry_after": { "secs": 1, "nanos": 0 },
                "unexpected": 1,
            },
        },
        "diagnostic": diag_unretryable,
    });
    assert!(serde_json::from_value::<ModelError>(unknown_nested_hint).is_err());

    let hint_nested_unknown = json!({
        "retryable": {
            "retry_after": { "secs": 1, "nanos": 0 },
            "unexpected": 1,
        },
    });
    assert!(serde_json::from_value::<RetryHint>(hint_nested_unknown).is_err());

    let started_retry = json!({
        "kind": "stream_interrupted",
        "delivery": "started",
        "retry_hint": { "retryable": {} },
        "diagnostic": diag_unretryable,
    });
    assert!(serde_json::from_value::<ModelError>(started_retry).is_err());

    let unknown_retry = json!({
        "kind": "timeout",
        "delivery": "unknown",
        "retry_hint": { "retryable": {} },
        "diagnostic": diag_unretryable,
    });
    assert!(serde_json::from_value::<ModelError>(unknown_retry).is_err());

    let serde_norm_retry = json!({
        "kind": "rate_limited",
        "delivery": "not_started",
        "retry_hint": {
            "retryable": {
                "retry_after": { "secs": 2, "nanos": 0 },
            },
        },
        "diagnostic": diag_unretryable,
    });
    let de_retry: ModelError = serde_json::from_value(serde_norm_retry).unwrap();
    assert!(de_retry.diagnostic().retryable);

    let serde_norm_never = json!({
        "kind": "timeout",
        "delivery": "started",
        "retry_hint": "never",
        "diagnostic": diag_retryable,
    });
    let de_never: ModelError = serde_json::from_value(serde_norm_never).unwrap();
    assert!(!de_never.diagnostic().retryable);

    let debug = format!("{not_started_retry:?}");
    assert!(!debug.contains("secret"));
}
