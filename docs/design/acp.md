# ACP 集成(Agent Client Protocol)

## 1. 背景与定位

ACP 是 Zed 发起的开放协议:编辑器作为 client,通过 **JSON-RPC 2.0 over stdio(nd-json)** 拉起并驱动 agent 子进程。gemini-cli 原生支持,claude-code / codex 通过适配器接入——它正在成为"编辑器内嵌 agent"事实上的标准接口之一。

对 tao 的意义:

- **第三前端**:TUI(自有)、web/gui(自有,走 serve)之外,ACP 让 Zed 及未来任何 ACP 编辑器把 tao 当内嵌 agent——这是分发渠道,不只是功能。
- **高度同构**:ACP 的 prompt / session/update / request_permission / cancel / plan / modes,几乎就是 tao `Op`/`Event` 的外文版。协议先行的设计在这里兑现:适配层只是翻译。
- **core 协议中立**:ACP(以及未来任何宿主协议)**不进 core**,永远活在独立的适配 crate 里。

> 注意:ACP 规范年轻且演进快(方法名/字段以 agentclientprotocol.com 与 zed-industries/agent-client-protocol 仓库为准),实施前先核对当版 spec;适配层需锁定官方 Rust SDK(`agent_client_protocol` crate)版本并跟进变更。

## 2. 形态

- 新 crate **`tao-acp`**(依赖 `agent_client_protocol` + `tao-core` + `tao-protocol`),暴露子命令 **`tao acp`**。
- 编辑器配置示例(Zed settings.json):

```json
{
  "agent_servers": {
    "tao": { "command": "tao", "args": ["acp"], "env": {} }
  }
}
```

- 进程模型:编辑器拉起一个 `tao acp` 进程 = 一条 ACP 连接;连接内 `session/new` 可多次 → 多个 `AgentHandle`(与 serve 的多会话复用同一机制)。

## 3. 方法映射

| ACP | tao |
|---|---|
| `initialize`(能力协商) | 协议握手;声明 fs/terminal 能力的使用意愿 |
| `authenticate` / authMethods | auth 状态检查;未认证时要求客户端触发(等价 `tao login`,见 config.md §3) |
| `session/new { cwd, mcpServers }` | `Agent::spawn(SessionConfig)`;客户端注入的 mcpServers 并入会话配置 |
| `session/load` | resume:日志重放(tao 事件溯源的原生强项,见 sessions.md §2) |
| `session/prompt`(text/image/resource blocks) | `Op::UserTurn` |
| `session/update`:`agent_message_chunk` | `EventMsg::AgentMessageDelta` |
| `session/update`:`agent_thought_chunk` | `EventMsg::ReasoningDelta` |
| `session/update`:`tool_call` / `tool_call_update` | `ToolCallBegin/End`、`ExecCommandBegin/OutputDelta/End`、`PatchApplyBegin/End`(kind/locations 尽力映射) |
| `session/update`:`plan` | `EventMsg::PlanUpdated` |
| `session/update`:`available_commands_update` | slash 命令清单(内置 + markdown 自定义) |
| `session/request_permission`(allow_once/allow_always/reject_once/reject_always) | `ApprovalRequest` ↔ `ApprovalResponse`;allow_always → `ApproveForSession` |
| `session/cancel` | `Op::Interrupt` |
| `session/set_mode` / modes 列表 | 权限模式切换(default/plan/accept-edits;**bypass 不对 ACP 暴露**) |
| prompt 完成 stop_reason | `TurnComplete.stop_reason` 映射(end_turn/max_tokens/cancelled/refused) |
| `fs/read_text_file` / `fs/write_text_file`(client 实现) | 可选路由,见 §4.1 |
| `terminal/*`(client 实现) | 可选路由,见 §4.1 |

## 4. 语义差异与取舍

1. **文件与终端的执行位置**:ACP 允许 agent 通过 client 读写文件、在编辑器终端跑命令(为了编辑器内联 diff 与终端可视)。
   - v1:tao 工具**直读写磁盘、自跑进程**(与编辑器共享 cwd,结果一致,实现最简;shadow-git checkpoint 照常工作)。
   - v2(可选增强):检测 client 能力后,把 `Edit`/`Patch` 的写盘路由为 `fs/write_text_file`(编辑器内呈现 diff),把 `Bash` 路由为 `terminal/*`。路由是适配层决策,core 无感。
2. **权限粒度**:ACP 只有四种选项。`allow_always` 映射为**会话级**授权;tao 独有的"固化为项目规则(pattern)"在 ACP 客户端无法表达——审批文案中提示"可在 tao TUI 中固化为规则"。
3. **模式映射**:`default/plan/accept-edits` 映射为 ACP modes;`bypass` 出于安全默认不暴露给编辑器侧。
4. **多会话与取消**:`session/new` 多次 → 多 AgentHandle;`session/cancel` 精确对应当前 turn 的 CancellationToken。
5. **版本跟进**:ACP spec 演进快,契约测试(§6)是版本升级时的安全网。

## 5. 与 serve 的关系

| | ACP(`tao acp`) | serve(`tao serve`) |
|---|---|---|
| 场景 | 被编辑器**集成**(编辑器拉起,stdio) | 自有前端(web/gui,TCP/WS,常驻) |
| 协议 | ACP JSON-RPC(翻译) | tao 原生 Op/Event JSONL(直通) |
| 依赖 | 无(独立于 serve) | 无 |

两者复用同一 `AgentHandle` 多实例机制,互不依赖,可分别交付。

## 6. 测试

- **契约测试**:脚本化 mock client 录放 JSON-RPC,覆盖 initialize / session/new / prompt / permission 四选项 / cancel / session/load / set_mode。
- **映射快照**:`session/update` 通知序列 insta 快照(从 fixture 会话日志回放生成)。
- **手测清单**(Zed 实接):新建线程、流式输出、中断、审批(含 allow_always)、plan 模式切换、slash 命令出现与执行、重启后 load 线程、图片/资源 prompt。

## 7. 里程碑

排入 **M4**(与 serve 同期,二者共同验证"core 无 UI 假设");若 Zed 集成优先级高可前移至 M3 末——依赖项(resume、权限事件、plan、slash 命令、modes)在 M3 结束时就齐了。工作量估计 1–2 周。
