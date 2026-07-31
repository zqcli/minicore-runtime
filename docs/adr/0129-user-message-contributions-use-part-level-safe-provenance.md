# ADR 0129：用户消息贡献使用Part-Level安全Provenance

状态：Accepted
日期：2026-07-31

> [ADR 0130](0130-user-message-composition-resolves-skills-asynchronously.md)进一步冻结本ADR的执行seam：TurnExecutionContext异步加载captured Skill并构造composition input，PromptSet同步原子规范化，Session Execution处理Starting/Steer cancellation与await后重验。

> 2026-07-31：V4-P1-1 public payload freeze删除未定义的`PromptBodyIntent::Template`。MVP body只支持Empty/Text；本ADR的`body + ordered skills[]`、part boundary与safe provenance决策保持。future template必须另行定义TemplateId、arguments、materialized render、limits、reload和capability。

## 背景

Prompt Q4尚未冻结`SkillIntent`、`UserMessageCompositionInput`和recorded contribution stamp的精确字段。旧草案把Text、Template、Skill和Composite放在同一个递归`PromptIntent` enum中，并用字符offset把Skill/Workspace来源关联到最终正文。

该形状存在三个问题：

- “Skill + 用户任务”和多个Skill必须依赖未定义的递归Composite语义；
- 字符offset没有说明byte、Unicode scalar或grapheme口径，也无法稳定覆盖Image等结构化content part；
- `CanonicalUserMessage`和`StoredUserMessage`分别保存不同stamp类型会形成重复事实源，并可能把绝对路径或authorization写入JSONL。

Pi、Codex、Gemini CLI、OpenCode、OpenHands和Claude Code都在模型调用前materialize最终用户内容。可观察实现通常把来源关联到整条消息或独立part；没有可靠实现依赖Skill正文字符offset完成恢复。MiniCore需要在此基础上保留Turn-pinned Skill/Workspace授权边界和tolerant replay。

## 决策

### Intent形状

用户正文与Skill选择正交表达：

```rust
pub struct PromptIntent {
    pub body: PromptBodyIntent,
    pub skills: Arc<[SkillIntent]>,
}

pub enum PromptBodyIntent {
    Empty,
    Text(TextIntent),
}

pub struct TextIntent {
    pub text: String,
}

pub struct SkillIntent {
    pub skill_id: SkillId,
}
```

删除`PromptIntent::Skill`、`PromptIntent::Composite`和`CompositePromptIntent`。MVP也不保留未定义Template variant。`TextIntent`保存用户显式non-empty text；`SkillIntent`只保存稳定`SkillId`；name只用于发现/展示，path、source ref和authorization不能进入public intent。多个Skill按`skills`中的声明顺序表达；重复`SkillId`在任何正文I/O或live apply前返回typed invalid-intent error。

Runtime边界的`PromptIntentInput`使用同样的`body + skills[]`逻辑形状，并在进入Session ingress前规范化为`PromptIntent`。slash command或GUI可以先按name/catalog selection解析，但成功输出必须是`SkillId`；queue保存intent，不提前展开Skill正文。exact serde tag/casing仍由通用wire freeze决定。

### Composition输入与授权

```rust
pub(crate) struct UserMessageCompositionInput {
    intent: PromptIntent,
    contributions: Arc<[PromptContribution]>,
}
```

`UserMessageCompositionInput`字段和constructor保持crate-private，只能由`TurnExecutionContext`在本Turn捕获对象上完成解析后构造：

1. 对每个`SkillIntent.skill_id`从本Turn captured `SkillView`取得exact entry；
2. `SkillService`只解析该entry持有的captured bytes；
3. `SkillInjector`产生包含exact `SkillSourceRef`的`PromptContribution`；
4. Workspace producer使用`WorkspaceSourceRef`携带exact `WorkspaceSourceAuthorization`；
5. `TurnExecutionContext`确认全部required Workspace contributions已经成功产生后才构造composition input；
6. `PromptSet`把SkillIntent与Skill contributions一一匹配，验证全部supplied Workspace contributions，再构造canonical message。

缺失Skill、stale selection、重复Skill、额外Skill contribution、required Workspace contribution失败或source mismatch都在live apply前失败。不能创建部分用户消息，也不能静默跳过required contribution。active Turn只从captured view解析；reload只影响future Turn。

