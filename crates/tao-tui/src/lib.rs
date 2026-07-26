//! # tao-tui
//!
//! ratatui 终端界面:inline viewport(非 alternate screen),见 docs/design/tui.md。
//! M1 实现最小可用(输入 + 流式文本),M2 完成渲染管线。

/// `tao tui`(默认子命令)入口(M1 实现)。
pub async fn run() -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "tao-tui 尚未实现(M1),见 docs/design/tui.md"
    ))
}
