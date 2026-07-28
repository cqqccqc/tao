# M2/M3 实现摘要(供 code review)

> 对应 commit:M2-1 `77e1475`、M2-2 `1bf561c`、M2-3 `b7ce662`、M2-4 `6829848`、M2-5 `c0adfcd`、M2-6 `e0bc0c1`、M3-1 `5cc0fe3`、M3-2 `3102b83`、M3-3 `509429b`、M3-4 `62eaee2`。`cargo ci` 全绿。**M2 全部完成**,M3 进行中(slash/hooks/子agent/MCP,剩 OAuth)。
> 设计文档:`docs/design/{permissions,tools,config,sessions}.md`。本文是**实现层**摘要,对照代码 review 用。

---

## M2-1 权限引擎 + 审批弹窗

### 核心:`crates/tao-core/src/permissions.rs`
- `PermissionEngine { mode, rules, session_grants }`——内部 `Mutex` 提供可变性,故 `decide`/`grant`/`set_mode` 只需 `&self`(便于 `Arc` 跨 turn/task 共享)。
- `decide(tool, key) -> Decision` = `first_match(会话决策 → 规则引擎 → 模式默认值)`。
- 规则匹配(`match_one`):`tool` 支持 `|` 多选;Bash 前缀 glob / Path glob(globset)/ Domain;`deny > ask > allow`(取 max),同 action 则 pattern 更长(更具体)优先。
- 逃逸分析(`analyze_bash`,**v1 非安全边界**):`bash -lc` 拆 `&&`/`||`/`;`/`|` 多段聚合(最严:全 allow 才 allow);`$()`/反引号/`${` 不可解析 ⇒ 升级 Ask;`sudo`/`env`/`xargs`/`find -exec`/`git -c` 危险包装 ⇒ 升级 Ask。

### 接入点
- `Tool::permission_key(args, cwd) -> Option<PermissionKey>`(默认 None);`bash.rs` 提取 argv,`fs.rs` 的 Read/Write 提取 path。
- `session.rs::run_turn` 签名加 `engine: &PermissionEngine` + `approver: &dyn Approver`;每个工具调用前判定:
  - `Allow` → 执行;`Deny` → `ToolOutput::error("被权限策略拒绝")`;`Ask` → `approver.request().await`
  - 审批响应:`Approve`/`ApproveForSession`(后者 `engine.grant`)→ 执行;`Deny` → error;`Abort` → `StopReason::Interrupted` 中断 turn
- `Approver` trait(`async fn request(ApprovalRequest) -> ReviewDecision`):
  - exec `HeadlessApprover`:按 `--on-ask`(默认 deny)直接决策
  - tui `TuiApprover`:`oneshot` + `HashMap<call_id>` 配对;主循环收 `TurnEvent::ApprovalRequest` 渲染弹窗,按键 y/s/n/a 后 `respond()`
- `TurnEvent` 加 `ApprovalRequest`/`ApprovalResolved`(exec 可输出,TUI 可渲染)
- config:`[permissions]` rules(只放 rules,mode 用顶层 `permission_mode`);CLI `--dangerously-bypass-permissions`(设 bypass)+ exec `--on-ask <deny|approve>`
- TUI:shift+tab 循环 default→plan→accept-edits(不含 bypass),状态栏显示模式

### 测试
- `permissions.rs` 18 单测:模式默认矩阵、deny>ask>allow、具体度优先、`|` 多选、Path/Domain glob、会话 grant 优先、bash -lc 拆分/不可解析/危险包装
- `tests/turn_loop.rs` 审批矩阵 6 例:Ask→Approve 执行 / Ask→Deny error / Ask→Abort 中断 / Plan 拒写(不审批)/ ApproveForSession 二次免审 / allow 规则跳过

---

## M2-2 Edit/Patch/Grep/Glob

### `tao-apply-patch`(引擎,`crates/tao-apply-patch/src/lib.rs`)
- `parse(input) -> Vec<Hunk>`:DSL(`*** Begin/End Patch`、`*** Add/Update/Delete/Move File`、`@@ 锚`、`+`/`-`/` ` 行)→ Hunk,有错即拒。
- `apply(hunks, base) -> String(diff)`:L1 文本 fuzz(归一化空白后滑动窗口匹配 `seek_context + remove`);多匹配⇒歧义拒绝,无匹配⇒拒绝;**事务性**(全部 hunk 寻址成功才写盘,临时文件 + rename);输出 similar unified diff。

