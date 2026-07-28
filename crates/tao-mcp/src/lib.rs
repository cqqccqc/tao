//! # tao-mcp
//!
//! MCP 客户端管理(自实现 JSON-RPC over stdio;HTTP 留后续)与 `tao mcp-serve`(M4)。

pub mod client;

pub use client::{McpClient, McpTool, McpToolInfo, McpToolResult, load_mcp_tools};

/// `tao mcp-serve` 入口(M4 实现)。
pub async fn run_server() -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "tao mcp-serve 尚未实现(M4),见 docs/design/tools.md §5"
    ))
}
