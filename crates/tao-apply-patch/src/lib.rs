//! # tao-apply-patch
//!
//! apply-patch DSL:语法/语义分离(见 docs/design/tools.md §3)。
//! M2 实现:解析层 → 两级寻址(文本 fuzz + tree-sitter AST)→ 事务性写盘。

use std::path::PathBuf;

/// 一个补丁块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub action: HunkAction,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkAction {
    Add,
    Update,
    Delete,
    Move { to: PathBuf },
}

/// 解析 patch 文本(M2 实现)。
pub fn parse(_input: &str) -> Result<Vec<Hunk>, ParseError> {
    Err(ParseError::Unimplemented)
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("apply-patch 解析器尚未实现(M2),见 docs/design/tools.md §3")]
    Unimplemented,
}
