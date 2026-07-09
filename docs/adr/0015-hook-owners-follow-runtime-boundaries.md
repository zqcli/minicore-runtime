# Hook owners follow runtime boundaries

Status: accepted

MiniCore 不把 `RuntimeHooks` 放进当前 MVP 阶段；hook system 是后期扩展点能力。为了关闭模型调用 hook 双 owner 风险，hook owner 固定为拥有对应安全点业务不变量的模块：`SessionRuntime` 拥有 run/prompt/context/queue/compaction/persistence 安全点，`Tools` 拥有工具治理安全点，`ModelGateway` 拥有 model/provider 边界安全点，`CommandManager` / session `Command` 拥有 command catalog/resolve/output 安全点；`Driver` 不调用 hook，`RuntimeHookRegistry` 只保存 handler 和策略，不拥有业务流程。这个决定牺牲了早期扩展灵活性，但避免 hook 在 provider/auth、tool policy、session persistence 和 UI event 边界上形成绕过通道。
