use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::super::actor::SessionActorExit;
use super::super::runtime::SessionRuntimeOptions;
use super::{OpenPayload, OpenRequest, run_open};
use crate::bindings::SessionBindings;
use crate::config::{CompactionConfig, KernelConfig, SessionManifest, SessionSpec};
use crate::conversation::{ConversationEntry, ConversationSeq};
use crate::error::{
    DiagnosticCategory, DiagnosticCode, DiagnosticSummary, SessionLogError, SessionLogErrorKind,
};
use crate::model::{
    Model, ModelCallContext, ModelDescriptor, ModelRequest, ModelStartFuture, ReasoningPreference,
};
use crate::storage::{AppendReceipt, ConversationPage, LogFuture, SessionLog};
use crate::tools::ToolSet;
use crate::value::BoundedText;

#[test]
fn load_orders_manifest_binding_proof_replay_repair_and_ready() {
    let source = include_str!("../runtime_open.rs")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("");
    let prepare_load = source
        .split_once("asyncfnprepare_load(")
        .and_then(|(_, rest)| rest.split_once("asyncfnbuild_owner("))
        .map(|(body, _)| body)
        .unwrap();
    let begin = prepare_load.find("ConversationLog::begin_load").unwrap();
    let environment = prepare_load
        .find("SessionEnvironment::build(&parts.kernel,&pending.manifest().spec,&parts.bindings)")
        .unwrap();
    let proof = prepare_load
        .find("LoadCompatibilityValidated::after_session_bindings_validation(&pending)")
        .unwrap();
    let finish = prepare_load.find(".finish(proof)").unwrap();
    let build = prepare_load
        .find("build_owner(session_id,conversation,environment,owner_cancel).await")
        .unwrap();
    assert!(begin < environment && environment < proof && proof < finish && finish < build);

    let run_open = source
        .split_once("pub(super)asyncfnrun_open(")
        .and_then(|(_, rest)| rest.split_once("asyncfnprepare("))
        .map(|(body, _)| body)
        .unwrap();
    let prepare = run_open
        .find("prepare(request,log,options,&owner_cancel).await")
        .unwrap();
    let prepared = run_open.find("Ok(prepared)=>prepared").unwrap();
    let ready = run_open.find("ready.send(Ok(OpenReady").unwrap();
    assert!(prepare < prepared && prepared < ready);
    assert!(!source.contains("HashMap<SessionId"));
}

#[test]
fn owner_signals_payload_claim_before_any_open_work_or_await() {
    let source = include_str!("../runtime_open.rs");
    let run = source
        .split_once("pub(super) async fn run_open(")
        .and_then(|(_, rest)| rest.split_once("async fn prepare("))
        .map(|(body, _)| body)
        .unwrap();
    let take = run.find("payload.take()").unwrap();
    let claim = run.find("payload_claimed.cancel()").unwrap();
    let prepare = run.find("prepare(request, log, options").unwrap();
    assert!(take < claim && claim < prepare);
    assert!(!run[take..claim].contains(".await"));
}

#[test]
fn host_controlled_panic_boundaries_remain_isolated() {
    let bindings = include_str!("../../bindings.rs");
    assert!(
        bindings.contains("catch_unwind(AssertUnwindSafe(|| self.model.descriptor().clone()))")
    );

    let conversation = include_str!("../../conversation/log.rs");
    let operation = conversation
        .split_once("pub(super) async fn run_log_operation")
        .map(|(_, body)| body)
        .unwrap();
    assert!(operation.contains("catch_unwind(AssertUnwindSafe(operation))"));
    assert!(operation.contains("AssertUnwindSafe(future).catch_unwind()"));

    let supervisor = include_str!("../actor/supervisor.rs");
    assert!(supervisor.contains("AssertUnwindSafe(actor.run())"));
    assert!(supervisor.contains(".catch_unwind()"));
    assert!(supervisor.contains("actor.close_after_panic().await"));
}

