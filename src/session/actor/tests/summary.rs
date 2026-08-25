use std::sync::atomic::Ordering;

use crate::agent::RunnerCommitError;
use crate::conversation::SummaryDraft;
use crate::value::BoundedText;

use super::super::*;
use super::support::actor_fixture;

#[tokio::test]
async fn stale_summary_snapshot_is_rejected_before_append_without_latching() {
    let mut fixture = actor_fixture(false).await;
    let head = fixture.actor.conversation.head();
    let appends = fixture.append_count.load(Ordering::SeqCst);
    let error = fixture
        .actor
        .commit_summary(
            ConversationSeq::ZERO,
            SummaryDraft {
                through: ConversationSeq::ZERO,
                summary: BoundedText::new("stale").unwrap(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error, RunnerCommitError::Stale);
    assert_eq!(fixture.append_count.load(Ordering::SeqCst), appends);
    assert_eq!(fixture.actor.conversation.head(), head);
    assert!(matches!(&fixture.actor.core.health, SessionHealth::Healthy));
    assert!(
        fixture
            .actor
            .core
            .active
            .as_ref()
            .is_some_and(|active| active.commit_failure.is_none())
    );
}
