//! # tao-server
//!
//! Op/Event 的 wire 传输:`tao proto`(stdio JSONL,M1)与
//! `tao serve`(TCP/WS 多客户端,M4)。stderr 留日志,stdout 只走协议。

/// `tao proto` 入口(M1 实现)。
pub async fn run_proto() -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "tao proto 尚未实现(M1),见 docs/design/protocol.md §5"
    ))
}

/// `tao serve` 入口(M4 实现)。
pub async fn run_serve(_port: u16) -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "tao serve 尚未实现(M4),见 docs/design/protocol.md §5"
    ))
}
