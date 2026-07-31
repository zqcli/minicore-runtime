# Conversation JSONL Format V1

状态：当前权威format specification（ADR 0134，生产实现待启动）
日期：2026-07-31

## Scope

本文冻结MiniCore conversation recording的byte-level v1 envelope、field/tag、Stored DTO projection、line limits、writer validation与tolerant decoder行为。

业务语义仍由以下owner定义：

- Conversation Storage：Entry tree、replay、fork、recording health；
- Prompt：CanonicalUserMessage、MessageContent、contribution stamp；
- ModelGateway：ReasoningContent、ModelResponseSummary、usage/finish/metadata；
- Tools：ToolCall/result/outcome/disposition、approval/question safe types；
- Turn / Item / Interaction：User/Assistant disposition、InteractionCancelReason；
- Compaction：StoredCompaction与StoredCompactionModelCall。

共同JSON、ID、revision、Timestamp、Money、BoundedJson与scanner floor见[Wire Schema](../modules/wire-schema.md)。本文不定义Turn lifecycle、Session definition/metadata/lifecycle event或execution checkpoint。

## Physical File

```text
sessions/<SessionId>.jsonl
```

```text
line 1   session_header
line 2+  entry
```

writer输出UTF-8 compact JSON + LF。reader接受LF/CRLF。每个complete line是一个adjacent-tagged record：

```json
{"type":"session_header","data":{...}}
{"type":"entry","data":{...}}
```

Header line最多65,536 bytes；entry line最多1,048,576 bytes；file/entry count/scanner behavior使用Wire Schema hard caps。

## Header

Rust semantic shape：

```rust
pub struct SessionHeader {
    pub format_version: u32,
    pub session_id: SessionId,
    pub created_at: Timestamp,
    pub initial_agent: AgentRevisionRef,
    pub initial_definition_revision: SessionDefinitionRevision,
}
```

Exact wire：

```json
{"type":"session_header","data":{"formatVersion":1,"sessionId":"ses_11111111111111111111111111111111","createdAt":"2026-07-31T12:00:00.000Z","initialAgent":{"agentId":"agt_22222222222222222222222222222222","revision":"ar_1"},"initialDefinitionRevision":"sdr_1"}}
```

规则：

- record 1必须exact为session_header；
- formatVersion必须JSON integer `1`；
- unknown version、unknown top-level type、malformed/duplicate field、invalid UTF-8或oversized Header使Load fail closed；
- Header.sessionId必须匹配caller打开的catalog/path SessionId；
- Header没有EntryId/parent/TurnId；
- Header只证明file identity与creation provenance，不是current definition/authorization；
- writable open只有在valid v1 Header与exclusive lease后才可truncate final partial tail；
- v1 writer不向higher version file append。

## Entry Envelope

```rust
pub struct StoredSessionEntry {
    pub entry_id: EntryId,
    pub parent_id: Option<EntryId>,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub timestamp: Timestamp,
    pub body: StoredEntryBody,
}

pub enum StoredEntryBody {
    UserMessage(StoredUserMessage),
    AssistantMessage(StoredAssistantMessage),
    ToolMessage(StoredToolMessage),
    InteractionRequested(StoredInteractionRequest),
    InteractionResolved(StoredInteractionResolution),
    Compaction(StoredCompaction),
}
```

Exact envelope：

```json
{"type":"entry","data":{"entryId":"ent_33333333333333333333333333333333","parentId":null,"sessionId":"ses_11111111111111111111111111111111","turnId":"trn_44444444444444444444444444444444","timestamp":"2026-07-31T12:00:01.000Z","body":{"type":"user_message","data":{...}}}}
```

所有v1 entry都是conversation/Interaction/Compaction Turn fact，因此turnId required non-null。Session definition/lifecycle event已由ADR 0131删除，不能通过`turnId:null`恢复。

Entry关系：

- entryId由LiveSessionState在apply前分配；
- first valid EntryId wins；duplicate later line skip + diagnostic；
- sessionId必须等于Header；
- parentId为null表示root，否则必须引用当前file中更早的first-valid EntryId；
- valid branch parent不要求是physical previous line；
- orphan/invalid relation line可以作为diagnosed history node隔离，但不能进入selected model conversation；
- timestamp不是ordering key；physical scan order只用于first-valid与parent-seen validation。

## User Message

```rust
pub struct StoredUserMessage {
    pub item_id: ItemId,
    pub source: UserMessageSource,
    pub content: CanonicalUserMessage,
}
```

Wire：

```json
{"type":"user_message","data":{"itemId":"itm_55555555555555555555555555555555","source":"input","content":{"parts":[{"type":"text","data":{"text":"hello"}}],"contributionStamps":[]}}}
```

UserMessageSource pure values：

```text
input
steer
```

