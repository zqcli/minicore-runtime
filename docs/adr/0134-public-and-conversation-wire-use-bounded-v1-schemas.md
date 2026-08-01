# ADR 0134：Public Protocol与Conversation Recording使用Bounded V1 Wire Schema

状态：Accepted
日期：2026-07-31

## 背景

ADR 0133冻结了Runtime public semantic payload，ADR 0124/0126/0131/0132冻结了conversation-only recording、tolerant replay与Compaction semantic contract，但实现仍缺少一个可生成serde codec、bounded decoder和golden fixtures的wire owner：

- public JSON field casing、enum tagging、version negotiation和unknown data policy未冻结；
- ID、revision、Timestamp、Duration、Money、path、cursor和Interaction key没有canonical text carrier；
- `ProtocolLimits`被公开但没有字段或默认值；
- Tool arguments/details和JSON Schema仍使用unbounded JSON value；
- conversation JSONL没有exact envelope、line/file limit、oversized complete-line和invalid UTF-8规则；
- public input、public output和corrupt storage不能共享同一种unknown-field/variant策略；
- byte-exact golden vectors与bounded corruption fixtures无法编写。

把这些规则分散到Runtime、Workspace、Prompt、Tools、ModelGateway和Conversation Storage会产生互相不兼容的serde默认值，也会让Transport Adapter承担本应由Runtime保证的DoS/redaction边界。相反，建设通用业务“Common”模块又会错误吞并各domain owner。

## 决策

1. 新增[Wire Schema](../modules/wire-schema.md)作为唯一serialization与bounded-decode owner。它只拥有：
   - public JSON v1.0 representation与version/capability negotiation；
   - shared scalar carriers、typed ID/revision text、Timestamp/Duration/Money/path encoding；
   - `ProtocolLimits`与bounded JSON/schema wrappers；
   - conversation JSONL共同lexical rules、format-v1 envelope/line/file limits和scanner behavior；
   - compatibility、redaction floor与golden/negative vector requirements。

   Runtime、Workspace、Prompt、Tools、ModelGateway、Turn/Interaction、Compaction和Conversation Storage继续拥有semantic validation、authorization、state transition和record meaning。禁止创建generic Error/Common/domain registry或第二套Stored semantic tree。

2. JSON v1使用UTF-8、无BOM、`camelCase` object fields与`snake_case` enum values。包含任意payload variant的enum统一使用adjacent tagging：`{"type":"...","data":...}`；pure unit enum使用snake_case string。canonical encoder输出compact JSON、known fields按schema declaration order、所有`Option` field显式输出value或`null`。object member order不具语义，decoder拒绝duplicate keys。

3. public方向兼容策略不对称：
   - client→Runtime input拒绝unknown field、unknown variant、duplicate key和wrong shape；
   - Runtime→client output允许client忽略unknown object field，但selected version/capability之外的unknown variant是protocol error，不能静默跳过会改变reducer语义的event/outcome；
   - capability token数组中的unknown token可以忽略；
   - command idempotency比较decoded canonical semantic value，不比较raw JSON bytes、whitespace或member order。

4. public bootstrap选择highest mutually supported exact `(major, minor)`；MVP只支持`1.0`。无交集返回wire-only typed rejection并停止初始化，不silent downgrade。selected version绑定整个adapter/connection；普通frame不重复version。future variant需要new minor，optional variant还需要negotiated capability。

5. Runtime生成identity使用type-prefixed、lowercase、128-bit CSPRNG hex：`agt_`、`ses_`、`trn_`、`itm_`、`req_`、`ent_`和`cmd_`。Interaction resolution key使用`irk_`加32 hex。ID prefix是decode-time type check，不是authorization或ordering。PromptId、SkillId、ProviderId、ModelId、ToolName和WorkspaceRootKey是owner-defined stable ASCII token，不强制随机UUID。provider-native opaque ID保留validated printable ASCII；Runtime fallback使用对应typed random prefix。

6. CAS revision使用type-prefixed canonical decimal string：Agent `ar_`、Session definition `sdr_`、Agent metadata `amr_`、Session metadata `smr_`、Workspace `wr_`。revision范围`1..=u64::MAX`，无leading zero。所有其他public/stored `u64`使用canonical decimal string，避免JavaScript safe-integer截断；`u8/u16/u32`在自身range内使用JSON integer。

7. Timestamp固定为UTC RFC3339 millisecond text `YYYY-MM-DDTHH:mm:ss.SSSZ`。Duration固定为non-negative integer milliseconds。Money固定为non-negative canonical decimal string加uppercase three-letter currency；aggregation按currency分组并使用checked decimal arithmetic，不能silent mix currency或overflow。

