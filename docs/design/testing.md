# 测试与质量策略

## 1. 测试金字塔

```
        ┌────────────┐
        │ E2E(assert_cmd + 脚本化 MockModel)│  少而精:装出来的二进制真跑
        ├──────────────┤
        │ 集成(core 公共 API:会话/权限/日志)  │  主力之一
        ├──────────────┤
        │ 组件(codec/SSE/补丁引擎/规则引擎)    │  主力之二
        ├──────────────┤
        │ 单元 + 性质(proptest)             │  大量、快
        └──────────────┘
```

## 2. MockModel:agent 测试的基石

`tao-core::testutil::MockModel`(仅 `#[cfg(any(test, feature = "testutil"))]`):

```rust
let model = MockModel::from_script(vec![
    // 第一轮:模型要求跑 cargo test
    Turn::tool_call("Bash", json!({"command": ["cargo","test"]})),
    // 第二轮:看到结果后输出文本结束
    Turn::text("全部通过,无需修改。"),
]);
let (agent, _) = Agent::spawn(config.with_model(model)).await?;
```

- 脚本化 turn:文本/工具调用/错误注入(429、断流)/延迟。agent loop、权限、日志全部可脱离网络测试。
- **fixture 回放**:把真实会话日志作为输入重放,断言fold后状态与UI投影一致(会话系统 = 纯函数,天然可测)。
- LLM 评估(eval)是**独立 crate**(不进 CI 主流程):任务集 + MockModel 反向用(录真实模型行为成 fixture)。

## 3. 分层细则

### 单元 / 性质(proptest)
- partial JSON 累积器:任意分片切分 → 最终 parse 等价。
- patch 引擎 round-trip:apply 后 reverse diff 可还原;fuzz 输入不 panic。
- 规则引擎:deny>ask>allow、具体度优先的偏序成立;任意规则集下判定确定。
- SSE 解析器:任意字节流不 panic;事件边界正确。

### 组件
- **codec golden tests**:每个 wire 协议一组请求/响应快照(insta);SSE fixture 用 wiremock 按 chunk 回放,断言规范增量序列。
- **重试**:wiremock 注入 429/断流/慢响应,断言退避次数与最终事件。
- **exec**:并发取消无孤儿进程(检查进程组);输出截断边界;超时路径。
- **shadow git**:并行会话不串仓;隐私模式(`.env`)不进快照。

### 集成(core 公共 API)
- 完整 turn:MockModel + 真实工具在 tempdir 中跑 Edit/Patch/Bash,断言历史与日志序列。
- 审批全矩阵:4 模式 × 3 决策 × allow/ask/deny 规则 → 事件序列快照。
- resume/fork:崩溃点注入(在每个 LogEvent 后 kill)重启重放,状态等价。
- 协议契约:`proto` 模式 JSONL 进出快照;id echo、审批往返、乱序容错。

### E2E
- `assert_cmd` 起真实二进制:`tao exec` 在 fixture 仓库完成任务(MockModel 经环境变量注入);`tao proto` 管道交互;退出码与 stdout 协议断言。
- TUI:`ratatui::backend::TestBackend` + insta 帧快照(流式中间态、审批弹窗、diff 视图);输入注入走 crossterm 事件队列。

## 4. 性能回归

- 微基准(criterion,不进 PR 必跑):markdown 增量渲染、规则匹配、patch 应用、fold 10k 事件。
- 启动时间基准:`tao --version` 与空会话启动(阈值 80ms,CI 警告)。
- 内存:10k 行历史 fixture 的 RSS 抽样。

## 5. CI 拓扑

| 阶段 | 内容 | 触发 |
|---|---|---|
| check | fmt + clippy(deny warnings)+ doc | 每 PR |
| test | 单元+组件+集成(macos/linux) | 每 PR |
| e2e | 脚本化 e2e + TUI 快照 | 每 PR |
| contract | 真实 API 冒烟(需 secret,只读操作) | nightly |
| perf | criterion vs main(报告不 gate) | nightly |

MSRV:stable-2;`rust-version` 固定在 workspace。安全:`cargo-deny`(license/advisory)+ `cargo audit` nightly。

## 6. 崩溃恢复与可观测(质量的一部分)

- panic = 进程级失败,但**会话日志已 fsync 的部分必须可恢复**:E2E 含 kill -9 注入。
- `tracing` 分层:`tao-core=debug` 等 env filter;日志写 `~/.tao/log/`(rolling),TUI 内 `/log` 打开;绝不写 stdout(stdio 协议模式)。
- 每次请求带 span 字段(provider/model/ttft/retries);`RUST_LOG` 之外提供 `TAO_LOG` 便捷开关。
