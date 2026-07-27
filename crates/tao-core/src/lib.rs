//! # tao-core
//!
//! 通用 agent harness(见 docs/design/architecture.md)。
//! M1 起依次落地:providers(模型 codec)→ tools → turn loop → 权限 → 日志。

pub mod agent;
pub mod config;
pub mod model;
pub mod providers;
pub mod session;
pub mod tools;

pub use agent::{Agent, AgentHandle, SessionConfig};
pub use config::{
    AnthropicAuth, CliOverride, Config, LoadOpts, ModelProviderConfig, PartialConfig,
    SessionsConfig, WireApi,
};
pub use model::{ModelError, ModelRequest, ModelStreamEvent};
pub use providers::ModelClient;
pub use session::{TurnConfig, TurnEvent, TurnResult, run_turn};
pub use tools::{Tool, ToolCtx, ToolError, ToolOutput, ToolRegistry};
