# tao

用 Rust 构建的 coding agent:通用 agent harness 内核(`tao-core`)+ 多前端(`tao-tui` 优先,后续 `tao-web-ui` / `tao-gui`,并可通过 ACP 内嵌 Zed 等编辑器)。对标 Claude Code / OpenAI codex CLI / gemini-cli / opencode。

## 状态

早期设计阶段。完整技术方案见 **[docs/design/](docs/design/README.md)**:

- 总体架构、crate 布局、进程模型 → [architecture.md](docs/design/architecture.md)
- core ↔ 前端协议(Op/Event)→ [protocol.md](docs/design/protocol.md)
- agent 循环 / 模型提供方(Anthropic·OpenAI Responses·OpenAI Chat)→ [agent-loop.md](docs/design/agent-loop.md) [providers.md](docs/design/providers.md)
- 工具 / 权限 / 会话持久化 / 配置 / 扩展性 / ACP → [docs/design/](docs/design/README.md) 索引
- 路线图(M0–M6)→ [roadmap.md](docs/design/roadmap.md)

## License

MIT,见 [LICENSE](LICENSE)。
