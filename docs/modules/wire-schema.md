# Wire Schema 与 Bounded Decode

状态：当前权威架构（ADR 0134/0135；Wire foundations、bootstrap router与initial incremental public roots已实现，完整public/storage manifest仍按M2–M11增量关闭）
日期：2026-07-31

## 目的

本模块是MiniCore public JSON v1.0、shared scalar carrier、bounded JSON、ProtocolLimits与conversation JSONL共同lexical规则的唯一owner。

它回答：

- Rust semantic type如何编码为稳定JSON；
- public input/output和corrupt storage如何处理unknown data；
- ID/revision/Timestamp/Duration/Money/path/cursor如何表示；
- 所有untrusted JSON在哪些byte/depth/count边界前被拒绝；
- Protocol negotiation与limits如何进入host；
- JSONL scanner如何在partial、malformed和oversized line下保持bounded recovery。

它不拥有Agent/Session/Turn/Item/Interaction、Prompt、Tool、Model、Workspace、Compaction或Stored record的业务语义。

## Ownership Boundary

```text
semantic owner
→ construct validated/redacted semantic value
→ WireV1 codec
→ bounded JSON bytes

bounded JSON bytes
→ lexical/shape/limit decode
→ semantic owner validation/construction
→ typed public or stored value
```

Wire codec不得：

- 扩大Tool permission或Workspace authority；
- 猜测missing model/Tool/Interaction semantic variant；
- 从raw string恢复private handle；
- 截断required Snapshot、Tool exchange或conversation fact；
- 把decode success当成domain authorization；
- 建立generic Error/Common/domain registry。

## JSON V1 Conventions

### Encoding

- UTF-8 only，无BOM；
- canonical encoder输出compact JSON，无非必要whitespace；
- object field使用`camelCase`；
- enum/type token使用`snake_case`；
- object member order无语义；typed schema object由canonical encoder按Rust field declaration order输出，dynamic `BoundedJson*` object按后文decoded-key UTF-8 byte order输出；
- decoder在任意深度拒绝duplicate member name；
- empty collection输出`[]`或`{}`，不输出`null`；
- 所有known `Option<T>` field由canonical encoder显式输出value或`null`；decoder接受field missing或`null`并统一为None；
- required non-optional field缺失或为`null`时拒绝；
- ordinary semantic integer不得使用fraction或exponent；具体wide integer规则见后文；
- JSON string不执行Unicode NFC/NFKC normalization；owner不能靠视觉等价判断identity。

Canonical string escaping：

- quote编码`\"`、backslash编码`\\`；U+0008/U+0009/U+000A/U+000C/U+000D分别编码`\b/\t/\n/\f/\r`；其他U+0000..U+001F使用lowercase `\u00xx`；
- solidus不escape；其余Unicode scalar直接输出UTF-8，不使用可选`\u` escape；
- lone surrogate不构成有效Unicode scalar，拒绝；
- human/model-visible safe text不得含NUL、ESC、DEL、C1 control或除HT/LF外的C0 control；
- TextIntent/source text在owner boundary把CRLF/CR规范化为LF；external provider/Tool text中的forbidden control由owner在live apply前reject或替换为U+FFFD，并记录bounded diagnostic，wire adapter不得接收raw后自行redact。

### Enum Representation

Pure unit enum编码为string：

```json
"healthy"
```

只要enum有任意payload variant，全部variants统一使用adjacent tagging。mixed enum的unit variant canonical form省略`data`；input中的`data: null`也拒绝，避免两种canonical shape：

```json
{"type":"cancelled"}
{"type":"set","data":"new value"}
{"type":"turn_started","data":{"turnId":"trn_0123456789abcdef0123456789abcdef"}}
```

Nested family保持nested：

```json
{
  "type":"turn",
  "data":{
    "type":"submit",
    "data":{
      "sessionId":"ses_0123456789abcdef0123456789abcdef",
      "intent":{
        "body":{"type":"text","data":{"text":"hello"}},
        "skills":[]
      }
    }
  }
}
```

`type`与`data`是wire-reserved enum fields。禁止untagged union、externally-tagged `{ "Variant": ... }`或按字段存在性猜variant。

### Unknown Data

| Direction | Unknown object field | Unknown enum/record variant | Duplicate field |
| --- | --- | --- | --- |
| client → Runtime input | reject | reject | reject |
| Runtime → client output | client defensively ignore field；Runtime仍必须只发送selected minor声明的fields | protocol error unless selected minor/capability declares it | reject malformed frame |
| conversation JSONL v1 entry（valid Header之后） | ignore additive field | skip owning complete line + diagnostic | skip owning complete line + diagnostic |

