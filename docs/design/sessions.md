# 会话、持久化与协作

## 1. 事件溯源:日志即真相

每个会话一个 append-only JSONL 文件:

```
~/.tao/projects/<cwd-slug>/sessions/<ts>-<uuid>.jsonl
~/.tao/projects/<cwd-slug>/index.redb     # 派生索引(可重建)
```

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogEvent {
    SessionMeta { id, parent: Option<SessionId>, cwd, git_head, config_fingerprint, created_at },
    SessionTitle { title: String },      // 会话标题:small_model 首轮后自动生成,/rename 可改
    UserInput { content: Vec<Content>, turn_id },
    AssistantMessage { content: Vec<Content>, usage: TokenUsage, turn_id },
    ToolCall { call_id, tool, args },                  // 决策前记录意图
    ToolResult { call_id, output, duration_ms },
    Approval { call_id, verdict: ReviewDecision, rule_suggestion: Option<String> },
    PermissionGrant { tool, pattern },                 // 会话级授权(ApproveForSession)
    PermissionDecision { tool, matched_rule, verdict },
    Compaction { summary_message, covers_through_seq }, // 摘要 + 覆盖范围
    Checkpoint { checkpoint_id, shadow_commit },
    ModeChange { mode: PermissionMode },
    TurnBoundary { turn_id, stop_reason },
    Error { message, retryable },
}
```

- 每条带 `seq`(单调)与 `ts`;写策略:每条 append + 每 1s 或关键事件后 fsync(崩溃最多丢 1s 的 delta,不丢语义状态)。
- **fold 语义**:内存 `SessionState = fold(LogEvent*)`;任何事件不可变,compaction 是"新事件改变投影"而非修改历史。
- `index.redb`:session 列表/搜索的派生索引(标题、首条消息、更新时间、大小);损坏或版本不符时扫描 JSONL 重建。

## 2. resume / fork / rewind

| 操作 | 机制 |
|---|---|
| `tao resume [id\|--last]` | 重放日志到最新状态(compaction 后的投影),继续对话 |
| fork | 重放到指定 `seq`,写新 session 文件(`parent` 指向原 id),后续事件独立 append |
| rewind(对话回退) |  fork 的特例:从某个 TurnBoundary 之前的 seq 分叉,UI 提供 `/rewind` |
| 跨 cwd | 拒绝默认 resume(`SessionMeta.cwd` 不匹配),`--force` 可覆盖并记录 |

`parent` 链让 fork 树可浏览(TUI `/sessions` 展示树形)。

## 3. checkpoint(文件回滚)

对话回退只能回退"说了什么",回退不了"文件被改了什么"。借鉴 gemini-cli 的 checkpoint 但更干净:

- **影子 git**:每个项目维护 `~/.tao/projects/<slug>/shadow.git`(bare),worktree 指向项目根。不触碰用户自己的 .git。
- 每个 turn 开始前的文件快照策略:对**将被 Patch/Edit/Write 触碰的文件**(patch 解析后可知路径)先 `git -C shadow add <paths> && commit` 生成 `shadow_commit`,写 `LogEvent::Checkpoint`。
- `/rollback <checkpoint>`:恢复文件到该快照 + fork 对话到对应 seq(两步原子地呈现为一个动作)。
- 隐私卫生:shadow repo 的 `.gitignore` 强制追加 `.env`、`*.key`、`secrets.*` 等模式;项目级可配置排除;**永不 push**,纯本地。
- 与 aider 式"每编辑即 commit 到用户 repo"的区别:不污染用户历史,回滚不影响用户已做的提交。

## 4. compaction(与日志的关系)

- 压缩事件 `Compaction { summary_message, covers_through_seq }`:重放时,`seq <= covers_through_seq` 的对话内容被摘要消息替代。
- 日志本体永远完整——share/审计用全量;模型上下文用投影。
- 自动触发:token 估算超 `auto_compact_at`;手动:`Op::Compact`(可带用户指令"重点保留 X")。

## 5. share(协作,M4+)

- 导出:`tao share <session>` 从日志生成**净化版** transcript(剥离子 agent 内部、按规则打码 secret 模式)→ gist/自托管 paste → 短链接。
- 链接可 view-only;协作续聊(基于 serve 模式的多客户端 attach)属 tao-web-ui 阶段能力。
- 打码规则与 checkpoint 的排除规则共用同一份 secret 模式表。

## 6. 生命周期卫生

- **并发写保护**:每个会话一把文件锁 `~/.tao/projects/<slug>/sessions/<id>.lock`(`fs2` flock)。同一会话同一时间只允许一个 writer;`tao serve` 常驻时各 tao 实例可读写**不同**会话,已锁会话在别处以只读 resume 打开(可查看,提交 turn 返回明确错误)。
- **rotate 续写链**:单文件超 `max_session_mb`(默认 50)时封存当前文件,新文件以 `parent` 指向它续写;resume 沿 parent 链回放整条链,对用户呈现为一个会话。
- 保留策略:`sessions.keep_days`(默认 30)与 `keep_count` 惰性清理;`tao sessions gc` 手动。
