# 可扩展性:hooks、子 agent、slash 命令、MCP、技能

设计取向:**进程 + 文件系统**优先,不做二进制插件 ABI。这一套覆盖了 Claude Code 生态价值的绝大部分,而实现只是 spawn 与 markdown 解析。

## 1. Hooks

事件点(v1):`SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`Notification`、`Stop`、`SubagentStop`、`SessionEnd`。

```toml
[hooks]
PreToolUse = [{ matcher = "Bash|Task", command = "./scripts/guard.sh", timeout_ms = 5000 }]
```

执行模型(与 Claude Code 语义对齐,降低学习成本):

- core spawn 命令,**stdin 喂 JSON**:`{session_id, cwd, tool_name, tool_input, ...}`(按事件点不同)。
- 退出码语义:`0` 放行(stdout JSON 可附加修饰,如 PreToolUse 改写输入/注入上下文);`2` 阻断(stderr 作为原因反馈给模型);其他 = 非阻断错误(警告展示)。
- 并发:PreToolUse 串行(顺序即配置顺序,可短路);PostToolUse 并行;全部有超时,超时按非阻断错误处理。
- hook 的 stdout/stderr 进 `tracing`,TUI `/hooks` 面板可查看最近执行记录。
- 安全:项目级 hook 需"信任项目"(见 config.md §5);hook 进程继承最小环境,不继承 provider 凭证。

## 2. 子 agent(Subagents)

定义文件:`~/.tao/agents/<name>.md` 或 `<repo>/.tao/agents/<name>.md`:

```markdown
---
name: explorer
description: 只读代码探索与定位;当需要理解陌生代码区域时使用
tools: [Read, Grep, Glob, Bash]     # 省略 = 全部只读工具
model: haiku                         # 省略 = 继承父级
---

你是代码探索专家。目标:用最小的读取量回答"X 在哪里/如何工作"。
约束:不修改任何文件;输出给出 file:line 引用……
```

- 通过内置 `Task` 工具调用:`Task { subagent: "explorer", prompt: "找出登录流程的入口" }`。
- 运行时:`Agent::spawn` 独立会话(独立日志文件,parent 标记);权限默认只读;`max_depth = 3`。
- 父会话看到 `ToolCallBegin{Task}/ToolCallEnd{报告}`;子的 `AgentMessage` 节流透传为 `BackgroundEvent`。
- 选择逻辑:系统提示中列出可用子 agent 的 name+description,模型自行决策;用户也可 `/agent explorer ...` 显式指派。

## 3. Slash 命令

markdown 定义,与 Claude Code 相同心智:`~/.tao/commands/<name>.md`、`<repo>/.tao/commands/<name>.md`。

```markdown
---
description: 为当前改动生成提交信息
argument_hint: "[--amend]"
---
请基于 `!`git diff --staged`` 的结果,按本项目 commit 规范生成提交信息。参数:$ARGUMENTS
```

- 展开时机:用户输入 `/commit` 后,**在 core 侧展开**为普通 user 消息(`` !`cmd` `` 执行命令注入输出,`$ARGUMENTS` 占位替换),日志记录展开后内容——可审计、可回放。
- 内置命令(core 实现,非 markdown):`/help /clear /compact /plan /mode /model /sessions /rewind /rollback /diff /cost /hooks /mcp /agent /init`。

## 4. MCP

见 tools.md §5。要点回顾:core 内嵌 rmcp 客户端;stdio + streamable HTTP;`mcp__server__tool` 命名;工具预算;`tao mcp-serve` 反向暴露。

## 5. 技能(Skills,M4)

`SKILL.md` + 资源的渐进披露包(对齐 agent-skills 生态):

```
~/.tao/skills/pdf-report/
├── SKILL.md          # frontmatter: name/description/triggers;正文:方法论
├── templates/        # 模型可 Read 的资源
└── scripts/          # 模型可 Bash 执行的脚本
```

- 系统提示只注入各技能的 name+description(几十 token);模型"触发"后自行 Read 正文与资源——**上下文按需加载**,这是技能与子 agent 的本质区别(技能 = 知识包,子 agent = 隔离执行)。
- 安全:技能脚本视为不可信内容,执行走正常 Bash 审批。

## 6. 扩展点汇总与边界

| 机制 | 形态 | 适合 | 不适合 |
|---|---|---|---|
| hooks | 进程 + JSON stdin/stdout | 策略、守门、格式化、通知 | 重计算、长任务(超时短) |
| 子 agent | markdown + Task 工具 | 上下文隔离的探索/评审/并行 | 需要交互的流程 |
| slash 命令 | markdown 模板 | 提示词复用、工作流封装 | 有状态逻辑 |
| MCP | 独立 server 进程 | 外部系统/数据源接入 | 高频低开销调用 |
| 技能 | 文件包 + 渐进披露 | 领域知识/流程方法论 | 隔离执行 |

明确不做(v1):WASM/动态库插件、UI 主题脚本化之外的 JS 扩展、远程扩展市场。