unknown capability token是唯一通用例外：receiver忽略不认识的token。不能使用generic `Unknown`替代Turn terminal、Tool outcome、Interaction resolution或conversation fact。Runtime encoder必须严格输出selected minor schema；client ignore unknown output field只是防御性forward tolerance，不授权sender在1.0 frame偷发1.1 field，也不能据unknown field改变state。

## Public Protocol V1.0

### Bootstrap

Bootstrap本身使用本节固定shape：

```rust
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

pub struct ClientCapabilities {
    pub values: Vec<String>,
}

pub struct ProtocolHello {
    pub supported_versions: Vec<ProtocolVersion>,
    pub client: ClientInfo,
    pub capabilities: ClientCapabilities,
}

pub struct ProtocolWelcome {
    pub selected_version: ProtocolVersion,
    pub runtime: RuntimeInfo,
    pub capabilities: RuntimeCapabilities,
    pub limits: ProtocolLimits,
}

pub enum ProtocolBootstrapResponse {
    Welcome(ProtocolWelcome),
    Reject(ProtocolReject),
}

pub struct ProtocolReject {
    pub reason: ProtocolRejectReason,
    pub supported_versions: Vec<ProtocolVersion>,
}

pub enum ProtocolRejectReason {
    UnsupportedProtocolVersion,
    InvalidHello,
}
```

MVP runtime支持exact `1.0`。Hello最多16个unique versions与64个unique capability tokens；duplicate version/token拒绝InvalidHello，不做dedup。capability token grammar为1..64 bytes、lowercase ASCII `^[a-z][a-z0-9_]*$`；ClientInfo name/version分别最多128 bytes并使用safe visible text。Runtime选择双方共同支持的highest exact pair；无交集返回`ProtocolBootstrapResponse::Reject`并结束初始化。Welcome中的`runtime.protocolVersion`必须等于`selectedVersion`。不得根据implementation version字符串silent downgrade。

selected version绑定整个in-process adapter/transport connection。ordinary command/query/snapshot/event frame不重复version。未来minor只能发送该minor声明的variants；optional capability还必须出现在Welcome协商结果中。

V1.0 capability tokens：

```text
state_events
progress_events
runtime_snapshot
session_snapshot
paged_queries
command_catalog
interaction_resolution
session_fork
```

Welcome capabilities是Runtime支持与client advertized optional capability的intersection。Core in-process query返回同一集合。

### Frame Limits与Error Stage

- byte/frame/BOM/UTF-8/duplicate-key/JSON-shape failure发生在typed request进入dedup/router前，使用outer RuntimeDispatchError或对应Query/Snapshot/Subscription decode error；
- typed CommandRequest已被CommandId dedup owner接纳后，field/domain limit或semantic validation失败完成为`CommandCompletion::Rejected(InvalidArgument)`；
- encoder发现Runtime即将输出超过对应frame limit的required complete payload是internal invariant failure；producer必须在publication前受upstream count/byte cap约束；
- unpaged Query超过其complete result cap返回`QueryErrorCode::ResultTooLarge`，不能truncate；
- Snapshot不能truncate cancel target、Pending Interaction或active Item。构造会超limit的new live fact必须在apply前fail closed并用owner-defined terminal/diagnostic收口。

## Shared Scalar Carriers

### Runtime-Generated IDs

```text
AgentId    agt_<32 lowercase hex>
SessionId  ses_<32 lowercase hex>
TurnId     trn_<32 lowercase hex>
ItemId     itm_<32 lowercase hex>
RequestId  req_<32 lowercase hex>
EntryId    ent_<32 lowercase hex>
CommandId  cmd_<32 lowercase hex>
```

32 hex编码exact 16 CSPRNG bytes。prefix、length、lowercase和hex alphabet全部canonical；nil/all-zero payload拒绝。

Scope：

- AgentId、SessionId：durable Runtime catalog identity；
- TurnId、ItemId、RequestId、EntryId：Session-scoped uniqueness；
- CommandId：current process command correlation/dedup；
- Fork复制historical Turn/Item/Request/Entry IDs到new Session，future IDs fresh；
- ID不承载timestamp、sort order、authorization或storage ordinal。

### Interaction Key与Cursor

