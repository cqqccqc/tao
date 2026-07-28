<div align="center">

# tao

**一个内核、一份协议、多种形态的 Rust coding agent。**

A Rust coding agent built around a universal harness core and one wire-stable protocol.

[English](README.md) | [简体中文](README_cn.md)

[![CI](https://github.com/cqqccqc/tao/actions/workflows/ci.yml/badge.svg)](https://github.com/cqqccqc/tao/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable%20·%20edition%202024-orange.svg)](https://www.rust-lang.org/)
[![Stage](https://img.shields.io/badge/stage-alpha%20·%20dogfood-yellow.svg)](#路线图)
[![Platform](https://img.shields.io/badge/platform-macOS%20·%20Linux-lightgrey.svg)](#安装)

</div>

tao 是一个用 Rust 构建的 coding agent。它的核心不是某个终端 UI，而是一个**与 UI 形态无关的 agent harness**（`tao-core`）：会话、模型、工具、权限、持久化、扩展能力全部通过同一份 `Op`/`Event` 协议暴露。TUI、headless 执行、stdio 协议、常驻服务、ACP 编辑器内嵌、MCP server——都是这份协议的不同消费者。

设计上对标 [Claude Code](https://github.com/anthropics/claude-code)、[OpenAI codex CLI](https://github.com/openai/codex)、[gemini-cli](https://github.com/google-gemini/gemini-cli) 与 [opencode](https://github.com/sst/opencode)：把经典 coding agent 体验做到位，同时在**多前端复用、会话资产化、模型无关性、可编程扩展**四条线上做得更彻底。

> **状态：alpha / dogfood 阶段。** M0–M4 主体完成，作者日常开发已自用 tao 完成。M5（硬化）与 M6（web 前端）进行中。详见 [路线图](#路线图)。

---

## ✨ 特性

**一个内核，多种形态**
- 单一 `Op`/`Event` 协议同时驱动 in-process TUI 与 JSONL-over-stdio / socket——加一个新前端的边际成本接近于零。
- 同一个二进制 `tao` 既是终端 agent，也是 headless 脚本工具，也能被 Zed 等编辑器通过 [ACP](https://agentclientprotocol.com) 拉起，还能反过来把自己暴露为 MCP server。

**多 provider，能力不抹平**
- 三套线协议 codec：Anthropic Messages · OpenAI Responses · OpenAI Chat Completions。
- core 内部维护**规范模型格式**作为唯一中间层，agent loop 只看见它；reasoning / thinking / prompt caching 在规范格式里是一等字段，不做“最小公分母”。
- `base_url` 可自定义，一键接入 DeepSeek / Qwen / Kimi / OpenRouter / Ollama 等 OpenAI 兼容生态。

**会话即资产**
- append-only JSONL 事件日志是唯一真相，内存状态可由日志完整重放重建。
- `fork` 探索分支、`rewind` 回退对话、影子 git `checkpoint` 一键回滚文件改动、`share` 导出净化版 transcript、完整权限审计轨迹——全部建立在事件溯源之上。

**可编程平台，不造轮子**
- hooks（8 个事件点）· markdown 子 agent · slash 命令 · MCP · skills（渐进披露），**进程 + 文件系统优先**，不做二进制插件 ABI。
- 覆盖 Claude Code 生态价值的绝大部分，实现成本只是 spawn 进程与解析 markdown。

**权限在内核，UI 只渲染**
- 权限模式（`default` / `plan` / `accept-edits` / `bypass`）+ 规则引擎 + 审批是协议里的一等公民；UI 无权决定一个操作是否安全，`plan` 模式本身就是一个权限 profile。

**Rust 单二进制**
- 快、内存安全、零运行时依赖；崩溃也能落盘日志，`resume` 恢复成功率是硬指标。

### 内置工具

| 工具 | 说明 |
|---|---|
| `Bash` | 流式输出、超时、cancel-on-drop、输出截断 |
| `Read` / `Write` / `Edit` | 精确编辑（须先 Read，唯一匹配才替换），写前生成 diff |
| `Patch` | apply-patch DSL，多文件事务性增改删 + 移动，语法/语义分离寻址 |
| `Glob` / `Grep` | 文件名模式搜索；ripgrep 包装（fallback 内置遍历） |
| `WebFetch` / `WebSearch` | 抓取转 markdown / provider 原生搜索 |
| `Task` | 子 agent，独立会话与只读权限 |
| `Plan` | 模型维护的 checklist |

---

## 📦 安装

> tao 尚未发布到 crates.io / Homebrew（计划中，见 [路线图](#路线图)）。当前从源码构建。

**前置**：[Rust stable](https://www.rust-lang.org/tools/install)（toolchain 固定见 `rust-toolchain.toml`）。

```bash
git clone https://github.com/cqqccqc/tao.git
cd tao
cargo build --release        # 产物：target/release/tao
# 可选：装到 PATH
cargo install --path crates/tao-cli
```

平台支持：macOS、Linux。Windows 完整支持（PTY / 进程组 / 沙箱）属于 M5 非目标，暂不在主线。

---

## 🚀 快速开始

tao 不绑定单一模型供应商。最快的方式是用一个 provider 的 API key：

```bash
export ANTHROPIC_API_KEY="sk-..."     # 或 OPENAI_API_KEY / DEEPSEEK_API_KEY / ...
tao                                     # 启动 TUI（默认子命令）
```

在 TUI 里直接输入任务：

```
> 把 src/render.rs 里的渲染循环改成帧节流，并补一个测试
```

tao 会读取文件、编辑、跑测试，每一步需要审批时弹出确认。按 `?` 查看键位，`/help` 查看命令。

**headless 一次执行**（适合脚本 / CI）：

```bash
tao exec "跑 cargo clippy 并修复所有 warning" --json
```

**接入任意 OpenAI 兼容端点**（如本地 Ollama）：

```bash
tao -c model_providers.local.base_url=http://localhost:11434/v1 \
    -c model_providers.local.wire_api=openai-chat \
    -c model=local/qwen2.5-coder
```

---

## 🧑‍💻 命令

`tao` 是一个薄壳二进制，按子命令分发到不同前端 crate：

| 命令 | 说明 | 状态 |
|---|---|---|
| `tao` / `tao tui` | 终端界面（ratatui inline viewport，流式 markdown + diff 着色） | ✅ |
| `tao exec "<prompt>"` | headless 单次执行；`--json` 输出事件流，`--on-ask deny\|approve` 控制审批 | ✅ |
| `tao proto` | 协议模式：stdin/stdout 走 JSONL `Op`/`Event`，供任意语言前端/脚本组合 | ✅ |
| `tao acp` | ACP 模式：被 Zed 等编辑器以 stdio 拉起内嵌 | ✅ |
| `tao sessions ls\|show\|share\|audit\|gc` | 会话管理：fork 树 / 预览 / 净化导出 / 权限审计 / 清理 | ✅ |
| `tao serve --port N` | 常驻服务：TCP/WS 多客户端 attach 同一会话 | 🚧 规划中 |
| `tao mcp-serve` | 把 tao 自身暴露为 MCP server（list/send/read session） | 🚧 规划中 |
| `tao login\|logout\|auth` | OAuth / API key 交互登录 | 🚧 规划中（当前用环境变量） |

常用全局参数：`-c key=value`（覆盖配置，支持点路径）、`--profile <name>`、`--model provider/id`、`--resume <id>`、`--fork`、`--dangerously-bypass-permissions`。

### 会话即资产，随手用

```bash
tao sessions ls                      # fork 树 + 标题
tao sessions show <id>               # 预览消息摘要
tao sessions share <id> > out.md     # 导出净化版 transcript（自动打码 secret）
tao sessions audit <id>              # 每一次权限判定的来源与结论
tao --resume <id> --fork             # 从历史会话分叉一条新分支继续
```

---

## ⚙️ 配置

分层加载，后者覆盖前者：

```
内置默认 < ~/.tao/config.toml（用户级） < <repo>/.tao/config.toml（项目级）
        < 环境变量 TAO_* < CLI flags（-c / --model） < --profile 覆盖
```

`~/.tao/config.toml` 示例：

```toml
model = "anthropic/claude-sonnet-4-6"
model_reasoning_effort = "medium"
permission_mode = "default"            # default | plan | accept-edits | bypass

[model_providers.anthropic]
base_url = "https://api.anthropic.com"
wire_api = "anthropic"                 # anthropic | openai-responses | openai-chat
env_key = "ANTHROPIC_API_KEY"

[model_providers.deepseek]             # 任意 OpenAI 兼容端点
base_url = "https://api.deepseek.com"
wire_api = "openai-chat"
env_key = "DEEPSEEK_API_KEY"

# 权限规则:命令前缀 / 路径 glob,allow | deny | ask
[[permissions.rules]]
tool = "Bash"
pattern = "cargo *"
action = "allow"

# MCP server
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/data"]
```

**认证**：当前优先环境变量（`env_key`，CI 友好）。OS keychain 与 OAuth 交互登录属于 `tao login` 子命令，开发中。

**指令文件 `AGENTS.md`**（兼容 `CLAUDE.md` / `TAO.md`）：层级发现 `~/.tao/AGENTS.md` → `<repo>/AGENTS.md` → 子目录，合并进 system 提示的缓存前缀；支持 `@path/to/file` 引用展开。

完整参考见 [docs/design/config.md](docs/design/config.md)。

---

## 🧩 扩展性

五种机制，各司其职，均为进程 + 文件系统：

| 机制 | 形态 | 适合 |
|---|---|---|
| **hooks** | 进程 + JSON stdin/stdout | 策略守门、格式化、通知 |
| **子 agent** | markdown 定义 + `Task` 工具 | 上下文隔离的探索 / 评审 / 并行 |
| **slash 命令** | markdown 模板 | 提示词复用、工作流封装 |
| **MCP** | 独立 server 进程（stdio / HTTP） | 外部系统与数据源接入 |
| **skills** | `SKILL.md` + 资源包，渐进披露 | 领域知识 / 流程方法论 |

```markdown
<!-- ~/.tao/agents/explorer.md —— 一个子 agent 定义 -->
---
name: explorer
description: 只读代码探索与定位;当需要理解陌生代码区域时使用
tools: [Read, Grep, Glob, Bash]
---
你是代码探索专家。用最小的读取量回答“X 在哪里 / 如何工作”，输出 file:line 引用。
```

内置 slash 命令：`/help /clear /compact /plan /mode /model /sessions /rewind /rollback /diff /cost /hooks /mcp /agent /init`。

详见 [docs/design/extensibility.md](docs/design/extensibility.md)。

---

## 🏗️ 架构

一个 Cargo workspace，二进制保持极薄，所有能力在库里：

```
                    ┌─► tao-tui      (ratatui 终端界面)
                    ├─► tao-exec     (headless 单次执行)
                    ├─► tao-server   (stdio proto / socket serve)
 tao-cli ───────────┼─► tao-acp      (ACP 适配,被 Zed 拉起)
 (薄壳调度)          ├─► tao-mcp      (MCP 客户端 + tao 自身作 MCP server)
                    └─► tao-core ──► tao-protocol (Op/Event + LogEvent 类型)
                            │
                            └─► tao-apply-patch (patch DSL 解析与执行)
```

三条承重设计：

1. **单一协议，两种传输** — `Op`/`Event` 同时用于 in-process channel 与 JSONL over wire。这是从 codex 学到并确认的关键决策：第二前端的边际成本 ≈ 0。
2. **事件溯源** — 会话落盘为 append-only JSONL；fork = 换 `session_id` 重放前缀；resume/rewind/checkpoint/share 全建立在日志之上。
3. **模型无关的规范模型** — 每种线协议各自做双向 codec，agent loop 只看见规范格式，provider 特性不被抹平。

完整设计文档（架构 / 协议 / agent loop / providers / tools / 权限 / 会话 / 配置 / 扩展 / ACP / TUI / 测试）见 **[docs/design/](docs/design/README.md)**。

---

## 🗺️ 路线图

| 里程碑 | 目标 | 状态 |
|---|---|---|
| **M0 骨架** | workspace + 协议类型先行 + CI | ✅ |
| **M1 会说话的 loop** | 端到端单轮对话：3 个 wire codec + 工具 + turn loop + 最小 TUI | ✅ |
| **M2 真正的 coder** | 权限+审批、Edit/Patch/Grep/Glob、AGENTS.md、事件日志+resume/fork、compaction、markdown 渲染 | ✅ |
| **M3 可扩展平台** | slash 命令、hooks、子 agent、MCP（OAuth 跳过留后续） | ✅ 4/5 |
| **M4 会话与协作** | skills、ACP、shadow-git checkpoint+rollback、会话浏览器、share 导出（serve / mcp-serve 留后续） | ✅ 5/7 |
| **M5 硬化** | OS 沙箱（seatbelt/landlock）、逃逸分析加强、性能与崩溃恢复审计、Windows 评估 | 🚧 |
| **M6 tao-web-ui** | 第二前端验证协议：web 消费 serve 协议、多客户端 attach、反向验证 core 无 UI 假设 | ⬜ |

每个里程碑结束即可发布。M2 起进入 dogfood——tao 的开发本身用 tao 完成。

完整路线图、风险登记与非目标见 [docs/design/roadmap.md](docs/design/roadmap.md)。

---

## 🤝 贡献

欢迎 issue 与 PR。本地开发：

```bash
cargo fmt --all --check        # CI 强制
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI（`RUSTFLAGS="-D warnings"`）在 macOS + Linux 上跑 fmt / clippy / test 全绿是合入门槛。xtask 提供快照更新与 fixture 生成等开发脚本。

设计决策与取舍都记录在 [docs/design/](docs/design/README.md)，动手前建议先读架构与协议两篇。

---

## 📄 License

[MIT](LICENSE)。

## 致谢

tao 的设计大量借鉴了 [Claude Code](https://github.com/anthropics/claude-code)、[OpenAI codex CLI](https://github.com/openai/codex)、[gemini-cli](https://github.com/google-gemini/gemini-cli) 与 [opencode](https://github.com/sst/opencode) 的理念与教训——“能抄不造”，只对承重抽象（model client、patch 引擎）自研。感谢这些项目让 coding agent 成为一种可复用的形态。
