# Wire V1 Boundary Recipe Semantics

本目录的recipe是future Rust conformance runner的machine-readable生成合同，不是示意性名称。runner不得自行改变counting、identity或relation规则。

## Shared Rules

- 所有large cases直接stream到bounded sink/temp file；1 MiB/1 GiB/1,000,000-entry输入不得先完整驻留内存。
- deterministic generated Runtime IDs以unsigned 128-bit counter `1..`编码为exact 32 lowercase hex；各prefix使用独立counter，禁止all-zero payload。Header SessionId固定，所有valid Entry.sessionId匹配Header。
- typed DTO按Rust declaration order编码；dynamic BoundedJson key按decoded UTF-8 bytes排序；number/string使用Wire Schema canonical rules。
- JSON depth以root value为1；每层object/array child加1。object key不是node。node是object、array或scalar value，各计1。
- object-member/array-item limit均计算单个container的direct children。Schema的properties/required/enum三类分别计算，不跨collection求和。
- `targetBytesExcludingLineEnding`不含LF/CRLF；`targetBytes`包含完整document/file bytes。
- boundary generator先满足所有更窄limit；若目标专门测试某owner leaf，其他owner validators使用合法最小值。`expected.*Accepted`只表示该case声明的owner layer，除非明确写`load`/`entryAccepted`等integration result。

## Conversation Generators

`headerWithTrailingWhitespace`
: 生成无unknown field的canonical v1 Header JSON，并在top-level value结束后、LF前追加JSON grammar允许的ASCII SP whitespace直到exact line bytes。strict Header semantic DTO不变；65,536-byte boundary可load，65,537-byte case由line scanner在parse前fail closed。

`entryWithBoundedUnknownPadding`
: 生成一个合法Input User entry，在entry data末尾增加可忽略`futurePadding` array；string分段<=524,288 bytes，array/items/nodes/depth合法，并调整最后一段达到exact line bytes。

`entryWithBoundedUnknownPaddingThenCanonicalEntry`
: 先输出目标entry+LF，再输出fresh Turn/Item/Entry IDs的合法root User entry+LF；用于证明oversized scanner在LF后恢复。

`canonicalHeaderAndOneEntryThenAsciiPartialTail`
: 输出canonical Header+LF和一个合法root User entry+LF，再stream ASCII partial tail直到exact physical file bytes；tail无LF。file cap必须先于tail truncate判断。

`streamCanonicalEntryChain`
: 输出canonical Header后，按counter生成N条Input User entry chain。第1条`parentId=null`；每个后续entry的parent是physical previous EntryId。每条使用fresh EntryId、TurnId和ItemId、session匹配Header，因此只有一个root且不存在same-Turn双Input、duplicate或orphan。payload固定最小ASCII，使1,000,001条仍低于1 GiB，count cap是首要失败。

`rawBytesThenCanonicalEntry`
: 输出recipe hex bytes，再输出fresh合法root User entry+LF；raw bytes中声明的LF是scanner recovery boundary。

`entryUnknownAdditiveNestedArrays` / `entryUnknownAdditiveObject` / `entryUnknownAdditiveArray` / `entryUnknownAdditiveString` / `entryUnknownAdditiveTree`
: 生成合法root User entry，并在entry data末尾加入一个可忽略`futureField`，只改变目标record structural metric。known fields、relations和所有非目标metrics保持合法；boundary接受且无diagnostic，+1产生`invalid_entry`。

`headerThenMalformedLinesThenCanonicalEntry`
: 输出合法Header、exact N条newline-terminated `{"type":"entry","data":` malformed lines，再输出fresh合法root User entry。每条bad line产生`malformed_json` total；detail按physical order保留前100，另追加typed `diagnostics_truncated` summary。expected `totals`使用按code bytes升序的`[{"code":"...","count":N}]`，count包含已保留和omitted facts；summary自身不计数。

## Bounded JSON Generators

以下generator直接测试`BoundedJsonObject` constructor，不构造incomplete conversation exchange：

