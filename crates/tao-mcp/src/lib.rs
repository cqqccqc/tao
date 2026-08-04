//! # tao-mcp
//!
//! MCP 客户端管理(自实现 JSON-RPC over stdio;HTTP 留后续)与 `tao mcp-serve`(M4)。

pub mod client;
pub mod server;

pub use client::{McpCallTool, McpClient, McpTool, McpToolInfo, McpToolResult, load_mcp_tools};
pub use server::run_server;