FollowUp真正admit为new Turn时记录input。

CanonicalUserMessage wire：

```text
content.parts: ordered 1..64 MessageContent
content.contributionStamps: ordered 0..64 PromptContributionStamp
```

MVP MessageContent：

```json
{"type":"text","data":{"text":"..."}}
```

PromptContributionStamp：

```json
{"contentPartIndex":1,"origin":{"type":"skill","data":{"skillId":"code-review"}}}
{"contentPartIndex":2,"origin":{"type":"workspace","data":{"rootKey":"repo","relativeLocation":"src/lib.rs"}}}
```

`contentPartIndex`是zero-based u32；示例中的1/2表示前面另有body part index 0。完整fixture必须同时展示parts数组，不能把示例误读为1-based。

规则：

- parts decode与stamps decode独立；
- parts malformed使entry invalid；
- malformed/unknown/out-of-range stamp单独drop + diagnostic；
- same contentPartIndex first valid stamp wins，later duplicate drop；
- body part无stamp；contribution part恰有一个stamp；
- max part bytes 131,072，aggregate user message bytes 524,288；
- stamp不含source bytes、absolute path、authorization、hash或cache key。

## Assistant Message

```rust
pub struct StoredAssistantMessage {
    pub disposition: AssistantDisposition,
    pub content: Arc<[StoredAssistantContent]>,
    pub model: ModelResponseSummary,
    pub response_id: Option<ProviderResponseId>,
    pub finish_reason: ModelFinishReason,
    pub effective_max_output_tokens: NonZeroU32,
    pub usage: Option<ModelUsage>,
    pub logical_retry_count: u8,
    pub metadata: ProviderResponseMetadata,
}

pub enum StoredAssistantContent {
    Reasoning {
        item_id: ItemId,
        content: ReasoningContent,
    },
    Text {
        item_id: ItemId,
        text: Arc<str>,
    },
    ToolCall {
        item_id: ItemId,
        tool_call_id: ToolCallId,
        name: ToolName,
        arguments: BoundedJsonObject,
    },
}
```

Wire example：

```json
{"type":"assistant_message","data":{"disposition":"final","content":[{"type":"text","data":{"itemId":"itm_66666666666666666666666666666666","text":"done"}}],"model":{"providerId":"openai","modelId":"gpt-5","reasoning":"provider_default","serviceClass":"standard"},"responseId":"resp_abc","finishReason":"stop","effectiveMaxOutputTokens":2048,"usage":{"inputTokens":"10","outputTokens":"3","reasoningTokens":null,"cacheReadTokens":null,"cacheWriteTokens":null,"providerTotalTokens":"13","reportedCost":{"amount":"0.01","currency":"USD"}},"logicalRetryCount":0,"metadata":{"providerRequestId":"req_provider_abc","rawFinishCode":"stop","serviceTier":null}}}
```

AssistantDisposition：`intermediate | final`。

StoredAssistantContent wire：

```json
{"type":"reasoning","data":{"itemId":"itm_77777777777777777777777777777777","content":{"text":null,"summary":"brief","encrypted":null,"signature":null,"providerItemId":null}}}
{"type":"text","data":{"itemId":"itm_66666666666666666666666666666666","text":"done"}}
{"type":"tool_call","data":{"itemId":"itm_88888888888888888888888888888888","toolCallId":"call_1","name":"read_file","arguments":{"path":"README.md"}}}
```

ModelResponseSummary：

```json
{"providerId":"openai","modelId":"gpt-5","reasoning":"provider_default","serviceClass":"standard"}
```

ModelReasoningSummary values：`provider_default | disabled | low | medium | high`。

ModelServiceClass values：`standard | priority`。

ModelUsage optional fields显式null；u64 token counts使用decimal strings。ProviderResponseMetadata仅三个allowlisted optional fields。

Assistant validation：

- content 1..128，ItemId在entry内unique；
- ToolCallId在assistant response内unique；
- Text non-empty safe text，单part<=65,536 bytes；
- ReasoningContent至少一个artifact field，按ModelGateway caps；
- BoundedJsonObject按Wire limits；
- total encoded assistant body必须<=entry line cap，content semantic aggregate<=786,432 bytes；
- logicalRetryCount AgentRun `0..=3`；
- effectiveMaxOutputTokens non-zero并匹配actual request basis；
- Length/ContentFiltered不形成StoredAssistantMessage；
- ToolCalls finish需要至少一个ToolCall；
- ToolCall content只允许`intermediate`；
- `final`与live TurnCompleted同一decision形成；
- responseId/metadata不是conversation identity或authorization。

## Tool Message

