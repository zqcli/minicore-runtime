use std::str::FromStr;

use minicore_runtime::wire::{CurrencyCode, Duration, Money, MoneyAmount, Timestamp};

#[test]
fn timestamp_is_exact_utc_rfc3339_milliseconds() {
    let wire = "2026-07-31T12:34:56.789Z";
    let timestamp = Timestamp::from_str(wire).unwrap();
    assert_eq!(timestamp.to_string(), wire);
    assert_eq!(
        serde_json::to_string(&timestamp).unwrap(),
        format!("\"{wire}\"")
    );
    assert_eq!(
        serde_json::from_str::<Timestamp>(&format!("\"{wire}\"")).unwrap(),
        timestamp
    );

    for invalid in [
        "2026-07-31T12:34:56Z",
        "2026-07-31T12:34:56.78Z",
        "2026-07-31T12:34:56.7890Z",
        "2026-07-31T12:34:56.789+00:00",
        "2026-07-31t12:34:56.789z",
        "2026-07-31T12:34:60.000Z",
        "0000-01-01T00:00:00.000Z",
    ] {
        assert!(invalid.parse::<Timestamp>().is_err(), "accepted {invalid}");
    }
}

#[test]
fn duration_is_a_bounded_json_integer_in_milliseconds() {
    let duration = Duration::new(2_000).unwrap();
    assert_eq!(duration.milliseconds(), 2_000);
    assert_eq!(serde_json::to_string(&duration).unwrap(), "2000");
    assert_eq!(
        serde_json::from_str::<Duration>("86400000")
            .unwrap()
            .milliseconds(),
        86_400_000
    );

    assert!(Duration::new(86_400_001).is_err());
    assert!(serde_json::from_str::<Duration>("86400001").is_err());
    assert!(serde_json::from_str::<Duration>("\"2000\"").is_err());
    assert!(serde_json::from_str::<Duration>("-1").is_err());
}

#[test]
fn money_uses_canonical_decimal_and_uppercase_currency() {
    let money: Money = serde_json::from_str(r#"{"amount":"12.34","currency":"USD"}"#).unwrap();
    assert_eq!(money.amount().to_string(), "12.34");
    assert_eq!(money.currency().as_str(), "USD");
    assert_eq!(
        serde_json::to_string(&money).unwrap(),
        r#"{"amount":"12.34","currency":"USD"}"#
    );

    for valid in ["0", "1", "0.01", "999999999999999999.999999999"] {
        assert_eq!(valid.parse::<MoneyAmount>().unwrap().to_string(), valid);
    }
    for invalid in [
        "00",
        "01",
        "+1",
        "-1",
        "1.",
        ".1",
        "1.0",
        "1.230",
        "1e3",
        "1000000000000000000",
        "0.1234567890",
    ] {
        assert!(
            invalid.parse::<MoneyAmount>().is_err(),
            "accepted {invalid}"
        );
    }

    assert!("usd".parse::<CurrencyCode>().is_err());
    assert!("US".parse::<CurrencyCode>().is_err());
    assert!(
        serde_json::from_str::<Money>(r#"{"amount":"1","currency":"USD","future":true}"#).is_err()
    );
}
