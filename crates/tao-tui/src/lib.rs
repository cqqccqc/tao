//! # tao-tui
//!
//! ratatui 终端界面(M1 最小可用版本)。
//! 见 docs/design/tui.md。
//!
//! M1 范围:inline viewport + 单行输入框 + 流式文本 + 工具状态行。
//! M2 起:多行编辑器、markdown 渲染、审批弹窗、diff 视图。

mod app;
mod render;

pub use app::{run, run_with_load_opts};
