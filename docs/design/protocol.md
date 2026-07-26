# 协议:Op / Event

`tao-protocol` crate 定义 core 与所有前端之间唯一的消息契约。纯 serde 类型,不依赖 tao-core。

## 1. 信封与方向

```rust
/// 前端 → core
pub struct Submission { pub id: String, pub op: Op }
/// core → 前端
pub struct Event { pub id: String, pub msg: EventMsg }
```

- `id`:client 生成的请求 id(turn 用 `turn_id`);Event 的 `id` 回显触发它的 Submission id,审批等子请求用 `call_id`。
- 所有 Event 另带 `seq: u64`(会话内单调递增)与 `turn: Option<TurnId>`,供前端排序、重连补拉(`Op::ResumeEvents { after_seq }`,serve 模式用)。
- 协议版本:`EventMsg::SessionConfigured { protocol_version: u32, .. }` 握手声明;wire 模式下首行必须是 `Op::Hello { protocol_version }`,不匹配则 core 关闭连接并给出可读错误。

## 2. Op(前端 → core)

```rust
pub enum Op {
    Hello { protocol_version: u32 },

    // 会话主流程
    UserTurn { turn_id: String, input: Vec<UserInput> },   // 发起一个 turn
    Interrupt,                                             // 中断当前 turn(等价 Esc)
    Compact,                                               // 主动压缩上下文
    Shutdown,

    // 审批应答(对应 EventMsg::ApprovalRequest)
    ApprovalResponse { call_id: String, decision: ReviewDecision },

    // 会话管理
    ListSessions,
    ResumeSession { session_id: SessionId, fork: bool },
    CheckpointRollback { checkpoint_id: CheckpointId },    // 回滚文件+对话

    // 查询
    GetHistory { after_seq: u64, limit: u32 },
    ResumeEvents { after_seq: u64 },                       // serve 模式重连补拉
    ListMcpTools,
    ListModels,
}

pub enum UserInput { Text { text: String }, Image { path: PathBuf } }

pub enum ReviewDecision {
    Approve,            // 本次允许
    ApproveForSession,  // 本次 + 同规则加入会话级 allow
    Deny,               // 拒绝该工具调用,模型收到拒绝结果继续
    Abort,              // 拒绝并中断整个 turn
}
```

## 3. EventMsg(core → 前端)

```rust
pub enum EventMsg {
    // 会话配置(协议握手后第一个事件)
    SessionConfigured { protocol_version: u32, session_id: SessionId, model: String,
                        permission_mode: PermissionMode, cwd: PathBuf },

    // turn 边界
    TurnStarted { turn_id: String },
    TurnComplete { turn_id: String, usage: TokenUsage, stop_reason: StopReason },

    // 助手流式输出(展示层)
    AgentMessageDelta { text: String },
    AgentMessage { text: String },               // 本条消息流结束(完整文本)
    ReasoningDelta { text: String },             // 思考流(可折叠展示)
    Reasoning { text: String },

    // 工具执行(内置 exec)
    ExecCommandBegin { call_id: String, command: Vec<String>, cwd: PathBuf },
    ExecCommandOutputDelta { call_id: String, stream: ExecStream, chunk: String },
    ExecCommandEnd { call_id: String, exit_code: i32, duration_ms: u64, truncated: bool },

    // 补丁
    PatchApplyBegin { call_id: String, files: Vec<PathBuf> },
    PatchApplyEnd { call_id: String, success: bool, diff: String },

    // 通用工具边界(读/写/搜索/MCP/子 agent 等一切非 exec/patch 工具)
    ToolCallBegin { call_id: String, tool: String, summary: String },
    ToolCallEnd { call_id: String, ok: bool, summary: String },

    // 审批(见 §4)
    ApprovalRequest { call_id: String, kind: ApprovalKind, detail: ApprovalDetail },

    // 计划 / 状态
    PlanUpdated { items: Vec<PlanItem> },
    TokenCount { used: u64, window: u64 },

    // 后台/系统
    BackgroundEvent { message: String },         // 如 "3 files changed on disk"
    Error { message: String, retryable: bool },
    StreamError { message: String },             // 单次流失败(可能自动重试)
}

pub enum ApprovalKind { Exec, Patch, Tool, McpTool }
pub struct ApprovalDetail {
    pub rule_matched: Option<String>,            // 命中的 ask 规则(便于用户理解)
    pub command: Option<Vec<String>>,            // Exec
    pub files: Option<Vec<PathBuf>>,             // Patch
    pub tool: Option<String>,                    // Tool/McpTool + 参数摘要
    pub pattern_suggestion: Option<String>,      // 建议的 allow 规则,如 "Bash(cargo test *)"
}
```

## 4. 审批往返(协议级时序)

```
前端                          core
  │  Op::UserTurn              │
  │ ─────────────────────────► │
  │   TurnStarted              │
  │ ◄───────────────────────── │
  │   AgentMessageDelta * n    │
  │ ◄───────────────────────── │
  │   ExecCommandBegin(若无需审批) / ApprovalRequest{call_id:"c1"}(若需审批)
  │ ◄───────────────────────── │
  │  Op::ApprovalResponse{c1, ApproveForSession}   (用户选择)
  │ ─────────────────────────► │
  │   ExecCommandBegin/.../End │
  │ ◄───────────────────────── │
  │   AgentMessageDelta * n    │
  │   TurnComplete             │
  │ ◄───────────────────────── │
```

契约约束(写进测试):

1. 每个 `ApprovalRequest` 的 `call_id` 在会话内唯一;`ApprovalResponse` 必须携带同一 id,否则 core 返回 `Error`。
2. 审批挂起期间,core 不再推进该 turn 的模型流,但其他事件(如后台通知)仍可到达。
3. 审批超时(可选配置)按 `Deny` 处理并向模型返回拒绝结果。
4. `Deny` 不等于失败:模型会看到"用户拒绝了该操作",通常会改用其他方式或停下来询问。`Abort` 才中断 turn。

## 5. 双传输

| 传输 | 形态 | 用途 |
|---|---|---|
| in-process | `tokio::mpsc` 直接传 `Submission`/`Event` | tao-tui、tao-exec、core 内测 |
| stdio JSONL | 每行一个 JSON(`{"id":..,"op":{..}}` / `{"id":..,"msg":{..}}`) | `tao proto`,脚本与其他语言前端 |
| socket(后期) | TCP + JSONL,或 WebSocket + JSON;SSE 只读流用于订阅 | `tao serve`,web/gui 前端 |

- wire 模式下 stderr 保留给日志,stdout 只走协议。
- serve 模式支持多客户端 attach 同一会话:事件用 `tokio::sync::broadcast` 扇出;新客户端先 `GetHistory` 再增量订阅。
- Op/Event 类型带 `#[serde(tag = "type", rename_all = "snake_case")]`;新增变体 = 次要版本,删除/改义 = 主版本。wire 稳定从 v1 就是硬要求。

## 6. 错误与流控

- 协议级错误一律以 `EventMsg::Error { retryable }` 表达,不破坏流;fatal 错误后 core 发 `TurnComplete { stop_reason: Error }`。
- 流式 channel 有界(1024),溢出时 core **合并** delta(文本拼接 / 输出行合并),不阻塞模型流;可靠事件(审批/边界)走独立 channel 绝不合并丢弃。
- `Interrupt` 是尽力而为:已经发出的 delta 不回收,前端收到 `TurnComplete { stop_reason: Interrupted }` 才认为结束。