struct TestModel(ModelDescriptor);

impl Model for TestModel {
    fn descriptor(&self) -> &ModelDescriptor {
        &self.0
    }

    fn start<'a>(
        &'a self,
        _request: ModelRequest,
        _context: ModelCallContext,
    ) -> ModelStartFuture<'a> {
        panic!("open readiness must not start the model")
    }
}

fn options() -> (SessionSpec, SessionRuntimeOptions) {
    let spec = SessionSpec::new(
        "host:model".parse().unwrap(),
        ReasoningPreference::Auto,
        BoundedText::new("system").unwrap(),
        BTreeSet::new(),
        4,
        CompactionConfig::Disabled,
    )
    .unwrap();
    let model: Arc<dyn Model> = Arc::new(TestModel(ModelDescriptor {
        model_ref: spec.model.clone(),
        context_window: 1,
        supported_reasoning: BTreeSet::from([ReasoningPreference::Auto]),
        supports_tools: false,
    }));
    let bindings =
        SessionBindings::new(model, ToolSet::builder().build().unwrap(), None, None, None);
    let options = SessionRuntimeOptions::new(
        KernelConfig::default_checked().unwrap(),
        bindings,
        tokio::runtime::Handle::current(),
    )
    .unwrap();
    (spec, options)
}

struct ReadyDropLog {
    initialize_count: Arc<AtomicUsize>,
    close_count: Arc<AtomicUsize>,
}

impl SessionLog for ReadyDropLog {
    fn initialize<'a>(&'a mut self, _manifest: SessionManifest) -> LogFuture<'a, ConversationSeq> {
        self.initialize_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ConversationSeq::ZERO) })
    }

    fn load_manifest<'a>(&'a mut self) -> LogFuture<'a, SessionManifest> {
        Box::pin(async { Err(unused_log_error()) })
    }

    fn read_page<'a>(
        &'a mut self,
        _after: Option<ConversationSeq>,
        _limit: usize,
    ) -> LogFuture<'a, ConversationPage> {
        Box::pin(async { Err(unused_log_error()) })
    }

    fn append<'a>(
        &'a mut self,
        _expected_head: ConversationSeq,
        _entries: Vec<ConversationEntry>,
    ) -> LogFuture<'a, AppendReceipt> {
        Box::pin(async { Err(unused_log_error()) })
    }

    fn close<'a>(&'a mut self) -> LogFuture<'a, ()> {
        self.close_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

fn unused_log_error() -> SessionLogError {
    SessionLogError::new(
        SessionLogErrorKind::Internal,
        DiagnosticSummary::bounded_static(
            DiagnosticCode::Internal,
            DiagnosticCategory::Storage,
            "unused ready-drop log operation",
            false,
        ),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn dropped_ready_receiver_closes_opened_log_without_starting_idle_actor() {
    let session_id = "ses_00000000000000000000000000000001".parse().unwrap();
    let initialize_count = Arc::new(AtomicUsize::new(0));
    let close_count = Arc::new(AtomicUsize::new(0));
    let log = ReadyDropLog {
        initialize_count: Arc::clone(&initialize_count),
        close_count: Arc::clone(&close_count),
    };
    let (spec, options) = options();
    let payload = OpenPayload::shared(
        OpenRequest::Create { session_id, spec },
        Box::new(log),
        options,
    );
    let cancel = CancellationToken::new();
    let payload_claimed = CancellationToken::new();
    let (ready, receiver) = oneshot::channel();
    drop(receiver);

    let exit = run_open(payload, cancel, payload_claimed.clone(), ready).await;
    assert!(matches!(exit, SessionActorExit::OpenFailed));
    assert!(payload_claimed.is_cancelled());
    assert_eq!(initialize_count.load(Ordering::SeqCst), 1);
    assert_eq!(close_count.load(Ordering::SeqCst), 1);
}
