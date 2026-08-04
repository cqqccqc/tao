# M0–M4 补齐进展与剩余 TODO

> 本文档跟进 M0–M4 遗留 TODO 的补齐。完成项移至"已完成",剩余项在此跟进。
> `docs/M2-review.md` 已删(原交接文档,不再用)。设计意图/目标架构见 `docs/design/`。

## 已完成(累计 ~50 项,`cargo ci` 全绿)

### 批 1 — tao-core 工具/会话/hooks/checkpoint/instructions
- `instructions`:`@path` 递归展开 + 向上找 repo 根(C1–C3)
- `compact`:`covers_through_seq` 对齐日志 seq(B1)
- `Task` 工具:`max_depth` 防递归(E1)
- `Edit` 先 Read 跟踪(`ToolCtx.read_files`)(I1)
- 子 agent 权限可配 frontmatter(E3)
- `run_turn` 返回 `usage`(M10)
- `PreToolUse` 在 Approve/ApproveForSession 路径(D6)
- 删 `agent.rs` 死代码(O1)
- hooks:`UserPromptSubmit`/`SubagentStop` 接线 + `Notification` enum(D1/D3)+ config 字段;并行(D4)
- `SessionStart` 接线 + 项目信任(D5);`Notification` emit(D2)
- `config_fingerprint` hash(A5)

### 批 2 — providers + compact
- `ModelClient::context_window(model)` + glm 128k 覆盖(B2)
- `tiktoken-rs` 精确计数(B4)
- stream 重试 `Last-Event-ID`(O2)

### 批 3 — wire 层(tao-server)
- `ToolCallBegin/End` 双发修复(M11)
- `TurnComplete.usage` 回填(M10)
- `Compact`/`SetModel`/`AddPermissionRule`(Session scope)/`ListMcpTools`/`ListModels`(M2/M6/M5/M8/M9)
- `GetHistory` 简化(内存 messages→events)(M1)
- `CheckpointRollback` + `truncate_to_seq`(J1)
- `ResumeSession`/`ResumeEvents`(M3/M7,简化)

### 批 4 — TUI
- shadow 快照(L2)、Esc 中断(L3)、`--resume`/`--fork`(L1)
- `syntect` 代码高亮(L4a)、viewport 滚动(L4b)、行缓存(L4d)
- slash:`/init` `/hooks` `/mcp` `/agent` `/model` + `/cost` `/rewind` `/rollback` `/diff`(✅ 全落地)

### 批 5 — grep + recorder
- `ignore` crate(.gitignore)(H1)
- `fs2` 并发锁(A2)、`redb` 索引(A3)、周期 fsync(A1)、rotate(A4)

### 批 6 — MCP client
- HTTP transport(F1)
- 惰性启动(F4)、重连(F3)、预算折叠(F2)

## 剩余 TODO

### Patch L2 tree-sitter AST 锚定(G1)
- **现状**:`tao-apply-patch` 仅 L1 文本 fuzz(归一化空白滑窗)。
- **TODO**:`@@ <symbol>` 声明时,用 tree-sitter 按语言 parse,定位 AST node 范围,再在范围内 L1 匹配。
- **依赖**:`tree-sitter` + 语法包(rust/typescript/python 等,重,C 绑定)。
- **决策**:暂不做(重依赖+大工程)。价值中(L1 够用)。若做,需选语法包 + parse + node 定位 + fallback L1。

### Stub(简化版,留完善)
1. **`/cost` 真实 usage**:✅ 已落地。`TurnEvent::Usage(TokenUsage)` 在 `ModelMessageEnd` 后 emit;exec `run_json` 输出 `{"type":"usage",...}`;server `turn_event_to_msg` → `EventMsg::TokenCount{used,window=0}`(window 简化为 0,TODO 取 context_window);TUI `UiState.usage` 累计 + `/cost` 展示真实用量。
2. **`/rewind` `/rollback` `/diff`**:✅ 全部已落地。`/rollback` 回滚到最近 checkpoint;`/rewind` 列出 checkpoint 栈(ShadowRepo.checkpoint_history,最多 20 个);`/diff` 显示最近 checkpoint diff stat(ShadowRepo.diff_last = `git show HEAD --stat`)。`/rewind` 仅显示列表,回退仍用 `/rollback`(多级回退 `/rewind N` TODO)。
3. **ToolSearch 元工具**:✅ 已落地。超 `mcp_tool_budget` 折叠+注册 `mcp__toolsearch`(ToolSearchTool,call 时遍历 servers spawn+list+模糊匹配);`mcp_lazy` 模式下始终注册。
4. **MCP 惰性彻底免 list-spawn**:✅ 已落地(dispatcher 模式)。`config.mcp_lazy=true` 时 `load_mcp_tools` 不 spawn 任何 server,只注册 `mcp__toolsearch` + `mcp__call`(`McpCallTool`,call 时 spawn+call_tool+drop);`lazy=false` 时半惰性不变(list-spawn 后 drop + call 重 spawn)。
5. **viewport auto-follow-to-bottom**:✅ 已落地。`push_history`/`push_live` 重置 scroll_offset=0(跟底);反转语义 0=底部/PgUp 向旧。
6. **`truncate_to_seq` 跨段**:✅ 已落地。扫描所有 rotate 段找 seq 所在段,截该段 + 删后续段 + 重建 redb 索引 + 重置计数器。新增 2 测试(跨段截断 + 单段 no-op)。
7. **ResumeSession 运行时 recorder 切换**:✅ 已落地。`Shared.recorder` → `Mutex<Arc<JsonlRecorder>>` + `session_id` → `Mutex<SessionId>`(fork-resume 改 id);ResumeSession 分支 lock + 替换 recorder + session_id + emit SessionConfigured。
8. **GetHistory 从 replay 含 seq**:✅ 已落地。从 `recorder.path()` 重放 LogLine→Event(含 seq),AssistantMessage→`AgentMessage`,其它跳过;保留 `after_seq`/`limit` 过滤。仅读当前段(跨段 TODO)。
9. **ResumeEvents after_seq**:✅ 已落地。复用 GetHistory replay 逻辑,按 `after_seq` 过滤(seq > after_seq),`done=true`。
10. **Notification emit 时机**:✅ 已落地。抽 `run_notification` helper,在 max_steps / 中断(step start / stream) / abort / end turn 各 return 前跑(message=最后 assistant text 或空)。
11. **SessionEnd/Stop hook 接线**:✅ 已落地。`SessionStart` 已接(D5);`run_session_end_stop` 在 end-turn return 前跑 `hooks.session_end` + `hooks.stop`(若非空)。中断/abort 不跑(非正常结束)。

## 验证
```bash
cargo ci                              # fmt + clippy -D warnings + test --workspace
cargo test -p tao-core                 # 71 passed
cargo run -p tao-cli -- proto          # wire 端到端(stdio JSONL)
cargo run -p tao-cli -- mcp-serve      # MCP server
cargo run -p tao-cli -- serve --port 17777   # TCP 多客户端 broadcast
```