8. Workspace absolute input path使用canonical RFC 8089 `file:` URI；Workspace relative path使用forward-slash UTF-8 relative text。Workspace owner继续负责canonicalization、containment、symlink/trust/authorization；wire owner只负责text carrier和lexical bounds。v1不lossy编码non-UTF-8 native path。

9. raw `serde_json::Value`不能越过public/storage seam。Wire owner提供`BoundedJsonValue`、`BoundedJsonObject`和`BoundedJsonSchema` constructor/codec；它们在materialization前验证encoded bytes、depth、member/item count、string和number literal。Tool/Model owner继续验证arguments与schema业务语义。remote `$ref`、network fetch和unbounded/unsafe regex不属于v1。

10. `ProtocolLimits`使用本ADR冻结的nested fields/defaults并由`ProtocolWelcome`返回。任何完整required Snapshot在publication前都必须可落入limits；不能截断queue target、Pending Interaction或active Item。无法分页的query超限返回`ResultTooLarge`，incoming request在typed command被dedup owner接纳前后分别遵守ADR 0133的outer/CommandError分层。

11. conversation JSONL format v1使用一行一个adjacent-tagged record：首行`session_header`，后续`entry`。Header中的`formatVersion = 1`是file storage major；reader/writer只接受exact v1，不向更高版本file append。writer发LF；reader接受LF/CRLF。valid v1 Header之后的entry unknown additive object fields忽略，unknown record/body/leaf variant使该完整entry line被skip并产生bounded diagnostic；Header unknown/malformed始终fail closed。不能把unknown conversation fact映射为generic Unknown。

12. JSONL scanner按bytes有界读取：Header最多64 KiB，entry line最多1 MiB（均不含line ending），file最多1 GiB且最多1,000,000 complete entry lines。oversized complete line必须stream-discard到newline、diagnose、skip并继续；final unterminated tail始终忽略，writable open只可truncate该tail到last LF。newline-terminated malformed/oversized/unknown line不得被truncate或repair。invalid UTF-8 complete line按malformed line隔离。

13. Recorder在触碰file前完成semantic validation和bounded in-memory encoding。encode/size/write/partial-or-unknown-write failure按ADR 0126令Recorder Degraded，不retry、不写later suffix。domain producer必须在live apply前把可能超limit的Model/Tool/Interaction/Compaction payload归约为truthful bounded semantic result；不能依赖Recorder late failure维持public Snapshot bounds。

14. diagnostics只使用owner allowlisted code与bounded redacted message。不得包含raw line、absolute path、raw OS/provider error、headers/body、Tool raw output、credential或hidden reasoning。每次replay对每个fact先递增per-code total counter，只保留physical order前100条detail；若有detail被省略，再追加一个`diagnostics_truncated` summary，携带omitted detail总数与全部observed facts的per-code totals。

15. exact wire、format和corruption behavior必须有byte-exact golden vectors、schema validation、round-trip semantic equality与boundary-plus-one negative vectors。object member order只用于canonical encoder fixture，不成为decoder compatibility requirement。

## 结果

- public protocol和conversation recording可以共享一套scalar/limit/JSON lexical implementation，而不共享业务owner。
- typed prefixes在wire boundary阻止CommandId/TurnId、definition/metadata revision等cross-type误用。
- public clients、Runtime input和corrupt JSONL使用明确不同的unknown-data策略。
- JSONL malformed/oversized middle line不brick later recoverable facts，也不会触发unbounded allocation。
- limits会约束MVP最大request、Turn Item数、Interaction/queue和stored payload；需要更大blob必须未来引入独立artifact/blob capability，而不是放宽所有JSON frame。
- exact schema是breaking contract。字段/variant变化必须遵守selected protocol minor或storage format migration，不能靠serde默认行为漂移。

## 不采用

- 裸UUID用于所有ID：无法在JSON boundary拒绝cross-type copy/paste，且Prompt/Skill/Provider等配置key并非随机entity。
- JSON number承载所有`u64`：JavaScript/JSON client可能在`2^53`以上静默丢精度。
- untagged或externally tagged enum：shape歧义且难以统一unit/newtype/struct variants。
- input/output/storage统一ignore unknown：public mutation会fail open，storage又可能因future fact被错误解释。
- unbounded `serde_json::Value`或`read_line`：无法建立DoS与corrupt-file allocation上限。
- 通用`Common` domain module：wire representation不能成为Agent/Session/Tool/Model semantic owner。

## 修订关系

本ADR完成ADR 0133明确后置的public wire/limits工作，并为ADR 0124、0129、0131和0132的conversation JSONL encoding提供共同基础。各Stored DTO的semantic owner位于对应canonical module，exact format-v1 representation由[Conversation JSONL Format V1](../formats/conversation-jsonl-v1.md)拥有，Conversation Storage负责record/replay消费；不得从本ADR反向推导执行语义。