- `boundedJsonObject`：root object，canonical keys `k000...`，用合法string values调整direct member count；
- `boundedJsonRawInputWithWhitespace`：semantic root object保持很小，使用JSON grammar允许的insignificant whitespace把raw input调到target；canonical output明确低于cap；
- `boundedJsonObjectCanonicalExpansion`：使用按key排序的多个exact decimal values（例如`1e-6` canonical为`0.000001`），使raw input<=65,536而canonical output精确达到target；
- `boundedJsonObjectWithArray`：root `{"value":[...]}`，只改变direct array items；
- `boundedJsonObjectWithNestedArrays`：root object depth=1，`value` child逐层嵌套array，按shared depth rule达到target；
- `boundedJsonObjectWithString`：root `{"value":"..."}`，decoded ASCII bytes达到target；
- `boundedJsonObjectWithNumber`：root `{"value":N}`；boundary使用无leading/trailing zero且canonical output正好64 bytes的positive integer，+1使用65-byte literal。

encoded-byte target计算完整canonical root object，而非只计算value。所有非目标metrics保持低于limit。`bounded_json_canonicalization`是literal exact vector：dynamic keys排序，`1.0/1e0/-0`分别归一为`1/1/0`；exponent +1 case在allocation/provider lowering前拒绝。

## Schema Generators

`localDraft202012Schema`
: root object至少含`{"$schema":"https://json-schema.org/draft/2020-12/schema"}`，仅使用v1 supported local keywords。按case调整nodes或单个properties/required/enum collection；dynamic keys canonical sort。

`schemaRawInputWithWhitespace`
: semantic schema保持很小，以insignificant JSON whitespace把raw input调到target，canonical output明确低于cap。

`schemaCanonicalExpansion`
: 使用supported numeric keyword中的exact decimal spellings和bounded description padding，使raw input<=65,536而canonical output精确达到target；用于独立测试canonical-output byte gate。

`nestedLocalDraft202012Schema`
: 使用nested object schemas达到exact root-inclusive depth；每层只含一个supported child keyword。

`localPatternSchema`
: 生成string schema和non-backtracking literal/character-class pattern；`targetRegexBytes`按decoded UTF-8 bytes计算。

`schemaWithRef`
: 原样使用recipe ref；remote URI必须在任何network/filesystem access前拒绝。

## Public Generators

`requestWithUnknownPadding`
: 生成otherwise-valid CommandRequest，在client input object增加`futurePadding`并调整document physical bytes。boundary通过transport byte gate后因unknown input field拒绝；+1在JSON decode前以RequestTooLarge拒绝。

`itemDeltaFrame`
: 生成valid Progress EventFrame，仅调整ASCII delta使complete compact document达到target bytes；Item/Turn/Session IDs与route一致。

## Protocol Limit Layers

`protocol-limit-cases.json`递归覆盖Welcome每个leaf。它是named owner-validator scalar matrix，不声称为每个leaf构造完整DTO；`probeContract`固定以minimal valid context直接调用selector指定validator，完整encoded payload只由`boundary-cases.json`和public manifest vector承担。runner按group选择owner validator：

- `transport.*Bytes`：encoded frame/document preflight；
- `transport.maxJsonDepth/maxObjectMembers/maxArrayItems/maxStringBytes`：streaming public JSON structural decoder；
- `text.*`：对应safe text/display/diagnostic constructor；
- `catalog.*`：Command catalog path/argument/entry collection constructor；
- `paging.maxPageSize`：PageRequest validator；
- `paging.maxPageCursorBytes`：cursor input allocation preflight，随后仍执行v1 exact `pc1_`+43 carrier；
- `prompt.*`：PromptIntent/CanonicalUserMessage constructor；
- `workspace.*`：Workspace root/path lexical constructor；
- `queues.*`：lane snapshot/admission bounded collection；
- `interaction.*`：approval/question/answer/view constructor；
- `observation.*`：Item/Interaction/diagnostic view and aggregate Snapshot preflight；
- `embeddedJson.value/schema.*`：对应bounded constructor。

对每个leaf，boundary必须通过该owner validator，boundary+1必须由该owner拒绝。通过owner leaf不表示完整DTO绕过更窄grammar；例如`maxPageCursorBytes=256`只证明allocation ceiling，canonical v1 cursor仍exact 47 bytes，48或256-byte opaque string都由typed cursor decoder拒绝。