```text
InteractionResolutionKey  irk_<32 lowercase hex>
PageCursor                pc1_<43 base64url-no-pad chars>
```

Interaction key编码16 host CSPRNG bytes，raw value不得出现在Debug/Display/log/diagnostic。PageCursor编码exact 32 random bytes；v1 decoder只接受`pc1_`加43个base64url-no-pad chars，不接受其他opaque <=256-byte shape。ProtocolLimits中的256 bytes是outer defensive allocation cap和future-version ceiling，不扩大v1 canonical carrier。默认bounded cursor store最多4096 entries、idle expiry 15 minutes。cursor绑定exact query family/filter/sort/captured immutable snapshot；restart、expiry、eviction、scope unload或binding mismatch返回StaleCursor。

### Stable Symbolic Keys

PromptId、SkillId、ProviderId、ModelId、ToolName、WorkspaceRootKey和command path segment使用owner-defined、case-sensitive stable ASCII token。共同floor：

- 1..128 bytes（WorkspaceRootKey与command segment最多64 bytes）；
- visible ASCII `0x21..0x7e`；
- 禁止whitespace、control、quote、backslash与`/`，除非owner为ModelId明确允许`/`；
- owner可以施加更窄provider/schema grammar；
- decoder不能lowercase或Unicode-normalize。

ToolCallId、ProviderRequestId、ProviderResponseId、ProviderItemId与RedactedProviderCode是opaque validated ASCII：1..256 bytes，禁止whitespace/control/quote/backslash。provider没有native ToolCallId时，Runtime fallback使用`tcl_<32 hex>`。opaque value不假定UUID或global uniqueness。

### Revisions

```text
AgentRevision              ar_<decimal u64>
SessionDefinitionRevision  sdr_<decimal u64>
AgentMetadataRevision      amr_<decimal u64>
SessionMetadataRevision    smr_<decimal u64>
WorkspaceRevision          wr_<decimal u64>
```

范围`1..=u64::MAX`；decimal无leading plus/zero。prefix mismatch直接decode failure，不能把metadata token传给definition CAS。

其他`u64`（token count、usage count、model_calls、compaction_calls、cancel_epoch等）编码为canonical unsigned decimal JSON string，例如`"18446744073709551615"`。`u8/u16/u32`和受限millisecond Duration使用JSON integer。全部arithmetic checked；overflow拒绝或按owner规则产生diagnostic，不wrap/saturate。

### Timestamp与Duration

Timestamp exact format：

```text
2026-07-31T12:34:56.789Z
```

- UTC only；
- year `0001..9999`；
- exactly three fractional digits；
- uppercase `T`/`Z`；
- reject offset、leap second、missing/excess fraction和non-canonical equivalent；
- timestamps不要求跨provider或filesystem monotonic。

Public `Duration`编码non-negative JSON integer milliseconds，范围`0..=86_400_000`。Model logical retry的typed Retry-After继续使用owner的<=60s更窄规则。

### Money

```rust
pub struct Money {
    pub amount: Decimal,
    pub currency: CurrencyCode,
}
```

Wire：

```json
{"amount":"12.34","currency":"USD"}
```

规则：

- amount non-negative plain decimal；
- 无exponent、leading plus、redundant leading zero或trailing fractional zero；
- zero只编码`0`；
- 最多18 integer digits、9 fractional digits；
- currency exact `[A-Z]{3}`；
- checked decimal aggregation；
- Session aggregate按currency分组并按currency升序输出，最多8 currencies；mixed currency不得coerce为单值，overflow令该currency aggregate缺失并产生bounded diagnostic。

### Workspace Paths

Absolute Workspace input path编码platform-independent canonical RFC 8089 `file:` URI。Wire decoder先生成fields private的shared typed carrier `CanonicalFileUri { family, authority: Option<String>, decoded_utf8_path }`，并把它交给Workspace-owned `WorkspaceRootInput`形成host-neutral typed command。只有command进入Runtime completion owner后，Workspace才按host family checked-convert为durable `WorkspaceRootSpec { path: PathBuf }`；在macOS接收Windows/UNC carrier仍必须完成typed decode且不得改变wire bytes或借用host path parser猜语义。unsupported host family在Workspace command application阶段返回`InvalidArgument + DoNotRetry`，不是`TypedJsonError`或outer `RuntimeDispatchError`。

Canonical examples：

```text
file:///Users/alice/project
file:///Users/alice/%E9%A1%B9%E7%9B%AE
file:///C:/work/project
file://server/share/project
```

