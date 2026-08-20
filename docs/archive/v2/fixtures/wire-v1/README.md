# Wire V1 Fixtures

> Archived V2 evidence. These fixtures are retained for historical/conformance reference and are not run by default.

本目录保存ADR 0134、[Wire Schema](../../pre-reset/modules/wire-schema.md)和[Conversation JSONL Format V1](../../pre-reset/formats/conversation-jsonl-v1.md)的byte-exact conformance vectors。fixture文件是规范的一部分，不是说明性伪代码。最低结构自检：

```bash
python3 docs/archive/v2/fixtures/wire-v1/verify.py
```

## Harness Contract

- `public/valid/*.json`：必须通过duplicate-key-aware、bounded、selected-version `1.0` decoder。canonical vector的decode/re-encode必须byte-for-byte等于原文件去除最后一个LF后的bytes；V1明确接受的noncanonical representation（例如known Option field missing）必须在manifest声明`canonicalReencodePath`，encoder输出该canonical target。
- `public/invalid/input/*.json`：必须在domain dispatch前拒绝；filename给出首要拒绝原因。
- `public/invalid/output/*.json`：模拟收到非法Runtime frame时必须报protocol error；Runtime encoder/preflight也不得发送这些bytes。
- `public/compat/*.json`：防御性forward-tolerance输入；按`public/manifest.json`删除ignored pointers后必须canonical re-encode到指定v1 target，Runtime v1 sender仍不得主动发送future field。
- `public/carriers/*.json`：shared scalar/path的valid canonical round-trip与typed rejection sets；每个set在manifest声明target，Rust runner逐条执行。
- `public/manifest.json`：为每个public vector声明target DTO、direction、decode stage/code、canonical target，以及incremental implementation `status = active | pending`与first owning `slice`。Rust runner必须处理全部active vectors；pending vectors保持可见并归属M2/M8/M9/M10/M11，M11要求pending数量归零。
- `conversation/golden/*.jsonl`：每个complete line必须decode并canonical re-encode为相同bytes；最后一行必须以LF结束。
- optional `conversation/golden/<name>.expected.json`：声明跨行ordering/protocol assertions；存在sidecar时Rust runner必须执行，不能只做per-line codec round-trip。
- `conversation/corruption/*.jsonl`：按同名`.expected.json`验证strict Header、tolerant entry scan、diagnostic和tail policy；这些输入不能被canonical re-encode覆盖。
- `recipes/*.json`：描述不直接提交巨型blob的boundary/+1生成向量；`protocol-limit-cases.json`对Welcome每个advertised leaf应用统一owner-validator boundary/+1 rule，`boundary-cases.json`保存需要组合结构的integration recipes。generator必须stream输出并校验exact byte/count目标。
- typed DTO object按semantic Rust field declaration order编码；dynamic `BoundedJson*` object递归按decoded key UTF-8 bytes排序并使用Wire-owned number/string canonicalization；field名camelCase、enum值snake_case、payload enum使用`type/data`；所有Option field显式编码为value或null。
- comparison按canonical UTF-8 bytes执行，不经过pretty print、Unicode normalization或platform newline转换；harness不得对typed object做generic key sort，也不得跳过dynamic object的required recursive sort。

## Public Coverage

- ProtocolHello/Welcome/Reject、MVP exact 1.0 overlap/no-overlap、capability intersection（unknown capability drop）及全部ProtocolLimits；
- nested RuntimeCommand、CommandCompletion、CommandError；
- Snapshot/State/Progress/Closed EventFrame；
- typed IDs/revisions、u64 decimal strings、Timestamp、Money、nullable fields；
- duplicate key、unknown input field/variant、noncanonical ID与number-vs-string rejection。

## Conversation Coverage

- header-only file；
- complete User → Assistant ToolCall → Tool result → final Assistant exchange；
- UserQuestion request/resolution、`PreExecution + Succeeded` Tool result与Compaction；
- malformed middle line、unknown body variant、duplicate EntryId、orphan relation和final partial tail；
- Header/entry line/file size与complete-entry count boundary/+1 recipes。

`verify.py`只做不复制协议实现的structural checks：JSON duplicate/compact byte stability、explicit Header/Entry declaration order、known dynamic arguments key order、fixture manifest completeness、key-scoped Runtime ID、revision grammar、canonical PageCursor bytes、JSONL field order/session identity、Welcome constants、cross-fixture exact-version/capability negotiation、LF/CRLF与partial-tail offset。dynamic decimal canonicalization的exact execution由Rust codec消费literal recipes。semantic decode/replay、negative stage/code和boundary recipe execution由P1-2 closure后的首个Rust wire/storage conformance tests消费；不得另写一套Python domain decoder成为第二schema owner。

`conversation/corruption/*.expected.json`是fixture-harness metadata，不是Runtime public wire。共同required fields：`load`、`acceptedEntryIds`、`selectedPath`、`sanitizedModelMessages`、`historicalItemIds`、`diagnostics`或`diagnosticsByMode`、`tail`；strict Header failure还必须含`openedSessionId`与typed storage `error`，其四个projection arrays仍显式为空。`acceptedEntryIds`表示required body/typed scalar decode成功、session匹配Header并进入collision guard/history graph的IDs；session mismatch/unknown variant/malformed line不reserve identity，parent或domain relation invalid的entry仍可在其中但不得污染sanitized model projection。`truncateOffset`是从file start起、保留最后一个complete LF后的zero-based byte offset。

`sanitizedModelMessages`使用closed structured harness DTO，不使用冒号分隔字符串：

```json
{"role":"user","content":[{"type":"text","data":{"text":"..."}}]}
{"role":"assistant","content":[{"type":"reasoning","data":{"text":null,"summary":"brief","encrypted":null,"signature":null,"providerItemId":null}},{"type":"text","data":{"text":"..."}},{"type":"tool_call","data":{"toolCallId":"call_1","name":"read_file","arguments":{}}}]}
{"role":"tool","toolCallId":"call_1","content":{"parts":[{"type":"text","data":{"text":"..."}}]}}
```

User Input与Steer都投影为`role = user`；source差异由input JSONL body与historical Item assertion处理，不伪造成model role。assistant content允许`reasoning | text | tool_call`并保持ModelGateway order；reasoning data使用ReasoningContent但不含ItemId；`role = tool`使用Tools-owned ToolResultContent。object fields和dynamic arguments仍遵守canonical encoding。

`jq`只能用于正向JSON syntax smoke check；它不能检测duplicate keys，也不能替代conformance decoder。
