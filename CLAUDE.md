# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What tao is

tao 是一个用 Rust 写的 coding agent。核心不是一个特定的终端 UI,而是一个 **UI 无关的 agent harness**(`tao-core`):会话、模型、工具、权限、持久化、扩展全部通过单一 `Op`/`Event` 协议暴露。TUI、headless、stdio 协议、ACP、MCP server 都是这一协议的不同消费者。状态:alpha / dogfooding(M0–M4 完成,M5 加固、M6 web 前端进行中)。

## Commands

工具链:pinned stable(`rust-toolchain.toml`),edition 2024,`rust-version = 1.88`,resolver 3。代码用了 edition 2024 的 let-chains,所以需要 ≥ 1.88。

```bash
cargo build --release                       # 二进制:target/release/tao
cargo run -p tao-cli                        # 默认子命令 = TUI(需 API key 环境变量)
cargo run -p tao-cli -- exec "fix tests" --json   # headless 单次,输出 JSONL 事件流

cargo ci                                    # CI 门禁 = xtask ci = fmt check + clippy(-D warnings) + test
cargo fmt --all --check                     # 仅格式检查;修复用 cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                      # 全量测试
cargo test -p tao-core <name>               # 跑单个测试 / 单个 crate
cargo test -p tao-core --test turn_loop <name>   # 指定集成测试文件
```

`cargo ci` 和 `cargo xt` 是 `.cargo/config.toml` 里的 alias,都转发到 `xtask`。xtask 目前**只有 `ci` 一个任务**(README 提到的"快照更新 / fixture 生成"尚未实现)。CI(`.github/workflows/ci.yml`)在 ubuntu + macos 上跑同一套 fmt/clippy/test,且 `RUSTFLAGS="-D warnings"` —— 任何 warning 都会红。

