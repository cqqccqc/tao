//! # tao-acp
//!
//! ACP 适配层:JSON-RPC 2.0 over stdio(nd-json),被 Zed 等编辑器拉起。
//! v1 自实现 JSON-RPC(不引入 agent_client_protocol 重依赖)。见 docs/design/acp.md。

pub mod acp;

pub use acp::AcpServer;

/// `tao acp` 入口:启动 ACP server 主循环。
pub async fn run() -> anyhow::Result<()> {
    let server = AcpServer::new();
    server.run().await
}
