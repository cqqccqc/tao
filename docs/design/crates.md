# 附录:关键 crate 选型

调研结论(2026-07,基于 codex-rs 等生产项目的实际用法)。原则:**承重抽象自研(model client / patch 引擎),其余用成熟 crate**。

## 运行时与基础

| 用途 | 选型 | 备注 |
|---|---|---|
| 异步运行时 | `tokio` 1.x(`rt-multi-thread, macros, process, sync, time, signal, fs`) | 事实标准;避开 async-std(基本休眠) |
| HTTP | `reqwest`(rustls) | provider 与 WebFetch 共用 |
| SSE | `eventsource-stream` + `reqwest` | 自研 client 的基础;`openai-streams` 已陈旧 |
| 序列化 | `serde` + `serde_json` + `toml` | — |
| 错误 | `thiserror`(库)+ `color-eyre`(二进制) | 公共 API 不用 anyhow |
| 日志 | `tracing` + `tracing-subscriber`(env-filter)+ `tracing-appender` | 写文件,绝不污染 stdout(stdio 协议) |
| 取消 | `tokio-util::CancellationToken` | 统一中断语义 |

## agent 领域

| 用途 | 选型 | 备注 |
|---|---|---|
| MCP | `rmcp`(modelcontextprotocol/rust-sdk) | 官方维护;stdio + streamable HTTP;无替代品 |
| diff | `similar` | 补丁展示与校验;codex 同款 |
| 补丁 DSL | 自研(`tao-apply-patch`) | 语法/语义分离设计,见 tools.md §3 |
| AST 寻址 | `tree-sitter`(按需语言 grammar) | 仅用于 patch L2 锚定与大纲,不做全量索引 |
| token 计数 | `tiktoken-rs` | Anthropic 为近似值,预算留余量 |
| grep | 优先外部 `rg`,fallback `ignore` crate 遍历 | 与 ripgrep 行为对齐 |
| 模糊搜索 | `nucleo`(fuzzy,性能好于 skim 系) | @文件补全/命令面板 |
| git | `git2`(libgit2) | shadow repo 与状态;`spawn_blocking` 包同步 API;gix 备选但 API 仍在演进 |
| 临时文件 | `tempfile` | exec 输出落盘等 |

## TUI

| 用途 | 选型 | 备注 |
|---|---|---|
| 框架 | `ratatui` 0.29+ + `crossterm` | 标准组合 |
| 输入区 | `tui-textarea` | 多行、历史、可外接编辑器 |
| markdown | `tui-markdown`(pulldown-cmark AST → ratatui Text) | 增量渲染在其上自建行缓存 |
| 语法高亮 | `syntect` | 仅代码块;bat 同款资产;tree-sitter 高亮属于过度设计 |
| 宽字符 | `unicode-width` / `unicode-segmentation` / `textwrap` | CJK/emoji 正确性 |
| 终端图像 | `ratatui-image`(M6 可选) | v1 用占位卡片 |

## 配置与数据

| 用途 | 选型 | 备注 |
|---|---|---|
| 分层配置 | `figment` | 比 config-rs 维护活跃;支持 TOML/env/CLI 合并 |
| XDG 路径 | `directories`(不是 `dirs`) | — |
| 凭证 | `keyring` + `secrecy` | macOS Keychain / Secret Service / Windows;内存中密钥包裹 |
| OAuth | `oauth2` crate(PKCE/device)+ loopback `tiny_http` | provider 登录流程 |
| 会话索引 | `redb` | 派生索引,可重建;避免 `sled`(停更)/ `sqlx`(过重) |
| 文件监听 | `notify` | 后台"文件已被外部修改"事件 |

## 测试

| 用途 | 选型 | 备注 |
|---|---|---|
| HTTP mock | `wiremock` | SSE 按 chunk 回放 |
| 快照 | `insta` | TUI 帧、codec golden、协议契约 |
| 性质测试 | `proptest` | JSON 累积/patch round-trip/规则引擎 |
| CLI E2E | `assert_cmd` + `predicates` + `tempfile` | 真实二进制 |
| 基准 | `criterion` | nightly 跑,不 gate PR |

## 风险提示

- 非官方 `anthropic-sdk` 类 crate:无权威实现,API 漂移风险高 → 自研。
- `genai`/`rig`:覆盖面广但抽象厚重;provider 特性(cache_control、thinking、responses items)会被抹平 → 仅作参考实现阅读。
- `aichat`(sigoden)与 `codex-rs` 是最好的两份现成参考:provider 抽象与 SSE 处理看 aichat;整体 harness 结构看 codex。