```rust
pub struct StoredToolMessage {
    pub item_id: ItemId,
    pub tool_call_id: ToolCallId,
    pub outcome: StoredToolOutcome,
}

pub enum StoredToolOutcome {
    Completed {
        source: ToolOutcomeSource,
        disposition: ToolResultDisposition,
        content: ToolResultContent,
    },
    Abandoned {
        reason: ToolAbandonReason,
    },
}
```

Completed wire：

```json
{"type":"tool_message","data":{"itemId":"itm_88888888888888888888888888888888","toolCallId":"call_1","outcome":{"type":"completed","data":{"source":"executed","disposition":"succeeded","content":{"parts":[{"type":"text","data":{"text":"file contents"}}]}}}}}
```

Abandoned wire：

```json
{"type":"tool_message","data":{"itemId":"itm_88888888888888888888888888888888","toolCallId":"call_1","outcome":{"type":"abandoned","data":{"reason":"outcome_unknown"}}}}
```

ToolOutcomeSource：`pre_execution | executed`。
ToolResultDisposition：`succeeded | failed | denied | cancelled`。
ToolAbandonReason：`outcome_unknown | runtime_failure`。

规则：

- Stored outcome不保存ToolResult.details；private/debug JSON不是conversation fact；
- PreExecution只允许failed/denied/cancelled，Executed只允许succeeded/failed/cancelled；
- Completed content使用Tools-owned ToolResultContent，1..32 Text parts、aggregate<=262,144 bytes；
- Abandoned不创建provider-visible ToolResult，complete exchange sanitizer隔离该exchange；
- itemId/toolCallId必须匹配更早assistant ToolCall；
- first valid terminal outcome wins；later duplicate/conflict diagnostic + skip；
- complete exchange按assistant call order投影，不按physical result completion order。

## Interaction Requested

```rust
pub struct StoredInteractionRequest {
    pub request_id: RequestId,
    pub item_id: ItemId,
    pub request: StoredInteractionRequestBody,
}

pub enum StoredInteractionRequestBody {
    ToolApproval(ToolApprovalRequestView),
    UserQuestion(UserQuestionRequest),
}
```

Tool approval example：

```json
{"type":"interaction_requested","data":{"requestId":"req_99999999999999999999999999999999","itemId":"itm_88888888888888888888888888888888","request":{"type":"tool_approval","data":{"toolName":"write_file","argumentsSummary":"path: src/lib.rs","reason":"write requested","requirements":{"filesystem":"write workspace file","network":null,"process":null},"options":[{"optionIndex":0,"kind":"as_requested","label":"Allow once","effectiveRequirements":{"filesystem":"write workspace file","network":null,"process":null}}]}}}}
```

User question example：

```json
{"type":"interaction_requested","data":{"requestId":"req_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","request":{"type":"user_question","data":{"title":"Choose target","questions":[{"questionIndex":0,"prompt":"Where?","required":true,"input":{"type":"single_choice","data":{"options":[{"optionIndex":0,"label":"A"}]}}}]}}}}
```

request limits使用ProtocolLimits InteractionLimits；整个request encoded<=131,072 bytes。Tool approval private PermissionSet/route/prepared args不进入record。UserQuestion只含non-secret Text/SingleChoice。

## Interaction Resolved

```rust
pub struct StoredInteractionResolution {
    pub request_id: RequestId,
    pub item_id: ItemId,
    pub resolution: StoredInteractionResolutionBody,
    pub resolution_key: Option<InteractionResolutionKey>,
}

pub enum StoredInteractionResolutionBody {
    ToolApproval(ToolApprovalResolution),
    UserAnswer(UserQuestionAnswer),
    Cancelled(InteractionCancelReason),
}
```

Examples：

```json
{"type":"interaction_resolved","data":{"requestId":"req_99999999999999999999999999999999","itemId":"itm_88888888888888888888888888888888","resolution":{"type":"tool_approval","data":{"type":"allowed","data":{"optionIndex":0,"kind":"as_requested"}}},"resolutionKey":"irk_cccccccccccccccccccccccccccccccc"}}
```

```json
{"type":"interaction_resolved","data":{"requestId":"req_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","itemId":"itm_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","resolution":{"type":"cancelled","data":"turn_cancelled"},"resolutionKey":null}}
```

InteractionCancelReason values：

```text
host_cancelled
turn_cancelled
security_revoked
session_unloaded
runtime_closing
turn_terminal
```

规则：

- requestId/itemId/family必须匹配更早Pending request；
- approval option index/kind必须来自exact request；
- UserAnswer required/index/value-family/choice校验必须通过；
- any host `InteractionCommand::Resolve`（ToolApproval、UserAnswer或HostCancelled）使用non-null resolutionKey；Cancel/SecurityRevoked/Unload/Runtime/Turn terminal owner-driven closure使用null；
- same key/same canonical resolution幂等不会写第二entry；
- duplicate/different terminal resolution first valid wins；
- answer/cancel reason可以恢复historical fact，不恢复waiter或Pending state。