共同规则：

- wire必须使用hierarchical `file://<authority><path>` spelling；scheme exact lowercase `file`；无userinfo、port、query或fragment；禁止`file://localhost/...` alias；
- percent triplet使用uppercase hex；连续triplet先还原bytes再strict UTF-8 decode；NUL、encoded `/` (`%2F`)与encoded backslash (`%5C`)拒绝；
- RFC 3986 unreserved与ASCII `pchar`（除`%`）通常必须literal；percent-encoded unreserved/pchar拒绝。唯一disambiguation例外是POSIX path首segment形如ASCII letter + colon时，colon必须编码`%3A`，以区别drive family；
- non-ASCII scalar按UTF-8 bytes逐byte `%HH`；literal `%`编码`%25`，space/`?`/`#`分别编码`%20/%3F/%23`；
- decoded separator仅`/`；禁止backslash、repeated separator、`.`/`..` segment和非root trailing slash；emitter不执行Unicode normalization或filesystem canonicalization；
- encoded URI最多8192 bytes；v1拒绝无法lossless表示为UTF-8 URI的native path。

empty-authority classification precedence固定：literal path prefix `/[A-Za-z]:`先按drive candidate解析并要求drive uppercase；first segment中的`%3A`只作为POSIX disambiguator并decode为colon；其余single-leading-slash path为POSIX。由此native POSIX `/C:/work` canonical为`file:///C%3A/work`，而drive `C:/work` canonical为`file:///C:/work`。

Family规则：

- POSIX：`authority = None`，decoded path以single `/`开头；`file:///`是root；preserve segment case；
- drive：`authority = None`，URI path exact `/[A-Z]:/segment...`，decoded path去掉URI-specific leading slash后为`C:/segment...`；drive letter uppercase，colon literal；drive root exact `file:///C:/`；
- UNC：`authority = Some(lowercase_host)`；authority是total<=253 bytes的一到多个lowercase ASCII labels；labels以`.`分隔，每label 1..63 bytes，单字符label为`[a-z0-9]`，多字符label首尾为`[a-z0-9]`且中间只允许`[a-z0-9-]`；authority不能是`localhost`；decoded path不含leading slash且至少含non-empty share segment；share/segment case preserve；share root无trailing slash。

Decoder只接受上述canonical spelling，不自动lowercase host/drive、不移除dot segment、不decode platform alias，并在所有host上接受全部canonical family进入`WorkspaceRootInput`。Workspace在typed command application时若family不被current host支持，返回`InvalidArgument + DoNotRetry`，而不是把URI交给ambient `PathBuf` parser重解释。exact cross-platform vectors见[Wire V1 file URI carriers](../fixtures/wire-v1/public/carriers/file-uri.json)。

WorkspaceRelativePath使用forward slash：

- empty string表示root；
- 最多4096 UTF-8 bytes、256 segments；
- 禁止leading/trailing slash、backslash、NUL、empty segment、`.`、`..`、drive/UNC/platform prefix；
- wire lexical validation不替代Workspace containment/canonicalization。

## Bounded JSON与Schema

```rust
pub struct BoundedJsonValue(/* private */);
pub struct BoundedJsonObject(/* private root object */);
pub struct BoundedJsonSchema(/* private root object */);
```

`BoundedJsonValue/Object/Schema`使用独立于typed DTO field order的canonical dynamic JSON：

- array保持输入semantic order；object递归按decoded member-name的UTF-8 bytes升序输出；相同decoded key是duplicate并拒绝；
- string使用本章唯一escaping规则；key不执行Unicode normalization，byte-distinct key保持distinct；
- input与canonical output都必须分别满足encoded-byte cap；不能靠canonicalization接纳oversized input；
- semantic equality、hash/idempotency和byte-exact golden均比较canonical bytes；不得依赖parser map insertion order；
- Schema的`properties`等dynamic objects同样排序；`required`、`enum`和ordinary arrays保持声明顺序。

Dynamic JSON number不降为`f64`，而是解析为exact decimal并按下列算法产生唯一literal：

1. input必须符合JSON number grammar、literal<=64 bytes，显式base-10 exponent绝对值<=1,000,000；
2. 合并integer/fraction digits为coefficient，去leading zero；zero（包括`-0`与`0e...`）canonical为`0`；
3. `decimal_exponent = explicit_exponent - fraction_digit_count`；反复移除coefficient trailing zero并递增decimal_exponent；
4. `adjusted_exponent = decimal_exponent + coefficient_digit_count - 1`；当`-6 <= adjusted_exponent < 21`时输出plain decimal，否则输出`first_digit[.remaining_digits]e<adjusted_exponent>`；positive exponent不带`+`或leading zero；
5. non-zero negative value加`-`；canonical literal仍须<=64 bytes，否则拒绝。

