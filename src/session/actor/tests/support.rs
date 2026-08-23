use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::{CompactionConfig, SessionManifest};
use crate::conversation::{
    AssistantMessageDraft, TurnExecutionRecord, UnsequencedEntry, UserInputRecord, UserMessageDraft,
};
use crate::model::{
    Model, ModelCallContext, ModelDescriptor, ModelFinishReason, ModelRequest, ModelStartFuture,
    ReasoningPreference, ToolCall, Usage,
};
use crate::storage::{AppendReceipt, ConversationPage, LogFuture, SessionLog};
use crate::time::Timestamp;
use crate::tools::{ToolName, ToolSet};
use crate::value::BoundedText;

use super::super::*;

pub(crate) struct ActorFixture {
    pub(crate) actor: SessionActor,
    pub(crate) ready: ActorReady,
    pub(crate) turn_id: TurnId,
    pub(crate) tool_call_id: crate::ids::ToolCallId,
    pub(crate) tool_name: ToolName,
    pub(crate) second_tool_call_id: crate::ids::ToolCallId,
    pub(crate) second_tool_name: ToolName,
    pub(crate) append_count: Arc<AtomicUsize>,
    pub(crate) critical_tx: mpsc::Sender<RunnerEvent>,
}

impl ActorFixture {
    pub(crate) fn install_runner<F>(&mut self, future: F)
    where
        F: std::future::Future<Output = TurnRunnerExit> + Send + 'static,
    {
        let guard = self.actor.runner_lifecycle.start();
        let generation = guard.generation();
        let runner = tokio::spawn(async move {
            let _guard = guard;
            future.await
        });
        self.actor
            .runner_lifecycle
            .install_abort(generation, runner.abort_handle());
        self.actor.active.as_mut().unwrap().runner = Some(runner);
    }

    pub(crate) fn root_cancel(&self) -> CancellationToken {
        self.actor.root_cancel.clone()
    }

    pub(crate) fn runner_lifecycle(&self) -> RunnerLifecycle {
        self.actor.runner_lifecycle.clone()
    }
}

pub(crate) async fn actor_fixture(with_pending_tool: bool) -> ActorFixture {
    actor_fixture_with_tool_calls(usize::from(with_pending_tool)).await
}

