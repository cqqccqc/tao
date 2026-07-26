//! # tao-acp
//!
//! ACP 适配层:把 tao Op/Event 翻译成 ACP JSON-RPC,被 Zed 等编辑器以 stdio 拉起。
//! M4 实现(依赖 agent_client_protocol crate),见 docs/design/acp.md。

/// `tao acp` 入口(M4 实现)。
pub async fn run() -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "tao acp 尚未实现(M4),见 docs/design/acp.md"
    ))
}
