# tao 交接文档

> 用于在另一台机器上继续开发。最后更新:2026-07-28,M2-2 Edit/Patch/Grep/Glob 工具闭环完成。

## 项目状态速览

tao 是一个用 Rust 构建的 coding agent,对标 Claude Code / codex CLI / gemini-cli / opencode。架构:通用 agent harness 内核(`tao-core`)+ 多前端(`tao-tui` 优先,后期 web/gui/ACP)。

**当前里程碑进度:**

| 里程碑 | 状态 | 内容 |
|---|---|---|
| M0 骨架 | ✅ 完成 | 9 crate workspace + 协议类型(Op/Event/LogEvent)+ CI |
| M1 会说话的 loop | ✅ 完成 | 三协议 provider codec + Bash/Read/Write 工具 + turn loop + tao exec + 最小 TUI |
| M2 配置体系 | ✅ 完成(提前) | 分层 config.toml + provider 注册表 + -c/--profile/--model CLI |
| M2-1 权限引擎 | ✅ 完成 | 三层判定(模式×规则×会话决策)+ 逃逸分析 + Approver trait + TUI 审批弹窗 + exec on_ask + bypass flag |
| M2-2 工具闭环 | ✅ 完成 | Edit(字符串替换)+ Patch(apply-patch DSL,事务性)+ Grep(rg+fallback)+ Glob |
| M2 其余 | ⬜ 未开始 | 会话持久化、TAO.md、compaction、markdown 渲染 |
| M3–M6 | ⬜ 未开始 | MCP/hooks/子agent、ACP、shadow-git checkpoint、OS 沙箱、web-ui |

**测试:`cargo ci` 全绿(fmt + clippy -D warnings + test)。含权限引擎单测 + turn_loop 审批矩阵 + apply-patch 12 单测(解析/寻址/事务性)+ Edit/Grep/Glob 单测。**

## 在另一台机器上续开发

### 1. 环境准备

```bash
# Rust 工具链(rustup + stable)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile default
source "$HOME/.cargo/env"

# 验证
cargo --version   # 应 >= 1.88(用了 edition 2024 的 let-chain 语法)
```

### 2. 拉取代码

```bash
git clone git@github.com:cqqccqc/tao.git
cd tao
```

### 3. 验证构建

```bash
cargo ci   # 等价于 cargo xtask ci:fmt --check + clippy -D warnings + test
# 应全绿,66 个测试通过
```

### 4. 配置 provider(用于真实调用测试)

```bash
mkdir -p ~/.tao
cat > ~/.tao/config.toml << 'EOF'
model = "anthropic/claude-sonnet-4-6"

# 自定义 provider 示例(Anthropic 风格代理网关)
[model_providers.meituan]
name = "美团 AIGC 网关"
base_url = "https://aigc.sankuai.com/v1/anthropic"
wire_api = "anthropic"
anthropic_auth = "bearer"   # 网关要 Authorization: Bearer;原生 Anthropic 用 api-key(默认)
env_key = "MEITUAN_AIGC_KEY"

# OpenAI 兼容生态示例
[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com"
wire_api = "openai-chat"
env_key = "DEEPSEEK_API_KEY"

# profile 示例:一键切换
[profiles.work]
model = "openai/gpt-5.1"
permission_mode = "accept-edits"
EOF
```

### 5. 跑起来

```bash
# 设置某个 provider 的 key
export ANTHROPIC_API_KEY="sk-ant-..."
# 或:export MEITUAN_AIGC_KEY="..."
# 或:export OPENAI_API_KEY="..." (配 OPENAI_BASE_URL 走兼容生态)

# headless 单轮
cargo run -p tao-cli --bin tao -- exec "列出当前目录文件"

# 交互式 TUI
cargo run -p tao-cli --bin tao --

# 切 profile
cargo run -p tao-cli --bin tao -- --profile work exec "hi"

# 命令行覆盖单个配置
cargo run -p tao-cli --bin tao -- -c model=deepseek/deepseek-chat exec "hi"
```

## 代码结构

```
tao/
├── crates/
│   ├── tao-protocol/      # Op/Event/LogEvent 线协议类型(纯 serde,无 core 依赖)
│   ├── tao-core/          # 通用 agent harness(本项目核心)
│   │   ├── src/
│   │   │   ├── config.rs       # 分层配置 + provider 定义 + [permissions] rules(M2)
│   │   │   ├── model.rs        # 规范模型格式(ModelRequest/ModelStreamEvent/ModelError)
│   │   │   ├── permissions.rs  # 权限引擎:三层判定 + 逃逸分析 + Approver trait(M2-1)
│   │   │   ├── session.rs      # turn loop 主循环(run_turn)
│   │   │   ├── providers/      # ModelClient trait + 三个 codec + 公共 HTTP/SSE 层 + registry
│   │   │   │   ├── mod.rs
│   │   │   │   ├── common.rs   # HttpSseClient(重试/超时/取消)
│   │   │   │   ├── anthropic.rs
│   │   │   │   ├── openai_chat.rs
│   │   │   │   ├── openai_responses.rs
│   │   │   │   └── registry.rs # 从 Config 构造 ModelClient(M2)
│   │   │   └── tools/          # Tool trait + Bash/Read/Write/Edit/Patch/Grep/Glob(M2-2)
│   │   └── tests/              # wiremock SSE fixture + MockModel + config
│   ├── tao-cli/           # `tao` 二进制:clap 调度 -c/--profile/--model + 子命令
│   ├── tao-tui/           # ratatui 前端(inline viewport + 流式文本)
│   ├── tao-exec/          # headless runner(text/json 输出)
│   ├── tao-server/        # stdio/socket wire 传输(M4 stub)
│   ├── tao-apply-patch/   # patch DSL:parse + L1 文本 fuzz 寻址 + 事务性写盘(M2-2)
│   ├── tao-mcp/           # MCP 客户端/服务端(M3 stub)
│   └── tao-acp/           # ACP 适配层(M4 stub)
├── xtask/                 # `cargo ci` = cargo xtask ci
├── docs/design/           # 完整设计文档(15 篇,中文)
└── .github/workflows/ci.yml
```