### 工具(`crates/tao-core/src/tools/`)
- `edit.rs`:`old_string` 唯一性(除非 `replace_all`)+ similar diff;permission_key=Path
- `patch.rs`:调 `parse`+`apply`;失败返回 `Ok(ToolOutput::error)`(模型可见,非 Err);permission_key=None(多文件,走 mode 默认)
- `grep.rs`:rg 子进程优先(argv/超时/cancel/截断,复用 Bash 模式);rg 不在 PATH 时 `regex` + 递归遍历 fallback(跳过 .git/target/node_modules 等);permission_key=None
- `glob.rs`:`glob` crate 遍历,mtime 倒序限 100;permission_key=None
- `permissions::tool_class` 加 `Grep`/`Glob` → `Read`(否则会被归 Other 触发 Ask)
- `ToolRegistry::builtin` 注册 7 工具

### 测试
- apply-patch 12 单测:解析 round-trip、Move、寻址(唯一/空白容忍/歧义拒绝/未找到)、Add/Delete/Move、事务性(中途失败不写盘)
- 各工具单测(edit 唯一性/replace_all/diff、grep fallback、glob 模式)

---

## M2-3 AGENTS.md 指令文件

### `crates/tao-core/src/instructions.rs`
- `load(cwd) -> Option<String>`:发现 `~/.tao/AGENTS.md`(全局)+ `<cwd>/AGENTS.md`(项目),兼容 `CLAUDE.md`/`TAO.md`(`AGENTS.md` 优先);合并全局→项目(后写优先)。
- exec/tui 构造 system 时前置 instructions `SystemBlock`(若 `Some`)。
- 顺带:system prompt 工具列表补 Edit/Patch/Grep/Glob。

### 测试
- 4 单测:项目 AGENTS.md、fallback CLAUDE.md、AGENTS.md 优先于 CLAUDE.md、(全局+项目合并由代码 `join` 保证)

---

## M2-4 会话持久化(recorder + replay + resume/fork)

### `crates/tao-core/src/recorder.rs`
- `Recorder` trait(`fn record(LogEvent)`)+ `NullRecorder`(测试)+ `JsonlRecorder`(JSONL append+flush,`seq` AtomicU64 续,`ts` SystemTime)。
- `JsonlRecorder::create/open_existing/create_fork`(~/.tao/projects/<slug>/sessions/<id>.jsonl;fork 写 `SessionMeta{parent}`);`session_dir`/`session_file_path` 供 CLI 扫描。

### `crates/tao-core/src/replay.rs`
- `SessionState { id, parent, cwd, title, messages, session_grants, mode }`;`replay(path)` fold LogEvent → SessionState。
- `UserInput`/`AssistantMessage` → ModelMessage;`ToolResult`(output 约定 `{content,is_error}`)→ ToolResult msg;`PermissionGrant` → grants;`ModeChange` → mode。Compaction 识别未应用(M2-5)。

### 接入
- `session.rs::run_turn` 加 `recorder: &dyn Recorder`;记 `AssistantMessage`/`ToolCall`/`ToolResult`/`Approval`/`PermissionGrant`/`TurnBoundary`(各 return 点前记 boundary)。
- exec `--resume <id>`/`--fork`:replay → messages + engine(mode+grants);记 `UserInput`;输出 session id。tui 落盘(新会话;tui resume 留 TODO)。
- CLI `--resume`/`--fork`(全局,exec);`sessions ls`(扫描 jsonl)/`audit <id>`(权限轨迹)/`gc`(keep_days 删旧)。

### 测试
- recorder 4 单测(slugify/create+record/open 续 seq/read_max_seq);replay 2 单测(状态重建/tool result 配对);turn_loop 传 NullRecorder 回归。

---

## M2-5 compaction(上下文压缩)

### `crates/tao-core/src/compact.rs`
- `approx_tokens(messages)` = 内容字符数 / 4(近似;安全侧偏高早压缩)。`DEFAULT_CONTEXT_WINDOW=200_000`、`DEFAULT_KEEP_LAST=4`。
- `compact(client, model, messages, keep_last, recorder)`:摘要 `messages[..len-keep]`(调 `client.stream`,结构化 prompt 目标/决策/改动/待办)→ `[Assistant(summary)] + messages[len-keep..]`;记 `Compaction { summary, covers_through_seq }`。

### 接入
- `replay.rs::apply` 的 `Compaction` 分支应用投影:摘要替代前 `covers_through_seq` 条,保留其后(keep)。
- exec turn 前 check `approx_tokens > window * auto_compact_at` → compact(small_model 或当前 model)。
- v1:tui 不 compact(state 同步复杂,留 TODO);自动触发 only;`Op::Compact` 手动留后续。

### 测试
- compact 3 单测(approx_tokens/compact 摘要+keep/noop when too few);replay Compaction 投影单测。

