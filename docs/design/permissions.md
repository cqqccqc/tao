# 权限与审批

## 1. 模型:模式 × 规则 × 会话决策

三层叠加,在 core 内解析(codex 的"审批是 core API,UI 只是渲染"原则):

```
最终判定 = first_match(
    会话决策(本会话内 ApproveForSession 累积),
    规则引擎(config 的 allow/ask/deny 规则,按优先级),
    权限模式默认值
)
```

### 权限模式(permission mode)

| 模式 | 含义 | 默认值 |
|---|---|---|
| `default` | 写/执行/网络类操作需要审批(除非规则允许) | read=allow, write/exec/net=ask |
| `plan` | 只读:一切 mutation 被 deny(模型被引导输出计划) | write/exec/net=deny |
| `accept-edits` | 文件编辑自动允许,执行/网络仍需审批 | write=allow, exec/net=ask |
| `bypass` | 全部允许(等价 yolo,启动时需显式 flag) | *=allow |

- **plan 模式就是权限 profile**,不是特殊循环——和 Claude Code 同款的优雅:模型看到工具被拒后会改用只读工具探索,最后输出计划文本。TUI 提供 `/plan` 切换与 shift+tab 循环切换模式。
- 模式切换本身是一个 `LogEvent`,可审计。

### 规则引擎

```toml
# ~/.tao/config.toml 或项目 .tao/config.toml
[permissions]
default_mode = "default"

[[permissions.rules]]
tool = "Bash"
pattern = "cargo *"        # 命令前缀 glob
action = "allow"

[[permissions.rules]]
tool = "Bash"
pattern = "rm *"
action = "ask"

[[permissions.rules]]
tool = "Edit"
pattern = "src/generated/**"
action = "deny"

[[permissions.rules]]
tool = "WebFetch"
pattern = "docs.rs"
action = "allow"           # 域名匹配
```

- 规则形式:`(tool, pattern) → allow|ask|deny`;deny 优先于 ask 优先于 allow,同优先级内**更具体的 pattern 优先**(借鉴 Claude Code,其行为已被用户广泛理解)。
- Bash pattern 对**规范化命令**匹配:shell 词法分析后按 `argv[0] + 前缀参数` 归一(如 `sudo cargo test` → `cargo test` 仍需单独规则;`A && B` 拆分为多条,全部 allow 才放行)。
- 审批 UI 给出 `pattern_suggestion`(如 `Bash(cargo test *)`),用户可一键把本次批准固化为项目规则。

### 会话决策

`ApproveForSession` 在内存累积 `(tool, normalized_pattern)`;写入日志(`LogEvent::PermissionGrant`),resume 时重放恢复。`--forget-permissions` 可清零。

## 2. 审批流程(engine 视角)

```
Tool::call 之前:
  verdict = engine.decide(tool, args, mode, rules, session_decisions)
  match verdict:
    Allow → 直接执行(记录 DecisionAllow 日志)
    Deny  → ToolError::Deny(记录;模型看到"被权限策略拒绝")
    Ask   → 逃逸分析(§3) → emit ApprovalRequest → await ApprovalResponse
             Approve/ApproveForSession → 执行
             Deny  → ToolError::Reject(模型看到"用户拒绝",通常换方案)
             Abort → 中断整个 turn
```

- 审批是 **core 内的 await point**,不占线程(task 内挂起)。
- TUI 审批弹窗显示:命令全文/文件 diff、命中规则、建议 allow pattern、模式切换提示。
- headless(`tao exec`)模式:`Ask` 按 config `exec.on_ask = deny|approve` 处理(默认 deny),保证脚本可预期。

## 3. 逃逸分析(v1 边界,诚实文档化)

规则引擎不可能完全理解 shell。v1 策略:

- 命令**显式不经 shell** 执行(`argv` 数组);需要管道/重定向时模型必须写 `bash -lc "..."`。
- 对 `bash -lc` 的命令串做**静态拆分**:`&&`/`||`/`;`/管道/子shell `$(...)`/反引号 逐段提取,每一段独立过规则;**任何一段无法静态解析 ⇒ 整条升级为 Ask**,并标注"包含不可分析结构"。
- 已知危险包装(`sudo`、`env`、`xargs`、`find -exec`、git 的 `-c`/alias)一律升级为 Ask。
- 文档明确:v1 的逃逸分析是**减少打扰**,不是安全边界;真正的安全边界是 M5 的 OS 沙箱。

## 4. 未来:OS 沙箱(M5,`tao-sandbox`)

`Sandbox` trait 已预留:

```rust
trait Sandbox {
    fn wrap(&self, cmd: CommandSpec, policy: &FsPolicy) -> Result<CommandSpec>;
}
struct FsPolicy { readable: Vec<PathBuf>, writable: Vec<PathBuf>, network: NetworkPolicy }
```

- macOS:`sandbox-exec` + **结构化 profile 构建器**(不做字符串模板拼接——codex 的 seatbelt.rs 是易碎点)。
- Linux:Landlock(fs)+ 网络命名空间/proxy;seccomp 仅作为 exec 过滤的补充。
- 语义:`SandboxPolicy`(能做到什么)× `AskForApproval`(什么时候问)正交——问得少不等于权限大。
- 接入点后,审批矩阵变为:`判定 = sandbox.deny ? Deny : engine.decide(...)`。

## 5. 审计

每一次判定写 `LogEvent::PermissionDecision { tool, pattern_matched, verdict, source }`。`tao sessions audit <id>` 可输出完整权限轨迹——这是"bypass 模式也能放心给团队用"的前提。
