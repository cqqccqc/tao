//! # tao-mcp
//!
//! MCP 客户端管理(基于 rmcp,stdio + streamable HTTP)与 `tao mcp-serve`。
//! M3 实现客户端;M4 实现 server 模式。

/// `tao mcp-serve` 入口(M4 实现)。
pub async fn run_server() -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "tao mcp-serve 尚未实现(M4),见 docs/design/tools.md §5"
    ))
}