因此`1`、`1.0`、`1e0`归一为`1`，`-0`归一为`0`，object key order与number spelling都不会制造第二个semantic encoding。provider若需要有限精度numeric type，adapter必须显式checked lowering；不得静默round后改写conversation/tool arguments。

上述64-byte/exponent/canonical cap只属于`BoundedJsonValue/Object/Schema` dynamic carrier。public typed frame的structural preflight对所有number执行JSON grammar与transport frame/depth/member bounds，但不能把embedded-number cap施加到将被selected-version output decoder忽略的unknown additive field；known typed numeric field随后仍按ordinary integer或owner scalar规则严格验证。该分离只提供receiver forward tolerance，Runtime encoder仍不得发送selected V1.0未声明的field。

constructor在分配完整semantic value前执行streaming/bounded validation。ordinary embedded JSON limits：

| Limit | Value |
| --- | ---: |
| encoded bytes | 65,536 |
| depth | 32 |
| array items | 256 |
| object members | 256 |
| string value bytes | 16,384 |
| number literal bytes | 64 |

ToolCall arguments必须是BoundedJsonObject；Tool result details可以是BoundedJsonValue。duplicate key拒绝。JSON number必须符合JSON grammar且literal <=64 bytes；NaN/Infinity不可表示。

BoundedJsonSchema使用JSON Schema Draft 2020-12 object form：

| Limit | Value |
| --- | ---: |
| encoded bytes | 65,536 |
| depth | 32 |
| total nodes | 4,096 |
| properties / required / enum items | 256 |
| regex text bytes | 1,024 |

所有JSON depth都以root value depth=1，直接child depth=parent+1；object member name不另算node。Schema `total nodes`计算root在内的每个object、array和scalar value各1个node；member name不计。`max_properties_required_or_enum_items = 256`分别限制任一`properties` object的direct members、任一`required` array items和任一`enum` array items，不对三类跨collection求和。ordinary BoundedJson的object/array limit同样是每个container的direct members/items。

v1允许local fragment `$ref`，拒绝remote/network ref。schema owner必须使用bounded/non-backtracking regex implementation；不能在decode或validation期间访问network/filesystem。Tool与Model owner定义支持keyword subset、provider lowering和semantic validation；wire owner只保证bounded object。

## ProtocolLimits V1.0

```rust
pub struct ProtocolLimits {
    pub transport: TransportLimits,
    pub text: TextLimits,
    pub catalog: CatalogLimits,
    pub paging: PagingLimits,
    pub prompt: PromptWireLimits,
    pub workspace: WorkspaceWireLimits,
    pub queues: QueueLimits,
    pub interaction: InteractionLimits,
    pub observation: ObservationLimits,
    pub embedded_json: EmbeddedJsonLimits,
}

pub struct TransportLimits {
    pub max_request_bytes: u32,
    pub max_response_bytes: u32,
    pub max_runtime_snapshot_bytes: u32,
    pub max_session_snapshot_bytes: u32,
    pub max_state_event_bytes: u32,
    pub max_progress_event_bytes: u32,
    pub max_json_depth: u16,
    pub max_object_members: u16,
    pub max_array_items: u32,
    pub max_string_bytes: u32,
}

pub struct TextLimits {
    pub max_text_intent_bytes: u32,
    pub max_command_input_bytes: u32,
    pub max_command_output_bytes: u32,
    pub max_display_name_bytes: u16,
    pub max_description_bytes: u32,
    pub max_public_summary_bytes: u32,
    pub max_diagnostic_code_bytes: u16,
    pub max_diagnostic_message_bytes: u16,
}

pub struct CatalogLimits {
    pub max_command_path_segments: u16,
    pub max_command_arguments: u16,
    pub max_command_catalog_entries: u16,
}

pub struct PagingLimits {
    pub max_page_size: u16,
    pub max_page_cursor_bytes: u16,
}

pub struct PromptWireLimits {
    pub max_skills_per_intent: u16,
    pub max_user_message_parts: u16,
    pub max_message_part_bytes: u32,
    pub max_user_message_bytes: u32,
}

pub struct WorkspaceWireLimits {
    pub max_workspace_roots: u16,
    pub max_absolute_path_uri_bytes: u32,
    pub max_relative_path_bytes: u32,
    pub max_relative_path_segments: u16,
}

pub struct QueueLimits {
    pub max_submit_admissions: u16,
    pub max_steers: u16,
    pub max_follow_ups: u16,
}

pub struct InteractionLimits {
    pub max_tool_approval_options: u16,
    pub max_interaction_questions: u16,
    pub max_choices_per_question: u16,
    pub max_answer_text_bytes: u32,
    pub max_interaction_answer_bytes: u32,
    pub max_interaction_view_bytes: u32,
}

pub struct ObservationLimits {
    pub max_active_items: u16,
    pub max_item_view_bytes: u32,
    pub max_pending_interactions: u16,
    pub max_snapshot_diagnostics: u16,
    pub max_query_diagnostics_per_scope: u16,
}

pub struct EmbeddedJsonLimits {
    pub value: JsonValueLimits,
    pub schema: JsonSchemaLimits,
}

pub struct JsonValueLimits {
    pub max_encoded_bytes: u32,
    pub max_depth: u16,
    pub max_array_items: u16,
    pub max_object_members: u16,
    pub max_string_bytes: u32,
    pub max_number_literal_bytes: u16,
}

pub struct JsonSchemaLimits {
    pub max_encoded_bytes: u32,
    pub max_depth: u16,
    pub max_nodes: u32,
    pub max_properties_required_or_enum_items: u16,
    pub max_regex_bytes: u16,
}
```