子命令现状:`tui`/`exec`/`proto`/`serve`/`acp`/`mcp-serve`/`sessions ls|show|share|audit|gc` 可用;`login`/`logout`/`auth` 是 stub(直接返回错误),认证目前走环境变量(`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `DEEPSEEK_API_KEY` / `MEITUAN_AIGC_KEY` 等 `env_key`)。`proto`/`serve` 是 Op/Event 的 wire 传输(stdio JSONL / TCP 多客户端 broadcast,共享 wire 驱动层 `crates/tao-server/src/session.rs`);`mcp-serve` 把 tao 暴露为 MCP server(stdio JSON-RPC,暴露 `list_sessions`/`send_message`/`read_session` 三工具,见 `crates/tao-mcp/src/server.rs`)。

## Architecture

Cargo workspace,二进制刻意保持极薄,所有能力在库里。**依赖只允许向下**:

```
tao-cli (薄壳,clap 解析后分发)
  ├─► tao-tui / tao-exec / tao-server / tao-acp / tao-mcp   (前端 = core 的消费者)
  └─► tao-core ──► tao-protocol   (+ tao-apply-patch)
```

- `tao-protocol`:纯 serde 类型(`Op`/`Event`/`LogEvent`/权限类型),不依赖任何 workspace crate。前端可以只依赖 protocol。
- `tao-core`:harness 本体。关键模块:`agent.rs`(`AgentHandle` actor,`spawn`/`submit`/`next_event`)、`session.rs`(turn loop 驱动 + 中断)、`providers/`(`ModelClient` trait + `anthropic`/`openai_responses`/`openai_chat` 三个 wire codec + `registry`)、`tools/`(`Tool` trait + 内置 Bash/Edit/Patch/Grep/Glob/Task)、`permissions.rs`、`recorder.rs`+`replay.rs`(事件日志落盘与 fold 重建)、`checkpoint.rs`(影子 git)、`compact.rs`、`hooks.rs`/`agents.rs`/`commands.rs`/`skills.rs`/`instructions.rs`(扩展与 AGENTS.md)、`config.rs`。
- `tao-apply-patch`:patch DSL 解析与执行(自研承重抽象)。
- 前端 crate 都是 core 的消费者:`tao-mcp` 把 core 会话能力对外提供工具;`tao-acp` 把 core 事件翻译成 ACP。**新增前端 = 再写一个消费者,不要给 core 加方法**。

三个承重决策(理解了它们就理解了大半个项目):

1. **一个协议,两种传输** —— `Op`/`Event` 同一对类型既走 in-process `tokio::mpsc`(TUI/exec),也走 JSONL over stdio(`tao proto`)/ socket(`tao serve`)。第二前端的边际成本 ≈ 0。
2. **事件溯源** —— 会话落盘为 append-only JSONL(`recorder.rs`);`replay.rs` 把 `LogEvent` 序列 fold 成内存 `SessionState`。fork / resume / rewind / checkpoint / share 全部建立在日志重放之上。日志没有"修改"语义,compaction、权限升级都是新事件。
3. **模型无关的规范 `Model` 格式** —— 每种 wire 协议是双向 codec,agent loop 只看见规范格式,reasoning / prompt caching 等能力不被抹平成最小公分母。

## core ↔ UI 协作(改协议前必读)

- core 与前端之间**没有方法调用,只有异步消息**:`submit(Op)` → `next_event(Event)`。前端不持有可变会话状态,它渲染的只是事件流的投影。这是 TUI 崩溃后能重放恢复、web 能中途 attach 的前提。
- **审批是唯一需要应答的事件**:`EventMsg::ApprovalRequest { call_id }` → `Op::ApprovalResponse { call_id, decision }`。`call_id` 必须严格回显,不匹配 core 返回 `Error`。
- **权限在 core,UI 只渲染**。模式 `default`/`plan`/`accept-edits`/`bypass` + 规则引擎 + 审批全是协议一等公民;`plan` 模式本身就是一个权限 profile。UI 无权决定操作是否安全。
- `Op`/`EventMsg` 用 `#[serde(tag = "type", rename_all = "snake_case")]`;新增变体 = 次要版本,删除/改义 = 主版本。wire 从 v1 起稳定是硬要求。wire 模式下 stdout 只走协议,日志走 stderr/文件。
- 取消:`Op::Interrupt` 经 `tokio_util::sync::CancellationToken` 传播到模型 SSE、exec 子进程、MCP 调用;drop 即取消。

完整契约见 `docs/design/protocol.md`、`agent-loop.md`。

## Conventions & gotchas

- **公共 API 用 `thiserror` 领域错误,不用 `anyhow`**:`ModelError`/`ToolError`/`PatchError`/`ConfigError`/`HookError`/`ProtocolError`,UI 按类别渲染(可重试网络错误 vs 权限拒绝 vs 模型 fatal)。`anyhow` 只在二进制(`tao-cli`)里用。
- **proto/serve 模式下绝不能往 stdout 写日志** —— stdout 是协议线(JSONL)。`tracing` 写文件(`~/.tao/log/`)/stderr。
- **配置分层**:内置默认 < `~/.tao/config.toml` < `<repo>/.tao/config.toml` < `TAO_*` 环境变量 < `-c key=value` / `--model` < `--profile`。用 `toml` crate 手写解析(**不是** figment)。`-c` 接 dot-path,如 `-c model_providers.local.base_url=http://localhost:11434/v1`。
- **指令文件 `AGENTS.md`**(兼容 `CLAUDE.md`/`TAO.md`):层级发现并合并进 system prompt 的缓存前缀;`@path/to/file` 引用就地展开。
- ⚠️ **`docs/design/` 是设计意图/目标架构,部分尚未落地**。它点名的一些 crate 并非实际依赖(如 figment、keyring、git2、tiktoken、rmcp、agent_client_protocol、insta、proptest、assert_cmd、criterion)—— 引用任何 crate 或 API 前先核对 `Cargo.toml` 与源码。例:`tao-mcp` 当前**未用** rmcp,`tao-acp` **手写** JSON-RPC 而非用 agent_client_protocol。读 `docs/design/` 看"为什么",读源码看"现在是什么"。
- **测试**:纯 `#[test]`/`#[tokio::test]` + `wiremock`(HTTP/SSE mock)+ `tempfile`。**没有** insta/proptest/assert_cmd。脚本化 mock 模型是 `crates/tao-core/tests/turn_loop.rs` 里的**私有** helper(`MockModel::new(vec![text_turn(...)/tool_turn(...)])`),不是 testing.md 描述的公开 `testutil::MockModel::from_script`。测试集中在 tao-core / tao-protocol / tao-apply-patch;exec/server/mcp/acp 几乎没有测试。
- 改动协议类型或 core 公共 trait 后,记得跑 `cargo ci`(fmt + clippy deny + test)—— 这是合并门禁。
