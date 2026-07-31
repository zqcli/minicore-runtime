# Prompt Content Is Materialized Before Publication

状态：Accepted
日期：2026-07-31

MiniCore在Prompt candidate build期间读取、解析并捕获完整正文，`PromptDefinition`持有materialized immutable `PromptContent`。实现通过进程内强`Arc`共享正文；path、URL、source ID和internal cache key只用于discovery、provenance或优化，不形成可重新解析或durable的`PromptContentRef`。该决策关闭Prompt Q1。

## 背景

Q1需要在两类表示之间选择：`PromptDefinition`直接拥有已经捕获的正文值，或只保存path、URI、hash/content ID并在`for_turn()`或`assemble()`时解析。后者可以延迟加载大正文，但会把filesystem/network I/O、cache miss、authorization漂移和source变化引入Turn执行，破坏PromptSet同步纯内存assembly与explicit reload语义。

Pi、Codex、Gemini CLI、OpenCode、OpenHands和Claude Code在provider调用前都会把Prompt materialize为字符串或消息对象。Codex和OpenHands在需要resume解释性的场景直接保存rendered正文；其他CLI通常在session初始化、reload或每次request前重新组装正文。没有被调研项目使用content ID/hash作为Model调用时必须解析的正文identity。MiniCore方案A不恢复旧PromptSet，也没有historical Prompt审计需求。

## 决策

1. `PromptContent`是已经materialize的immutable text value，clone只复制强引用：

   ```rust
   #[derive(Clone)]
   pub struct PromptContent {
       text: Arc<str>,
   }
   ```

   字段与constructor保持private，公开只读`text() -> &str`。
2. shared Prompt source在Runtime initialize或shared `/reload` candidate build期间完成读取、template/source解析、规范化和`PromptContent`构造；Workspace-bound Prompt source在Session load、Idle Workspace update或`/reload workspace` candidate期间完成同一步骤。
3. `PromptResourceView`、`PromptDefinition`和active `PromptSet`持有强引用。active Turn继续使用old content；reload publication只影响future Turn。
4. `PromptSourceAdapter`可以接受path、URL、built-in preset或其他source locator作为adapter配置，但返回值必须包含materialized content。`PromptService::for_turn()`与`PromptSet::assemble()`不得读取source、执行network/filesystem I/O或通过content key查找正文。
5. `PromptContentCache`是PromptService private实现优化。cache eviction只删除future reuse机会，不使任何已发布view、PromptDefinition或PromptSet失效。correctness不能依赖cache hit、PromptId、path、hash或额外version。
6. 相同正文允许在PromptService内部共享同一个`Arc<str>`；PromptDefinition identity、role和provenance仍独立，不能因正文相同而合并授权或来源语义。
7. MVP不定义`PromptContentRef`、Prompt content hash、content version或durable content-addressed store。Session JSONL不保存静态Prompt baseline、PromptContent或其resolver identity。
8. provider prompt caching只消费最终materialized sections和provider cache-control，不拥有Prompt正文identity或MiniCore recovery语义。
9. Q3未来若引入Prompt template helper，template source和rendered output都必须是materialized values；render过程不能在active assembly中重新读取source。

## 后果

- PromptSet assembly保持同步、确定、无I/O；source unavailable只在candidate build阶段暴露。
- 多Session通过强`Arc`共享大Prompt正文，避免把同一字符串复制进每个PromptSet。
- source文件或远程内容变化只有在对应显式reload/candidate publication后影响future Turn。
- cache清空、eviction或implementation key变化不影响active Turn correctness。
- restart按current source重建PromptResourceView，不恢复旧PromptSet或旧Prompt正文。
- future若出现historical Prompt审计、exact Prompt replay或跨机器execution migration需求，应设计独立audit/execution记录并保存rendered value；不能把可重新解析locator加入当前conversation JSONL。

## 修订关系

本ADR细化ADR 0110的shared immutable Prompt view与explicit reload、ADR 0123的immutable Arc/no fingerprint规则，并与ADR 0127的conversation-only Session recording保持一致。Prompt Q4随后由[ADR 0129](0129-user-message-contributions-use-part-level-safe-provenance.md)关闭。