### Transport

| Field | Value |
| --- | ---: |
| max_request_bytes | 1,048,576 |
| max_response_bytes | 8,388,608 |
| max_runtime_snapshot_bytes | 8,388,608 |
| max_session_snapshot_bytes | 8,388,608 |
| max_state_event_bytes | 8,388,608 |
| max_progress_event_bytes | 65,536 |
| max_json_depth | 64 |
| max_object_members | 256 |
| max_array_items | 4,096 |
| max_string_bytes | 262,144 |

### Text与Catalog

| Field | Value |
| --- | ---: |
| max_text_intent_bytes | 131,072 |
| max_command_input_bytes | 32,768 |
| max_command_output_bytes | 65,536 |
| max_display_name_bytes | 256 |
| max_description_bytes | 8,192 |
| max_public_summary_bytes | 8,192 |
| max_diagnostic_code_bytes | 64 |
| max_diagnostic_message_bytes | 2,048 |
| max_command_path_segments | 16 |
| max_command_arguments | 64 |
| max_command_catalog_entries | 1,024 |

### Paging、Prompt与Workspace

| Field | Value |
| --- | ---: |
| max_page_size | 200 |
| max_page_cursor_bytes | 256 |
| max_skills_per_intent | 32 |
| max_user_message_parts | 64 |
| max_message_part_bytes | 131,072 |
| max_user_message_bytes | 524,288 |
| max_workspace_roots | 16 |
| max_absolute_path_uri_bytes | 8,192 |
| max_relative_path_bytes | 4,096 |
| max_relative_path_segments | 256 |

### Queue、Interaction与Observation

| Field | Value |
| --- | ---: |
| max_submit_admissions | 16 |
| max_steers | 32 |
| max_follow_ups | 32 |
| max_tool_approval_options | 16 |
| max_interaction_questions | 32 |
| max_choices_per_question | 64 |
| max_answer_text_bytes | 16,384 |
| max_interaction_answer_bytes | 65,536 |
| max_interaction_view_bytes | 131,072 |
| max_active_items | 64 |
| max_item_view_bytes | 65,536 |
| max_pending_interactions | 16 |
| max_snapshot_diagnostics | 50 |
| max_query_diagnostics_per_scope | 100 |

### Embedded JSON

`ProtocolLimits.embedded_json`返回前节的65,536/32/256/256/16,384/64与schema 65,536/32/4,096/256/1,024 constants。

limits是hard maxima，不是host recommendation。Runtime可以配置更小operational queue/admission caps，但Welcome必须返回effective values，且同一adapter lifetime内不得增大；future reload只能缩小并需new capability/version contract，MVP不hot reload ProtocolLimits。

Snapshot aggregate proof同时要求：每个ItemView encoded bytes<=65,536、每个InteractionView<=131,072、active Items<=64、Pending Interactions<=16，且最终Runtime/Session Snapshot encoded bytes<=8,388,608。前四项把两类主要变长集合限制在约6 MiB，剩余queue/usage/diagnostic/structural overhead仍由最终frame preflight兜底。producer不能仅检查count而跳过per-view与whole-frame check。

