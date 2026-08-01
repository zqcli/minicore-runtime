use minicore_runtime::turn_item_interaction::{
    AssistantDisposition, InteractionCancelReason, ItemContentFamily, ItemStatus, UserMessageSource,
};

const fn cancellation_code(reason: InteractionCancelReason) -> u8 {
    match reason {
        InteractionCancelReason::HostCancelled => 0,
        InteractionCancelReason::TurnCancelled => 1,
        InteractionCancelReason::SecurityRevoked => 2,
        InteractionCancelReason::SessionUnloaded => 3,
        InteractionCancelReason::RuntimeClosing => 4,
        InteractionCancelReason::TurnTerminal => 5,
    }
}

const fn item_family_code(family: ItemContentFamily) -> u8 {
    match family {
        ItemContentFamily::UserMessage => 0,
        ItemContentFamily::AgentMessage => 1,
        ItemContentFamily::Reasoning => 2,
        ItemContentFamily::ToolInvocation => 3,
    }
}

#[test]
fn turn_item_and_interaction_leaves_are_closed_and_distinct() {
    assert_ne!(UserMessageSource::Input, UserMessageSource::Steer);
    assert_ne!(
        AssistantDisposition::Intermediate,
        AssistantDisposition::Final
    );
    assert_ne!(ItemStatus::Completed, ItemStatus::Abandoned);
    assert_eq!(item_family_code(ItemContentFamily::ToolInvocation), 3);

    let cancellation_reasons = [
        InteractionCancelReason::HostCancelled,
        InteractionCancelReason::TurnCancelled,
        InteractionCancelReason::SecurityRevoked,
        InteractionCancelReason::SessionUnloaded,
        InteractionCancelReason::RuntimeClosing,
        InteractionCancelReason::TurnTerminal,
    ];
    assert_eq!(
        cancellation_reasons.map(cancellation_code),
        [0, 1, 2, 3, 4, 5]
    );
}