pub(crate) async fn actor_fixture_with_tool_calls(tool_call_count: usize) -> ActorFixture {
    let session_id: SessionId = "ses_00000000000000000000000000000081".parse().unwrap();
    let instance_id: SessionInstanceId = "ins_00000000000000000000000000000081".parse().unwrap();
    let turn_id: TurnId = "trn_00000000000000000000000000000081".parse().unwrap();
    let tool_call_id: crate::ids::ToolCallId =
        "call_00000000000000000000000000000081".parse().unwrap();
    let tool_name: ToolName = "search".parse().unwrap();
    let second_tool_call_id: crate::ids::ToolCallId =
        "call_00000000000000000000000000000082".parse().unwrap();
    let second_tool_name: ToolName = "lookup".parse().unwrap();
    let model_ref: crate::model::ModelRef = "host:actor-suspension".parse().unwrap();
    let mut enabled_tools = BTreeSet::from([tool_name.clone()]);
    if tool_call_count > 1 {
        enabled_tools.insert(second_tool_name.clone());
    }
    let spec = SessionSpec::new(
        model_ref.clone(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        enabled_tools,
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();
    let kernel = KernelConfig::default_checked().unwrap();
    let manifest = SessionManifest::new(session_id, spec.clone()).unwrap();
    let append_count = Arc::new(AtomicUsize::new(0));
    let mut conversation = ConversationLog::initialize(
        Box::new(MemoryLog {
            manifest: None,
            append_count: Arc::clone(&append_count),
        }),
        manifest,
        kernel.clone(),
        Box::new(Timestamp::now_utc),
    )
    .await
    .unwrap();
    let execution =
        TurnExecutionRecord::new(model_ref.clone(), ReasoningPreference::Auto, 4).unwrap();
    conversation
        .append_validated(vec![UnsequencedEntry::UserMessage(UserMessageDraft {
            turn_id,
            input: UserInputRecord::new(BoundedText::new("question").unwrap()).unwrap(),
            execution,
        })])
        .await
        .unwrap();
    if tool_call_count > 0 {
        let mut calls = vec![
            ToolCall::new(
                tool_call_id.clone(),
                tool_name.clone(),
                serde_json::json!({}),
                0,
            )
            .unwrap(),
        ];
        if tool_call_count > 1 {
            calls.push(
                ToolCall::new(
                    second_tool_call_id.clone(),
                    second_tool_name.clone(),
                    serde_json::json!({}),
                    1,
                )
                .unwrap(),
            );
        }
        conversation
            .append_validated(vec![UnsequencedEntry::AssistantMessage(
                AssistantMessageDraft {
                    turn_id,
                    model: model_ref.clone(),
                    text: None,
                    reasoning: None,
                    tool_calls: calls,
                    usage: Usage::new(1, 1, 0),
                    finish_reason: ModelFinishReason::ToolCalls,
                },
            )])
            .await
            .unwrap();
    }
    let model: Arc<dyn Model> = Arc::new(NoCallModel(
        ModelDescriptor::new(
            model_ref,
            4_096,
            BTreeSet::from([ReasoningPreference::Auto]),
            true,
        )
        .unwrap(),
    ));
    let bindings =
        SessionBindings::new(model, ToolSet::builder().build().unwrap(), None, None, None);
    let (mut actor, ready) = SessionActor::new(
        conversation,
        kernel.clone(),
        bindings,
        spec,
        session_id,
        instance_id,
        CancellationToken::new(),
    )
    .map_err(|_| ())
    .unwrap();
    let cancellation = CancellationToken::new();
    let (_handle, completion) =
        TurnHandle::new(session_id, instance_id, turn_id, cancellation.clone());
    let (critical_tx, critical) = mpsc::channel(kernel.runner_capacity);
    let (_progress_tx, progress) = mpsc::channel(kernel.runner_capacity);
    actor.active = Some(ActiveTurn {
        turn_id,
        cancellation,
        completion,
        critical,
        progress,
        runner: None,
        critical_open: true,
        progress_open: true,
        outcome: None,
        pending: None,
        commit_failure: None,
    });
    let mut state = actor.state();
    state.status = SessionStatus::Running;
    state.active_turn = Some(turn_id);
    actor.publish_state(state);
    ActorFixture {
        actor,
        ready,
        turn_id,
        tool_call_id,
        tool_name,
        second_tool_call_id,
        second_tool_name,
        append_count,
        critical_tx,
    }
}

struct NoCallModel(ModelDescriptor);

impl Model for NoCallModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.0
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        panic!("suspension actor test must not start the model")
    }
}

struct MemoryLog {
    manifest: Option<SessionManifest>,
    append_count: Arc<AtomicUsize>,
}

impl SessionLog for MemoryLog {
    fn initialize<'a>(&'a mut self, manifest: SessionManifest) -> LogFuture<'a, ConversationSeq> {
        self.manifest = Some(manifest);
        Box::pin(async { Ok(ConversationSeq::ZERO) })
    }

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest> {
        let manifest = self.manifest.clone().unwrap();
        Box::pin(async move { Ok(manifest) })
    }

    fn read_page<'a>(
        &'a mut self,
        _after: Option<ConversationSeq>,
        _limit: usize,
    ) -> LogFuture<'a, ConversationPage> {
        panic!("suspension actor test must not read the log")
    }

    fn append<'a>(
        &'a mut self,
        expected_head: ConversationSeq,
        entries: Vec<crate::conversation::ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt> {
        self.append_count.fetch_add(1, Ordering::SeqCst);
        let new_head = entries.last().unwrap().seq();
        let appended = entries.len();
        Box::pin(async move {
            Ok(AppendReceipt {
                previous_head: expected_head,
                new_head,
                appended,
            })
        })
    }

    fn close<'a>(&'a mut self) -> LogFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}
