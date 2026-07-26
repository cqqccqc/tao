# Agent 循环(tao-core 的运行原理)

## 1. 会话与 turn

- **Session**:一次对话的全部状态(历史、配置、权限决策、事件日志)。一个 `Agent` actor 对应一个 Session。
- **Turn**:从 `Op::UserTurn` 到 `EventMsg::TurnComplete` 的一次完整交互。一个 turn 内部可能包含多轮"模型 → 工具 → 模型"。

## 2. 主循环伪代码

```rust
async fn run_turn(sess: &mut Session, cancel: CancellationToken) -> Result<()> {
    emit(TurnStarted);
    loop {
        let req = build_request(sess)?;            // 见 §3
        let mut stream = model.stream(&req).await?; // 重试在 client 内,见 providers.md
        let mut assembled = Assembled::default();

        while let Some(ev) = stream.next().await {
            cancel.check()?;                        // Interrupt 传播
            let ev = ev.map_err(retry_or_fail)?;    // 流中断:可恢复则整流重试
            assembled.apply(&ev);
            forward_to_ui(&ev);                     // delta 事件实时透传
        }

        sess.history.push(assembled.message);       // 规范 ModelMessage
        if assembled.tool_calls.is_empty() {
            emit(TurnComplete { usage, stop_reason: assembled.stop_reason });
            return Ok(());
        }

        for call in assembled.tool_calls {
            if cancel.is_cancelled() { finish(Interrupted); }
            let result = dispatch_tool(sess, call).await?; // 权限/hook/执行,见 tools.md
            sess.history.push(tool_result(result));
        }
        // 循环:带着工具结果再问模型
    }
}
```

要点:

- **无状态模型调用**:每次循环都是"全量历史 → 模型"。state 只在 core;provider 无会话(Responses API 的 server-side state 不用,便于跨 provider 与 fork)。
- **中断**:Esc → `Op::Interrupt` → `CancellationToken`。模型 SSE、exec 子进程、MCP 调用统一注册。中断时已完成的工具结果保留在历史中,`stop_reason: Interrupted`。
- **最大步数**:`max_turn_steps`(默认 100)防失控;触顶以 `stop_reason: MaxSteps` 结束并提示。
- **后台文件监听**:turn 进行中若工作区文件被外部修改,发 `BackgroundEvent`,并在下一次模型请求的 system 附注中列出变化文件(防止模型基于过期内容编辑)。

## 3. build_request:请求组装管线

```
静态前缀(prompt-cache 友好,尽量不变)      动态尾部
┌──────────────────────────────┐  ┌────────────────────────────────┐
│ 1. system 基础提示词          │  │ 5. 会话历史(规范 ModelMessage*) │
│ 2. 环境块: cwd/平台/git 分支   │  │ 6. 附件(图片等)                │
│ 3. TAO.md 合并指令            │  │                                │
│ 4. 工具定义(子集,见下)        │  │                                │
└──────────────────────────────┘  └────────────────────────────────┘
```

- **缓存友好**:Anthropic 的 `cache_control` 断点放在静态前缀末尾与历史尾部;OpenAI 自动前缀缓存同理(保持前缀字节稳定)。工具定义顺序固定,避免 cache miss。
- **工具子集化**:内置工具全量提供;MCP 工具若超过 `mcp_tool_budget`(默认 20 个)则按 config 白名单裁剪,并在 system 中说明可用 `ToolSearch` 元工具发现被折叠的 MCP 工具。
- **token 预算**:发送前用 tiktoken 估算(Anthropic 计为近似),超过 `auto_compact_at`(默认窗口 92%)触发自动压缩。

## 4. 上下文压缩(compaction)

策略按触发顺序:

1. **微压缩(免费)**:单个工具结果超阈值(默认 40k token)时,只保留头尾各 N 行 + 中间省略标记;exec 输出实时截断(默认 30k 字符,头 10k + 尾 20k)。
2. **结构性丢弃**:超过 K 轮前的 exec 输出折叠为 `exit_code + 摘要`;已被后续消息覆盖的 Read 结果折叠为路径+行数。
3. **摘要压缩(一次额外模型调用)**:当预算仍超标,用当前模型(可配置小模型)对"将被丢弃的历史"生成结构化摘要(目标:保留任务目标、已做决策、文件改动清单、未完成事项)。摘要作为一条 user 消息写入历史与日志(`LogEvent::Compaction`),原消息标记为已压缩(不删除——日志是 append-only,压缩是派生视图)。

原则:**rollout 仍是 source of truth**,压缩只改变"发给模型的视图"。resume 后重放的是压缩后视图(与 codex 一致),但完整历史可从日志重建(供 share/审计)。

## 5. 子 agent 对循环的复用

`Task` 工具 = 用受限配置重新 `Agent::spawn`:

```rust
SubAgentConfig {
    model: override.or(parent_model),          // 可降级到小模型
    tools: ToolSubset::from_def(&def),         // markdown 定义里的 tools 字段
    permission_mode: ReadOnly,                 // 子 agent 默认只读探索
    depth: parent.depth + 1,                   // max_depth 默认 3,防递归爆炸
    ..inherit_env()
}
```

- 子 agent 的事件流**不进**父会话日志(独立 session 文件);父会话只看到 `ToolCallBegin/Task → ToolCallEnd(汇总报告)`。
- 父子之间通过事件做"进度透传":子 agent 的 `AgentMessage` 以节流方式转成父会话的 `BackgroundEvent { "explorer: ..." }`,TUI 可折叠展示。

## 6. 与日志/重放的关系

循环每完成一个原子步骤(用户输入入史、助手消息完成、工具调用出结果、审批决策、压缩)就 append 一条 `LogEvent`。**fold(LogEvent*) = 当前内存状态**——这既是 resume/fork 的基础,也是测试的基础(fixture 回放,见 testing.md)。顺序即真相:日志没有"修改"语义,compaction、权限升级都是新事件。