## Compaction

```rust
pub struct StoredCompaction {
    pub summary: String,
    pub first_kept_entry_id: Option<EntryId>,
    pub model_call: Option<StoredCompactionModelCall>,
}

pub struct StoredCompactionModelCall {
    pub model: ModelResponseSummary,
    pub response_id: Option<ProviderResponseId>,
    pub usage: Option<ModelUsage>,
    pub finish_reason: ModelFinishReason,
    pub requested_max_output_tokens: NonZeroU32,
    pub logical_retry_count: u8,
    pub metadata: ProviderResponseMetadata,
}
```

Automatic example：

```json
{"type":"compaction","data":{"summary":"Earlier work summary","firstKeptEntryId":"ent_dddddddddddddddddddddddddddddddd","modelCall":{"model":{"providerId":"openai","modelId":"gpt-5-mini","reasoning":"disabled","serviceClass":"standard"},"responseId":"resp_compact","usage":{"inputTokens":"1000","outputTokens":"200","reasoningTokens":null,"cacheReadTokens":null,"cacheWriteTokens":null,"providerTotalTokens":"1200","reportedCost":null},"finishReason":"stop","requestedMaxOutputTokens":512,"logicalRetryCount":0,"metadata":{"providerRequestId":null,"rawFinishCode":"stop","serviceTier":null}}}}
```

规则：

- summary non-empty safe text<=65,536 bytes；
- automatic active-Turn path modelCall required non-null；
- model_call finishReason只允许stop/unknown；
- requestedMaxOutputTokens non-zero；
- logicalRetryCount `0..=1`；
- firstKeptEntryId null表示覆盖全部current non-empty stable units；
- non-null marker必须按ADR 0132匹配index>0 stable-unit first EntryId；
- invalid marker忽略该Compaction effect并diagnose，不brick file；
- model response/usage/metadata limits复用Assistant规则。

## Writer Field Order

Canonical encoder field order固定用于golden bytes：

1. top-level `type`, `data`；
2. Header：formatVersion, sessionId, createdAt, initialAgent, initialDefinitionRevision；
3. Entry：entryId, parentId, sessionId, turnId, timestamp, body；
4. nested structs按本文code block field declaration order；
5. adjacent enum：type before optional data；
6. Option field始终存在，None输出null。

Decoder不依赖member order。

## Bounded Decode与Replay

1. Wire scanner先执行file/header/line/UTF-8/JSON structural caps；
2. Header strict decode；
3. each complete entry line decode raw envelope；
4. validate typed scalar/IDs/session/relation；
5. variant-specific semantic conversion；
6. apply tolerant history/reducer projection；
7. collect<=100 diagnostics + aggregate counters。

Entry-level invalid data：

- unknown additive field ignore；
- duplicate field、unknown top/body/leaf variant、invalid required scalar使owning line skip；
- contribution stamp是唯一independently degradable nested element；
- invalid Tool/Interaction relation隔离entry/effective exchange，不synthetic repair；
- invalid Compaction marker忽略effect但保留line diagnostic/history visibility；
- no raw line in diagnostic。

Writable open只truncatefinal unterminated tail。完整malformed/oversized/unknown line保留physical bytes。

## Version与Migration

- v1 reader/writer only formatVersion 1；
- unknown field additive compatible；
- new body/leaf enum需要format v2或explicit migration；
- v1 writer拒绝higher version；
- MVP无automatic migration、repair utility或middle-line rewrite；
- future migration必须source backup、staged full validation与atomic publication。

## Golden Fixture Matrix

必须有byte-exact line/whole-file fixtures：

- Header only；
- User Input与Steer，Skill/Workspace stamps；
- Assistant final/intermediate，Reasoning/Text/ToolCall；
- Completed ToolResult四dispositions与Abandoned两reasons；
- ToolApproval/UserQuestion request与allowed/denied/answer/cancel resolution；
- automatic Compaction model provenance；
- optional null fields、Money/u64/revision/ID/Timestamp；
- CRLF decode canonical LF re-encode；
- unknown additive field；
- unknown body variant；
- malformed/duplicate/out-of-range stamp；
- invalid UTF-8, malformed JSON, duplicate EntryId, missing parent, Session mismatch；
- incomplete/abandoned Tool exchange；
- invalid Interaction relation；
- invalid Compaction marker；
- final partial tail + exact truncation offset；
- oversized complete line followed by valid recoverable line；
- Header/entry/file/count/depth/member/string exact boundary and boundary+1 recipes；
- diagnostics 100 + aggregate truncation。

Expected fixture metadata至少保存accepted EntryIds、selected path、sanitized ModelMessages、historical Items、diagnostic codes/counts与writable truncation offset。
