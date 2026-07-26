//! # tao-core
//!
//! 通用 agent harness(见 docs/design/architecture.md)。
//! M1 起依次落地:providers(模型 codec)→ tools → turn loop → 权限 → 日志。

pub mod agent;

pub use agent::{Agent, AgentHandle, SessionConfig};
