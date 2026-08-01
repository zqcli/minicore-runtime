use std::str::FromStr;

use minicore_runtime::model_gateway::{
    ModelId, ProviderId, ProviderItemId, ProviderRequestId, ProviderResponseId,
    RedactedProviderCode,
};

#[test]
fn model_identity_and_provider_opaque_values_have_distinct_grammars() {
    assert_eq!(ProviderId::from_str("openai").unwrap().as_str(), "openai");
    assert_eq!(
        ProviderId::from_str("OpenAI-Prod_1").unwrap().as_str(),
        "OpenAI-Prod_1"
    );
    assert!(ProviderId::from_str("open/ai").is_err());
    assert_eq!(
        ModelId::from_str("claude/sonnet-4").unwrap().as_str(),
        "claude/sonnet-4"
    );
    assert_eq!(
        ModelId::from_str("Vendor/Model:V2").unwrap().as_str(),
        "Vendor/Model:V2"
    );
    assert!(ProviderId::from_str(&"x".repeat(128)).is_ok());
    assert!(ProviderId::from_str(&"x".repeat(129)).is_err());

    for value in ["request/abc:1", "response_2"] {
        assert_eq!(ProviderRequestId::from_str(value).unwrap().as_str(), value);
        assert_eq!(ProviderResponseId::from_str(value).unwrap().as_str(), value);
        assert_eq!(ProviderItemId::from_str(value).unwrap().as_str(), value);
    }
    for invalid in ["", "has space", "has\"quote", "has\\slash", "é"] {
        assert!(ProviderResponseId::from_str(invalid).is_err());
        assert!(RedactedProviderCode::from_str(invalid).is_err());
    }
    assert!(ProviderResponseId::from_str(&"x".repeat(256)).is_ok());
    assert!(ProviderResponseId::from_str(&"x".repeat(257)).is_err());
}