## Conversation JSONL Shared V1 Rules

Conversation Storage拥有Stored semantic DTO和replay projection；本节冻结共同wire/scanner floor。六种body与byte-exact field projection见[Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md)。

### Physical Records

```text
record 1  {"type":"session_header","data":{...}} LF
record 2+ {"type":"entry","data":{...}} LF
```

- `session_header.data.formatVersion = 1`；
- writer使用LF；reader接受LF或CRLF；
- Header最多65,536 bytes excluding line ending；
- each entry最多1,048,576 bytes excluding line ending；
- file最多1,073,741,824 bytes且最多1,000,000 complete entry records；file byte cap包含final unterminated tail；
- canonical writer使用本模块JSON rules与stable field order；reader的strict Header指schema/version/typed scalar strict，不要求input为compact canonical bytes，JSON grammar允许的insignificant whitespace与member order在line cap内可接受；
- header unknown version、malformed、oversized或invalid UTF-8使Load fail closed，禁止append/truncate；
- writer v1绝不向higher-version file append。

Storage record JSON在line byte cap之外还使用：depth<=64、每object members<=256、每array items<=4,096、total nodes<=16,384、单string decoded UTF-8 bytes<=524,288。unknown additive field的value也必须在这些limits内完成bounded skip；不能先materialize unbounded DOM再ignore。

### Bounded Scanner

scanner不得使用unbounded `read_line`/`read_until`。open首先对包含tail的physical file byte count执行1 GiB cap；超过时由Conversation Storage返回`HistoryTooLarge` internal error并映射public `DurableStateTooLarge`，不进入partial-tail truncate。stat不可用时scanner在累计读取跨cap时执行同一fail-closed。byte cap通过后，按<=65,536-byte chunks读取，每line buffer最多`applicable_limit + 1`：

- record 1是strict Header exception：oversized/malformed/invalid UTF-8/unknown type或unsupported version立即fail Load，禁止应用entry-line skip-and-continue；
- entry line在newline前超过limit：停止增长buffer，stream-discard到LF；若完整line结束则记录oversized_line diagnostic并继续；
- complete malformed/invalid UTF-8/unknown variant line：skip + diagnostic，继续；
- final bytes没有LF且whole file未超cap：无论是否构成valid JSON都视为partial tail；read-only replay忽略，writable open在valid v1 header和exclusive lease下只truncate到last LF；
- newline-terminated malformed/oversized/unknown line永不truncate或rewrite；
- later valid line可以成为orphan root并按Conversation Storage规则隔离；
- replay在准备接受第1,000,001个complete entry时由Conversation Storage返回`HistoryTooLarge`；不能把未扫描suffix误报为普通complete history。

### Diagnostics

每次Load对每个diagnostic fact先checked-increment其per-code total counter，并只保留physical order前100条ordered detail。若至少一条detail被省略，再追加一个`diagnostics_truncated` summary；summary记录`omitted_detail_count`以及全部observed source facts（含已保留100条）的per-code total counts；synthetic summary自身不递增`diagnostics_truncated` total。因此101条`malformed_json`的aggregate是101、omitted detail是1，而不是aggregate 1。每条detail/summary message最多512 bytes；不含raw line、absolute path、raw OS error、provider body/header、Tool output、credential或hidden reasoning。