---

## M2-6 markdown 流式渲染(TUI)

### `tao-tui/src/render.rs`
- `markdown_to_lines(text)`:pulldown-cmark 解析 → ratatui `Line`/`Span`。支持标题(粗体青)、段落、列表(• 缩进)、代码块(灰缩进)、行内代码(黄)、`**粗体**`/`*斜体*`、`---` Rule。
- `diff_lines(text)`:行首 `+`绿/`-`红/其余灰(工具输出与 diff 着色)。
- `draw` 历史区:`Assistant`/`live_text` 用 `markdown_to_lines`(第一行带 "tao " 前缀);`Tool` 用 `diff_lines`。

### 测试
- 4 单测(markdown 标题/代码块/行内代码;diff 着色)。

### v1 简化
- 不 syntect 语法高亮(代码块纯样式);不主题(TOML);不 inline viewport(保留 alternate screen);不 textwrap/unicode-width(ratatui Wrap);不行缓存;流式 live_text 每帧解析(容忍不完整)。

---

## M3-1 slash 命令

### `tao-core/src/commands.rs`
- `CommandDef { name, description, argument_hint, body }`:markdown 命令(frontmatter + body)。
- `load_commands(cwd)`:发现 `~/.tao/commands/` + `<cwd>/.tao/commands/`(项目优先,同名覆盖);`expand(body, args, cwd)`:`` !`cmd` `` 执行注入 + `$ARGUMENTS` 替换。
- `Builtin`:`Help`/`Clear`/`Mode(PermissionMode)`/`ModeCycle`/`Compact`/`Sessions`;`parse_builtin` + `split_name_args`。

### TUI `/` 触发(`tao-tui/src/app.rs`)
- handle_key Enter:`/` 开头 → 内置命令(`/help` 列命令、`/clear` 清历史、`/mode [default|plan|accept-edits]` 切模式、`/mode` 循环、`/compact` 压缩、`/sessions` 列会话);非内置 → markdown 模板展开为 user 消息走 run_turn。
- `/compact` 同步 await compact(M2-5);`/sessions` 扫描 session_dir(M2-4)。

### 测试
- commands 6 单测(parse_builtin/split_name_args/expand 参数+命令注入/load_commands/frontmatter)。

### v1 简化
- 内置只 `/help /clear /mode /compact /sessions`;`/model /rewind /rollback /diff /cost /hooks /mcp /agent /init` 留后续;slash 只 TUI;`/compact` 同步(UI 短暂冻结)。

---

## M3-2 hooks(事件点 + 守门)

### `tao-core/src/hooks.rs`
- `HookEvent`:`SessionStart`/`SessionEnd`/`PreToolUse{tool}`/`PostToolUse{tool}`/`Stop`。
- `HookConfig { matcher, command, timeout_ms }`:`matcher` 匹配 tool 名(`|` 多选/`*`)。
- `run_hooks`:spawn `sh -c command`,stdin JSON,超时,退出码(0 Pass / 2 Block(stderr) / 其他非阻断警告+Pass)。串行(Block 短路)。最小环境(剥离 provider 凭证)。

### 接入
- `config.rs`:`HooksConfig`(按事件点 Vec,`[hooks]` PascalCase 表)+ merge(append)。
- `session.rs::run_turn` 加 `hooks` 参数;Allow 路径 PreToolUse(Block→`ToolOutput::error`)/正常路径 PostToolUse(非阻断)。
- exec/tui 传 `config.hooks`。

### 测试
- hooks 5 单测(Pass/Block/超时/matcher 过滤/Block 短路)。

### v1 简化
- 5 事件点;串行;不 Modify;Approve/ApproveForSession 跳过 PreToolUse(TODO);项目信任不强制(TODO)。

---

## M3-3 子 agent(Task 工具)

### `tao-core/src/agents.rs`
- `SubagentDef { name, description, tools, model, system_prompt }`:frontmatter + body。
- `load_agents(cwd)`:发现 `~/.tao/agents/` + `<cwd>/.tao/agents/`(项目优先)。

### Task 工具(`tools/task.rs` + `session.rs`)
- `TaskTool`:spec only(模型知道可调);`call` 返回 error(run_turn 拦截)。
- `run_turn` 工具循环:`if tool_name == "Task"` → `exec_task`(spawn 子 run_turn:只读 Plan/fork recorder/NullApprover/max_steps 20/Box::pin 递归)→ 报告(子最后 Assistant text)。
- `ToolRegistry::readonly_subset`:子 agent 只读工具子集(Read/Grep/Glob)。

### 测试
- agents 2 单测(load/frontmatter/default tools)。

