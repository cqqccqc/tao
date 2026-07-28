# 总体架构

## 1. 工作区布局

一个 Cargo workspace,二进制保持极薄,所有能力在库里:

```
tao/
├── Cargo.toml                 # workspace
├── crates/
│   ├── tao-core/              # 通用 agent harness(本项目的核心)
│   ├── tao-protocol/          # Op/Event + LogEvent 类型(core↔前端共享)
│   ├── tao-tui/               # ratatui 终端界面(默认前端)
│   ├── tao-cli/               # `tao` 二进制:调度 tui / exec / proto / serve / login
│   ├── tao-exec/              # headless 单次执行:`tao exec "..."`(输出可 JSONL)
│   ├── tao-server/            # JSONL-over-stdio(proto)与 socket serve(web/gui 用)
│   ├── tao-apply-patch/       # patch DSL 解析与执行(文本 fuzz + AST 两级)
│   ├── tao-mcp/               # MCP 客户端管理(rmcp 封装)+ tao 自身作为 MCP server
│   ├── tao-acp/               # ACP 适配层:`tao acp`,被 Zed 等编辑器拉起(见 acp.md)
├── docs/design/               # 本文档集
└── xtask/                     # 开发任务脚本(快照更新、fixture 生成)
```

crate 依赖方向(只允许向下依赖):

```
                    ┌─► tao-tui
                    ├─► tao-exec
                    ├─► tao-server
 tao-cli ───────────┼─► tao-mcp ──► tao-core ──► tao-protocol
 (子命令调度,薄壳)  ├─► tao-acp  ──┘      │
                    └─► tao-login 内嵌    ▼
                                    tao-apply-patch
```

- `tao-protocol` 不依赖 `tao-core`(纯 serde 类型),前端可以只依赖 protocol。
- `tao-core` 依赖 `tao-apply-patch`、`tao-mcp` 的实现;`tao-tui` 只通过 protocol + `AgentHandle` 与 core 交互。
- `tao-mcp` / `tao-acp` 都是 core 的**消费者**(客户端适配层):mcp-serve 借 core 的会话能力对外提供工具,acp 把 core 事件翻译成 ACP。
- 未来 `tao-web-ui` 复用 `tao-server` 的传输层;`tao-gui` 可同进程内嵌或走 server。

### CLI 子命令(tao-cli 调度)

| 命令 | 目标 crate |
|---|---|
| `tao` / `tao tui` | tao-tui |
| `tao exec "<prompt>"` | tao-exec |
| `tao proto` | tao-server(stdio 模式) |
| `tao serve --port N` | tao-server(socket 模式,M4) |
| `tao acp` | tao-acp |
| `tao mcp-serve` | tao-mcp(server 模式) |
| `tao login|logout|auth` | tao-cli(内嵌,调 tao-core auth) |
| `tao sessions <ls|audit|gc>` | tao-cli(内嵌,读日志/索引) |

## 2. 进程模型

```
tao tui (默认)                tao exec "fix tests"           tao proto (协议模式)          tao serve --port 7777 (后期)
┌──────────────────┐        ┌──────────────────┐           ┌──────────────────┐         ┌────────────────────┐
│ tao-tui (ratatui)│        │ tao-exec          │           │ tao-server stdio │         │ tao-server         │
│   │ in-process   │        │   │ in-process     │           │   stdin/stdout   │         │  TCP/WS + SSE      │
│   │ mpsc channel │        │   │ mpsc channel   │           │   JSONL lines    │         │  多客户端/多会话    │
│   ▼              │        │   ▼               │           │        ▲         │         └─────────┬──────────┘
│ tao-core Agent   │        │ tao-core Agent    │  ◄────────┼────────┘         │                   │ 同一套 Op/Event
└──────────────────┘        └──────────────────┘           (任意语言的前端/脚本)      ┌───────────▼───────────┐
                                                                                     │  浏览器 / tao-gui     │
                                                                                     └───────────────────────┘
```

- **in-process**:`tokio::mpsc` channel 对,`(Submission, Event)` 直接传结构体。TUI 的最佳体验与可测试性。
- **wire**:同一对类型 serde 序列化为 JSON Lines。stdio(proto 模式,供脚本/其他 CLI 组合);socket(serve 模式,供 web/gui,支持多客户端订阅同一会话)。
- **一个类型,两种传输**——这是从 codex 学到并确认的关键决策:第二前端的边际成本 ≈ 0。

## 3. tao-core 类型概览

```rust
// 入口:克隆即新句柄,共享同一会话 actor
pub struct AgentHandle { /* submit_tx, event_rx(broadcast) */ }
impl AgentHandle {
    pub async fn spawn(config: SessionConfig) -> Result<(Self, SessionId)>;
    pub async fn submit(&self, op: Op) -> Result<()>;        // Op 定义见 protocol.md
    pub async fn next_event(&self) -> Result<Event>;          // Event 定义见 protocol.md
}
```

内部组件(每个都是小模块,避免 god-struct——codex 的教训):