V1 exact code set如下；semantic emission precedence由[Conversation Storage](conversation-storage.md#tolerant-replay)拥有：

```text
partial_tail
oversized_line
invalid_utf8
malformed_json
invalid_entry
unknown_record_variant
unknown_entry_variant
duplicate_entry_id
missing_parent
session_mismatch
invalid_relation
invalid_contribution_stamp
duplicate_contribution_stamp
invalid_tool_exchange
invalid_interaction_relation
invalid_compaction_marker
diagnostics_truncated
history_too_large
```

## Writer Rules

```text
semantic owner validates/redacts/bounds
→ live owner allocates identity and applies fact
→ Recorder validates exact immutable entry
→ bounded encode into memory
→ write_all(compact_json + LF)
```

Recorder encode buffer最多entry limit + LF。oversized or encode failure在任何file write前失败并令RecordingHealth Degraded。partial/unknown write同样Degraded；不retry、不创建segment、不写later suffix。

下列是跨owner必须满足的bounded outcome约束；exact error/disposition constructor仍由链接的semantic owner定义并在consumer同步时冻结：

- oversized user input由Runtime/Prompt在Input apply前映射InvalidArgument；
- provider assistant/arguments/reasoning超cap由ModelGateway validation失败，不apply partial assistant；
- known Tool output超cap由Tools归约为truthful bounded Failed ToolResult，不silent truncate或Abandoned；
- Interaction request/answer超cap由Turn/Interaction在request publication/Resolve前拒绝；
- Compaction summary/provenance超cap由Compaction拒绝install；
- Recorder revalidation是defense in depth，不是正常payload reduction seam。

## Compatibility与Migration

Public protocol：

- major改变允许breaking representation；
- minor只做selected-version-compatible additive field/variant；
- input新增optional field必须在new minor中协商，old Runtime仍拒绝unknown input；
- output新增field可由old client忽略，但new variant必须new minor/capability；
- no selected version overlap时停止bootstrap。

Conversation storage：

- v1 reader只解释formatVersion 1；
- unknown additive fields可忽略；
- unknown fact/enum line skip，不将其语义化；
- higher format不能由v1 writer降级append；
- migration必须显式、staged、validated、backup/atomic publication；MVP无automatic migration或repair utility。

## Golden与Negative Vectors

实现前必须建立byte-exact fixtures：

Public：

- Hello/Welcome/Reject与capability intersection；
- nested Submit、Completed(TurnStarted)、Rejected(CommandError)、outer dispatch error；
- nullable Option、pure unit enum、mixed enum unit/newtype/struct；
- first/next/stale PageCursor；
- empty/active SessionSnapshot，完整queue targets，ToolApproval/UserQuestion；
- Snapshot/State/Progress/Closed四种EventFrame；
- TurnCompleted/Interrupted/Failed distinct variants；
- typed ID/revision、Timestamp、Duration、Money、file URI/relative path；
- unknown input field reject、unknown output field tolerate、unknown variant fail、duplicate key reject；
- every advertised limit exact-boundary and boundary-plus-one。

Storage：

- header-only、每种entry body、complete Tool exchange、两种Interaction、Compaction provenance；
- malformed middle line、invalid UTF-8、unknown additive field、unknown variant、duplicate ID、orphan、invalid marker；
- final partial tail；
- generated Header 64 KiB boundary/+1；
- oversized complete entry line followed by valid recoverable line；
- generated physical file 1 GiB and 1,000,000-entry boundary/+1 recipes；
- diagnostics cap/aggregate；
- expected accepted EntryIds、sanitized conversation、diagnostics和writable truncation offset。

Oversized fixture使用recipe在test生成，不提交1 MiB/1 GiB blob。Decoder assertions比较semantic result，不比较input member order；canonical encoder golden比较exact bytes。authoritative vectors、public target/expectation manifest、conversation replay expectations和all-limit recipes位于[Wire V1 Fixtures](../fixtures/wire-v1/README.md)；`verify.py`只校验资产结构，首个Rust protocol/storage crate必须执行semantic conformance。

## Test Matrix

- duplicate key at every nesting depth；
- invalid UTF-8/BOM/surrogate/control/ANSI；
- typed ID prefix/length/hex/nil/cross-type rejection；
- revision prefix/zero/leading-zero/u64 overflow；
- u64 decimal string and checked aggregate；
- Timestamp exact millis/offset/leap-second rejection；
- Money canonical decimal/currency/mixed-currency/overflow；
- PageCursor expiry/eviction/restart/scope mismatch；
- file URI POSIX/drive/UNC与non-UTF-8 rejection；
- relative path traversal/backslash/segment limits；
- BoundedJson bytes/depth/member/item/string/number boundaries；
- local/remote ref与schema depth/node/regex limits；
- complete Snapshot never truncates actionable state；
- JSONL header/version/CRLF/partial/oversized/malformed/unknown behavior；
- Recorder bounded encode before first write；
- diagnostics redaction与100+aggregate cap；
- schema/fixture round-trip and fuzz/property decode without panic/unbounded allocation。

## 开放边界

Wire v1不定义：

- remote transport authentication/authorization；
- blob/artifact upload/download；
- non-UTF-8 Workspace paths；
- storage format v2 migration tool；
- remote JSON Schema ref；
- unbounded Tool result/history export；
- public event replay cursor。

这些能力未来必须建立独立capability或format decision，不能扩大v1 generic JSON limits。