## 关键设计决策(必读)

1. **一个协议,两种传输**:`Op`/`Event` 枚举同时跑 in-process channel(TUI)和 wire JSONL(exec/proto/serve)。第二前端边际成本≈0。
2. **规范模型 + codec**:core 内部用 `ModelRequest`/`ModelStreamEvent`(provider 无关);每个 provider 是双向 codec。agent loop 只看规范格式。
3. **Tool 错误也是输出**:`ToolError` 不上抛,转成 `ToolOutput::error` 回灌给模型。模型看到"工具失败"会自我修正。
4. **config 分层**:默认 < `~/.tao/config.toml` < `<repo>/.tao/config.toml` < `TAO_*` 环境变量 < `--profile` < `-c key=value`。
5. **Anthropic 双 auth**:`anthropic_auth = "api-key"`(原生,`x-api-key`头)或 `"bearer"`(代理网关,`Authorization: Bearer`)。
6. **权限三层 + Approver trait**:`PermissionEngine.decide` = `first_match(会话决策, 规则引擎, 模式默认值)`;`Ask` 时 `run_turn` 经 `Approver` trait await 前端(TUI 弹窗 / exec 按 `--on-ask`)。逃逸分析 v1 只减少打扰(非安全边界,M5 OS 沙箱兜底);不可解析⇒升级 Ask。`Tool::permission_key` 声明权限维度(Bash argv / Path / Domain)。
7. **Edit/Patch + apply-patch**:Edit 靠 `old_string` 唯一性(不强制先 Read,v1);Patch 用 apply-patch DSL,语法/语义分离(parse→L1 文本 fuzz 寻址→事务性写盘,失败即拒不猜测),L2 AST 锚定留后续。Grep 优先 `rg` 子进程,fallback `regex` 遍历;Glob 用 `glob` crate。

## 已知问题与局限

### M1 局限(逐步修复中)
- **TUI 无法中途中断 turn**:`run_turn` 是同步回调式,cancel 句柄没暴露给 UI。审批 await 期间可响应按键,但 turn 中断仍留 `AgentHandle` actor(M2 后续)。
- **单行输入**:无 tui-textarea,多行/粘贴/历史搜索留 M2。
- **纯文本渲染**:无 markdown/代码高亮/diff 视图,留 M2。
- ~~无审批~~ → ✅ M2-1 已实现(权限三层 + TUI 弹窗 + exec on_ask)。
- **无会话持久化**:turn 结果不落盘,JSONL 日志留 M2。ApproveForSession 的会话授权目前仅内存(resume 重放留会话持久化任务)。

### 配置体系测试结论(2026-07-27)
用美团 AIGC 网关(`https://aigc.sankuai.com/v1/anthropic`)测试时:
- ✅ 配置加载正常:`~/.tao/config.toml` 被读到,provider 解析正确
- ✅ auth_kind 切换正常:`anthropic_auth = "bearer"` 生效,网关接受了 `Authorization: Bearer`
- ⚠️ 模型名不匹配:网关返回 "invalid model name for appId"——key 有效但模型名需确认网关侧支持的名称
- **这不影响 tao 本身**——换一个能用的 key/模型名即可跑通

### 其他
- `default_user_config()` 用 `HOME` 环境变量拼 `~/.tao/config.toml`(不走 XDG,对齐 codex/claude-code 风格)
- 2024 edition 用了 let-chain 语法(`if let ... && let ...`),需 Rust >= 1.88

## 下一步(M2 剩余,按建议顺序)

1. ✅ **权限引擎 + 审批弹窗**(M2-1 已完成):`permissions.rs` + `Tool::permission_key` + `Approver` trait + TUI 弹窗 + exec `--on-ask` + `--dangerously-bypass-permissions`。
2. ✅ **Edit/Patch + Grep/Glob**(M2-2 已完成):`tools/edit.rs`+`patch.rs`+`grep.rs`+`glob.rs` + `tao-apply-patch` 引擎(parse + L1 文本 fuzz 寻址 + 事务性写盘)。
3. **会话持久化**——`recorder.rs` + `replay.rs`:JSONL 事件日志 + resume/fork。
4. **TAO.md 指令文件**——`instructions.rs`:层级发现,注入 system prompt。
5. **compaction**——上下文压缩(摘要)。
6. **markdown 流式渲染**——TUI 升级。

详见 `docs/design/roadmap.md` 的 M2 行。

## 常用命令速查

```bash
# 开发循环
cargo ci                          # fmt + clippy + test(本地 CI)
cargo test -p tao-core            # 只测 core
cargo test -p tao-core --test config   # 只测配置
cargo build -p tao-cli --bin tao  # 编译二进制
cargo run -p tao-cli --bin tao -- exec "..."   # 运行

# 调试
RUST_LOG=tao_core=debug cargo run -- exec "..." 2>tao.log   # 开日志(写 ~/.tao/log/,M2 实现)
tao exec --json "..." | jq .                       # JSON 事件流,方便管道处理

# 文档
ls docs/design/                   # 15 篇设计文档
```

## 联系点

- 设计文档:`docs/design/`(README.md 是索引)
- 路线图:`docs/design/roadmap.md`
- 配置参考:`docs/design/config.md`
- 协议定义:`docs/design/protocol.md` + `crates/tao-protocol/src/`
- 提交历史:`git log --oneline`,每个里程碑一个 feat commit