| 模块 | 职责 |
|---|---|
| `session.rs` | 会话生命周期、turn loop 驱动、中断信号 |
| `history.rs` | 规范模型消息史(内存视图,可 fold 重建) |
| `providers/` | `ModelClient` trait + `anthropic.rs` / `openai_responses.rs` / `openai_chat.rs` 三个 wire codec |
| `tools/` | `Tool` trait、`ToolRegistry`;内置 Bash/Read/Write/Edit/Patch/Grep/Glob(M2) |
| `tools/mcp.rs` | MCP server 连接管理、工具名映射 `mcp__server__tool` |
| `exec.rs` | 子进程执行:流式输出、超时、cancel-on-drop、输出截断 |
| `permissions.rs` | 权限引擎(模式+规则+会话决策,纯函数 `decide`)+ `Approver` trait(审批 await,M2-1) |
| `hooks.rs` | hook 触发点、进程执行、退出码语义(M3-2) |
| `agents.rs` | 子 agent 定义加载(`~/.tao/agents/`)+ Task 工具(M3-3) |
| `commands.rs` | slash 命令(内置 + markdown 模板)(M3-1) |
| `skills.rs` | 技能(SKILL.md 渐进披露)(M4-1) |
| `checkpoint.rs` | 影子 git 仓库快照/回滚(M4-3) |
| `recorder.rs` | `LogEvent` append-only 落盘(JSONL)(M2-4) |
| `replay.rs` | 日志 → `SessionState` 的 fold;resume/fork(M2-4) |
| `compact.rs` | 上下文压缩策略(M2-5) |
| `config.rs` | 分层配置加载 + `[permissions]`/`[hooks]`/`[mcp_servers]` |
| `auth.rs` | 凭证存储(keyring + auth.json)、OAuth PKCE(⬜ 未实现) |
| `instructions.rs` | AGENTS.md 层级发现与合并(M2-3) |

核心 trait(抽象清单刻意保持最小):

```rust
trait ModelClient: Send + Sync {
    async fn stream(&self, req: &ModelRequest)
        -> Result<BoxStream<'static, Result<ModelStreamEvent>>>;
}

trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;                     // 名字/描述/schema(wire 无关)
    async fn call(&self, args: Value, ctx: &ToolCtx)
        -> Result<ToolOutput, ToolError>;           // ToolError 见 tools.md
}

trait Sandbox: Send + Sync {                        // M5 才实现;v1 为 NullSandbox
    fn wrap(&self, cmd: CommandSpec, policy: &FsPolicy) -> Result<CommandSpec>;
}
```

**领域错误类型**(不用 anyhow 做公共 API):
`ModelError`(Retryable/Fatal/Auth/ContextLength)、`ToolError`(Deny/Reject/Failed)、`PatchError`、`ConfigError`、`HookError`、`ProtocolError`。UI 按类别渲染(可重试网络错误 vs 权限拒绝 vs 模型 fatal)。

## 4. core ↔ UI 的协作原理

### 4.1 一切都是 Op/Event 流

UI 与 core 之间**没有方法调用**,只有异步消息:

- UI 侧:`submit(Op::UserTurn{...})` → 之后持续 `next_event()` 渲染增量流(`AgentMessageDelta`、`ExecCommandBegin/OutputDelta/End`、`ApprovalRequest`……)。
- 审批是唯一需要"应答"的事件:`EventMsg::ApprovalRequest { call_id, ... }` 由 UI 展示,用户选择后 `submit(Op::ApprovalResponse { call_id, decision })`。core 内部 await 这个 call_id 的 decision。
- UI 不持有任何会话可变状态;它渲染的只是一个事件流的投影 → TUI 可以崩溃重启后重放日志恢复界面;web 前端可以中途 attach。

### 4.2 请求关联

每个 `Op::UserTurn` 携带 client 生成的 `turn_id`;core 产生的所有事件带回 `turn_id` 与单调 `seq`。审批/工具调用用 `call_id` 关联。**echo ID 必须严格**——codex 在 headless 模式下因 request-id 不匹配死锁过,这是协议层要写进契约测试的。

### 4.3 事件有界与背压

- 流式输出(模型 delta、exec output)走专门的有界 channel(如 1024 条),满了则 core 合并 delta(对模型文本直接拼接;对 exec 输出按行合并)而不是阻塞模型流。
- 非流式事件(审批请求、TurnComplete)走可靠 channel,绝不丢弃。

### 4.4 取消语义

`Op::Interrupt` 通过 `tokio_util::sync::CancellationToken` 传播:模型 SSE 流、exec 子进程(SIGTERM→SIGKILL)、MCP 调用统一注册取消。drop 即取消,杜绝孤儿进程。

## 5. 关键架构取舍(为什么这样做)

| 取舍 | 选择 | 理由 |
|---|---|---|
| 核心形态 | 库 + 同进程 TUI + stdio/socket 协议 | 单二进制体验最好(codex 模式);协议前置让 web/gui 不用改 core |
| 会话存储 | append-only JSONL,不用 SQLite | 崩溃安全、可 grep、fork 廉价;搜索列表用派生索引(见 sessions.md) |
| 模型抽象 | 规范 `Model` 格式 + 3 个 codec | 三家 provider 的请求/响应结构互不相似,强行统一成"最小公分母"会丢掉 reasoning/cache/thinking 等关键能力 |
| 补丁格式 | 双模式:精确 Edit 工具 + apply-patch DSL(语法/语义分离) | 见 tools.md §3;比 str-replace 可靠,比 unified diff 对模型友好 |
| 权限 | 模式 + 规则 + 审批全在 core;plan 模式 = 权限 profile | codex 的矩阵是最干净的核心抽象;Claude Code 的规则列表补齐灵活性 |
| 扩展 | hooks + markdown 子 agent + MCP,无二进制插件 | 覆盖 Claude Code 生态 90% 价值,实现成本是文件系统 + 进程 spawn |
| TUI | ratatui inline viewport(非 alternate screen) | 聊天场景天然滚入终端 scrollback;codex 已验证 |
