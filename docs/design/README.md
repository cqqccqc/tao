# tao 设计文档

> tao 是一个用 Rust 构建的 coding agent,对标 Claude Code / OpenAI codex CLI / gemini-cli / opencode。
> 内核是通用 agent harness(`tao-core`),上层是前端界面:`tao-tui`(优先)、`tao-web-ui` / `tao-gui`(后期)。

## 愿景与定位

- **通用内核,多前端**:`tao-core` 不假设任何 UI 形态,所有核心行为(会话、模型、工具、权限、持久化、扩展)都通过同一份 `Op`/`Event` 协议暴露。TUI 是 in-process 消费者;web/gui 是 wire 消费者;ACP 适配层(见 [acp.md](acp.md))让 Zed 等编辑器内嵌 tao。协议从第一天起就是 wire-stable 的。
- **经典体验做到位**:会话、工具、审批、plan 模式、MCP、自定义指令——与 Claude Code / codex 同等水准是入场券。
- **可编程平台**:hooks、markdown 子 agent、slash 命令、MCP、技能(skills)优先于任何二进制插件系统。
- **会话即资产**:append-only 事件日志是 source of truth,resume / fork / share / checkpoint 全部建立在它之上。

## 设计原则

1. **单一协议,两种传输**:`Op`/`Event` 枚举同时用于 in-process channel(TUI)和 JSONL over stdio(headless / proto / 未来 web)。不为任何前端写"第二套 API"。
2. **事件溯源**:会话落盘为 append-only JSONL 事件日志;内存状态可完全由日志重放重建。fork = 换 session_id 重放前缀。
3. **模型无关的规范模型**:core 内部定义 `Model` 规范格式;每种线协议(Anthropic Messages / OpenAI Responses / OpenAI Chat)各自做双向 codec。agent loop 只看见规范格式。
4. **权限在 core,UI 只渲染**:审批是协议里的一等公民(request event → decision op)。UI 无权决定一个操作是否安全。
5. **能抄不造**:尽可能复用成熟 crate;只对"承重"抽象(model client、patch 引擎)自研。详见附录 crates.md。
6. **先小后大**:每个里程碑都可交付、可试用;早期避免过度抽象(第三个协议适配器出现前不写第 0 个抽象)。

## 决策摘要(与用户对齐)

| 决策点 | 结论 |
|---|---|
| 模型协议 | 第一优先级:**Anthropic Messages API / OpenAI Responses API / OpenAI Chat Completions API** 三套协议 + base_url 自定义(覆盖 OpenAI 兼容生态) |
| 进程架构 | **核心库 + 前端同进程**(tao-tui 直接链接 tao-core),同时保留 headless/stdio JSONL 协议,未来 web/gui 走 socket 传输 |
| 安全模型 | 第一版:**权限模式 + 审批 + 规则引擎**;OS 沙箱(seatbelt/landlock)后置为 `tao-sandbox` |
| 差异化 | ① 经典体验先行 ② 可编程/可扩展性(hooks、子 agent、MCP)③ 会话管理/协作(fork/share/checkpoint)④ TUI 体验 |

## 文档索引

| 文档 | 内容 |
|---|---|
| [architecture.md](architecture.md) | 工作区布局、crate 职责、进程模型、core 类型概览、core–UI 原理 |
| [protocol.md](protocol.md) | Op/Event 协议全量定义、审批往返、协议版本化、双传输 |
| [agent-loop.md](agent-loop.md) | 会话/turn 循环、工具调度、中断、上下文压缩——核心原理 |
| [providers.md](providers.md) | 规范模型格式、三个线协议 codec、SSE 累积、重试/缓存/usage |
| [tools.md](tools.md) | Tool trait、内置工具、Edit/patch 引擎、MCP 客户端 |
| [permissions.md](permissions.md) | 权限模式、规则引擎、审批流程、逃逸分析、未来 OS 沙箱 |
| [sessions.md](sessions.md) | 事件日志格式、resume/fork、checkpoint(影子 git)、compaction、share |
| [config.md](config.md) | 分层配置、config.toml 参考、模型提供方、认证(OAuth/API key)、TAO.md 指令文件 |
| [extensibility.md](extensibility.md) | hooks、子 agent、slash 命令、MCP、技能 |
| [acp.md](acp.md) | ACP(Agent Client Protocol)适配层:被 Zed 等编辑器内嵌集成 |
| [tui.md](tui.md) | tao-tui 架构、inline viewport、渲染管线、组件、键位、主题 |
| [testing.md](testing.md) | 测试金字塔、MockModel、fixture 回放、TUI 快照测试、fuzz |
| [roadmap.md](roadmap.md) | M0–M6 里程碑、依赖关系、风险、非目标 |
| [crates.md](crates.md) | 关键第三方 crate 选型及理由(附录) |

## 快速术语表

| 术语 | 含义 |
|---|---|
| `Op` | 前端 → core 的操作(protocol 方向) |
| `Event` | core → 前端的事件(protocol 方向) |
| `Agent` / `AgentHandle` | 单会话 actor 与它的句柄(`submit(Op)` / `next_event()`) |
| `Model`(规范格式) | core 内部的请求/响应表示,与任何线协议无关 |
| `wire API` | 某个 provider 的 HTTP 协议:`anthropic` / `openai-responses` / `openai-chat` |
| `LogEvent` | 会话事件日志里的一行(JSONL) |
| 权限模式 | `default` / `plan` / `accept-edits` / `bypass`(plan 模式即一个权限 profile) |
