//! 协议层错误。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("协议版本不兼容:对端 {peer},本端 {local}")]
    VersionMismatch { peer: u32, local: u32 },

    #[error("JSONL 编码/解码失败: {0}")]
    Codec(String),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
}
