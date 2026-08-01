use std::fmt::Display;
use std::str::FromStr;

use minicore_runtime::wire::{
    AgentId, AgentMetadataRevision, AgentRevision, CanonicalU64, CommandId, EntryId,
    InteractionResolutionKey, ItemId, PageCursor, RequestId, SessionDefinitionRevision, SessionId,
    SessionMetadataRevision, TurnId, WorkspaceRevision,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

fn assert_string_round_trip<T>(wire: &str)
where
    T: FromStr + Display + Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    T::Err: std::fmt::Debug,
{
    let parsed = wire.parse::<T>().unwrap();
    assert_eq!(parsed.to_string(), wire);
    let json = serde_json::to_string(&parsed).unwrap();
    assert_eq!(json, format!("\"{wire}\""));
    assert_eq!(serde_json::from_str::<T>(&json).unwrap(), parsed);
}

#[test]
fn runtime_ids_use_typed_nonzero_lower_hex_carriers() {
    assert_string_round_trip::<AgentId>("agt_0123456789abcdef0123456789abcdef");
    assert_string_round_trip::<SessionId>("ses_0123456789abcdef0123456789abcdef");
    assert_string_round_trip::<TurnId>("trn_0123456789abcdef0123456789abcdef");
    assert_string_round_trip::<ItemId>("itm_0123456789abcdef0123456789abcdef");
    assert_string_round_trip::<RequestId>("req_0123456789abcdef0123456789abcdef");
    assert_string_round_trip::<EntryId>("ent_0123456789abcdef0123456789abcdef");
    assert_string_round_trip::<CommandId>("cmd_0123456789abcdef0123456789abcdef");

    assert!(
        "agt_00000000000000000000000000000000"
            .parse::<AgentId>()
            .is_err()
    );
    assert!(
        "ses_0123456789abcdef0123456789abcdef"
            .parse::<AgentId>()
            .is_err()
    );
    assert!(
        "agt_0123456789ABCDEF0123456789ABCDEF"
            .parse::<AgentId>()
            .is_err()
    );
    assert!("agt_0123456789abcdef".parse::<AgentId>().is_err());
}

#[test]
fn interaction_key_round_trips_only_through_wire_serde() {
    let json = "\"irk_0123456789abcdef0123456789abcdef\"";
    let key: InteractionResolutionKey = serde_json::from_str(json).unwrap();
    assert_eq!(serde_json::to_string(&key).unwrap(), json);
    assert!(
        serde_json::from_str::<InteractionResolutionKey>(
            "\"irk_00000000000000000000000000000000\""
        )
        .is_err()
    );
}

#[test]
fn revisions_are_typed_positive_canonical_decimals() {
    assert_string_round_trip::<AgentRevision>("ar_1");
    assert_string_round_trip::<AgentMetadataRevision>("amr_2");
    assert_string_round_trip::<SessionDefinitionRevision>("sdr_3");
    assert_string_round_trip::<SessionMetadataRevision>("smr_4");
    assert_string_round_trip::<WorkspaceRevision>("wr_18446744073709551615");

    assert!("ar_0".parse::<AgentRevision>().is_err());
    assert!("ar_01".parse::<AgentRevision>().is_err());
    assert!("smr_1".parse::<AgentRevision>().is_err());
    assert!("ar_18446744073709551616".parse::<AgentRevision>().is_err());
}

#[test]
fn ordinary_u64_uses_a_decimal_json_string() {
    assert_string_round_trip::<CanonicalU64>("18446744073709551615");
    assert!("01".parse::<CanonicalU64>().is_err());
    assert!(serde_json::from_str::<CanonicalU64>("1").is_err());
}

#[test]
fn generated_carriers_are_parseable_and_nonzero() {
    let agent = AgentId::generate().unwrap();
    assert_eq!(agent.to_string().parse::<AgentId>().unwrap(), agent);
    assert!(agent.as_bytes().iter().any(|byte| *byte != 0));

    let key = InteractionResolutionKey::generate().unwrap();
    let json = serde_json::to_string(&key).unwrap();
    assert!(serde_json::from_str::<InteractionResolutionKey>(&json).is_ok());

    let cursor = PageCursor::generate().unwrap();
    assert_eq!(cursor.to_string().parse::<PageCursor>().unwrap(), cursor);
}

#[test]
fn page_cursor_is_exact_canonical_base64url_without_padding() {
    let wire = "pc1_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    assert_string_round_trip::<PageCursor>(wire);

    assert!(
        "pc1_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
            .parse::<PageCursor>()
            .is_err()
    );
    assert!(
        "pc1_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAB"
            .parse::<PageCursor>()
            .is_err()
    );
    assert!(
        "pc2_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            .parse::<PageCursor>()
            .is_err()
    );
}