### Canonical排序与Part边界

最终`CanonicalUserMessage`使用稳定顺序：

1. 非空用户body产生的顶层content parts；
2. Skill contributions按`PromptIntent.skills`声明顺序；
3. Workspace contributions按`(WorkspaceRootKey, WorkspaceRelativePath)`稳定排序。

每个contribution必须形成一个独立顶层`MessageContent` part。PromptSet不把多个来源拼成一个字符串part，也不使用字符offset。`body = Empty`只有在至少存在一个合法contribution时有效。

### Safe Provenance Stamp

```rust
pub struct PromptContributionStamp {
    content_part_index: u32,
    origin: PromptContributionOrigin,
}

pub enum PromptContributionOrigin {
    Skill {
        skill_id: SkillId,
    },
    Workspace {
        root_key: WorkspaceRootKey,
        relative_location: WorkspaceRelativePath,
    },
}
```

`content_part_index`是`CanonicalUserMessage.message().content`顶层part数组的零基索引，与UTF-8 byte、Unicode scalar、grapheme或rendered text offset无关。首版每个贡献part恰有一个stamp；body part没有contribution stamp。索引超出`u32`或不能指向对应贡献part时composition失败，不截断或猜测。

exact `SkillSourceRef`、`WorkspaceSourceAuthorization`、canonical root、trust、绝对路径和captured bytes只用于composition前的process-local验证。验证成功后，PromptSet只投影上述safe origin。stamp不包含name、absolute path、canonical root、trust revision、authorization、hash、cache key或正文引用。

### Live与Recording单一表示

`CanonicalUserMessage`继续拥有`MessageRecord + Arc<[PromptContributionStamp]>`。live reducer、JSONL和后续Prompt assembly使用同一个`PromptContributionStamp`类型；不定义`StoredPromptContributionStamp`，`StoredUserMessage`也不保存第二份stamp字段。

conversation正文是恢复正确性的事实，stamp只用于安全解释。JSONL decoder必须允许独立降级stamp：未知origin、malformed stamp或越界`content_part_index`直接丢弃；同一part出现多个stamp时按recorded顺序first valid wins，丢弃后续重复并产生bounded diagnostic。合法UserMessage正文始终保留。Replay不根据stamp重新加载Skill/Workspace source，不重新授权，也不重建旧PromptSet。

### Skill调用的领域边界

用户通过slash、GUI或typed command显式选择Skill时，这是用户消息规范化的一部分，不创建独立Item。模型主动调用Skill Tool时继续创建ToolInvocation Item，并通过ToolCall/ToolResult complete exchange进入conversation。未来自动Skill选择若被引入，也必须在加载正文前先产出typed `SkillIntent`，不能建立current-call injection旁路。

## 后果

- Skill请求与用户任务可以直接组合，也能稳定表达多个Skill；
- exact source authorization不会进入durable schema，restart也不会把解释性stamp当成授权；
- part-level关联适用于文本和结构化消息，不受Unicode offset影响；
- contribution正文一旦进入conversation就按实际materialized value恢复，source变化不会回写历史；
- tolerant replay可以在provenance损坏时保留conversation正文；
- 历史UI只能安全显示SkillId或Workspace root-relative location；需要historical name、完整Prompt审计或exact source快照时应设计独立audit记录。

## 拒绝的方案

- `PromptIntent::{Skill, Composite}`：把正文与贡献混成递归树，排序、重复和原子失败语义不清晰；
- character range stamp：坐标口径不稳定，不能覆盖结构化part；
- 在intent中保存path/source authorization：把用户选择与读取授权耦合，并产生reload/TOCTOU风险；
- JSONL保存exact authorization或绝对路径：泄露敏感环境信息，并错误暗示restart可以复用旧授权；
- 只保存provenance并在replay时重新加载正文：source可能变化或消失，无法恢复当时conversation事实。

## 修订关系

本ADR关闭Prompt Q4，细化ADR 0110的Skill按需加载与PromptSet规范化接口、ADR 0128的materialized PromptContent规则，并与ADR 0127的conversation-only best-effort recording保持一致。它不冻结通用serde casing、public ID文本格式、`Timestamp`、`Money`或`StoredCompaction` wire。