### v1 简化
- 不递归(子只读无 Task);不透传 BackgroundEvent;`/agent` 显式留后续;子权限 Plan 只读。

---

## M3-4 MCP 客户端(JSON-RPC over stdio)

### `tao-mcp/src/client.rs`
- `McpClient`:spawn MCP server(command/args)+ JSON-RPC 2.0 over stdio(initialize / tools/list / tools/call)。自实现(不引入 rmcp)。
- `McpTool`(Tool trait):`mcp__server__tool`,schema 透传,`call` → `McpClient::call_tool`。同 server 多工具共享 `Arc<Mutex<McpClient>>`。
- `load_mcp_tools`:遍历 config.mcp_servers,spawn + initialize + list_tools → 注册;失败 skip + warn。

### 接入
- `config.rs`:`McpServerConfig { command, args, env, startup_timeout_ms }` + `Config.mcp_servers` + `[mcp_servers]` 解析。
- exec/tui `run()`:构造 ToolRegistry 后调 `load_mcp_tools`。

### v1 简化
- stdio only(HTTP 留后续);不 ToolSearch/预算折叠(全暴露);不重连(启动失败 skip);全启动时 initialize(不惰性)。

---

## v1 简化 / TODO(留后续)

| 项 | v1 现状 | TODO |
|---|---|---|
| 逃逸分析 | 减少打扰,非安全边界 | M5 OS 沙箱(seatbelt/landlock) |
| 会话授权 | resume 重放 `PermissionGrant`(M2-4 已实现) | — |
| 会话持久化 | recorder+replay+resume/fork+sessions | index.redb、shadow-git checkpoint(M4)、rotate 续写、fs2 并发锁、config_fingerprint、tui resume |
| compaction | token 近似+自动压缩+投影 | 按模型 tokenizer、registry context_window、手动 Op::Compact、tui compact、covers_seq 对齐日志 seq |
| markdown 渲染 | pulldown-cmark 基本元素+diff 着色 | syntect 高亮、主题、inline viewport、textwrap、行缓存 |
| slash 命令 | 内置 /help /clear /mode /compact /sessions + markdown 模板 | /model /rewind /rollback /diff 等;exec slash;交互式参数 |
| hooks | PreToolUse/PostToolUse/Stop/SessionStart/SessionEnd + 守门 | UserPromptSubmit/Notification/SubagentStop;Modify;并行;项目信任 |
| 子 agent | Task 工具 + 独立会话 + 只读 | max_depth;BackgroundEvent 透传;/agent 显式;子权限可配 |
| MCP 客户端 | JSON-RPC over stdio + mcp__server__tool | HTTP transport;ToolSearch/预算;重连;惰性启动 |
| Edit 先 Read | 不强制(靠唯一性+diff) | `ToolCtx` 加 `read_files` 跟踪 |
| Patch 寻址 | L1 文本 fuzz only | L2 tree-sitter AST 锚定 |
| Grep fallback | 无 .gitignore(跳过常见目录) | rg 优先时无此问题;fallback 可加 ignore crate |
| AGENTS.md | 全局+项目两级 | 向上找 repo 根、子目录惰性、`@path` 展开、fingerprint hash |

---

## 验证

```bash
cargo ci                                          # fmt + clippy -D warnings + test
cargo test -p tao-apply-patch                     # patch 引擎 12 例
cargo test -p tao-core --lib permissions          # 权限 18 例
cargo test -p tao-core --test turn_loop           # 审批矩阵 + 回归

# headless(default 模式 Bash 走审批;on-ask deny 拒绝)
cargo run -p tao-cli --bin tao -- exec "列出当前目录文件"
cargo run -p tao-cli --bin tao -- --dangerously-bypass-permissions exec "运行 echo hi"
cargo run -p tao-cli --bin tao -- exec --on-ask approve "用 Grep 找 fn main"

# TUI:shift+tab 切模式;工具触发审批弹窗 y/s/n/a
cargo run -p tao-cli --bin tao --

# 会话持久化:exec 落盘 → resume/fork → sessions 管理
cargo run -p tao-cli --bin tao -- exec "列出当前目录文件"   # 输出 session id
cargo run -p tao-cli --bin tao -- sessions ls
cargo run -p tao-cli --bin tao -- --resume <id> exec "接着刚才的"
cargo run -p tao-cli --bin tao -- --resume <id> --fork exec "分叉探索"
cargo run -p tao-cli --bin tao -- sessions audit <id>

# compaction:超长会话自动压缩(单测覆盖;v1 window 200k 需大量消息)
cargo test -p tao-core --lib compact
cargo test -p tao-core --lib replay
```
