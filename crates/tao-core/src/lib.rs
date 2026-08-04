//! # tao-core
//!
//! 通用 agent harness(见 docs/design/architecture.md)。
//! M1 起依次落地:providers(模型 codec)→ tools → turn loop → 权限 → 日志。

pub mod agents;
pub mod checkpoint;
pub mod commands;
pub mod compact;
pub mod config;
pub mod hooks;
pub mod instructions;
pub mod model;
pub mod permissions;
pub mod providers;
pub mod recorder;
pub mod replay;
pub mod session;
pub mod skills;
pub mod tools;

pub use agents::{SubagentDef, load_agents};
pub use checkpoint::ShadowRepo;
pub use commands::{Builtin, CommandDef, expand, load_commands, parse_builtin, split_name_args};
pub use compact::{DEFAULT_CONTEXT_WINDOW, DEFAULT_KEEP_LAST, approx_tokens, compact};
pub use config::{
    AnthropicAuth, CliOverride, Config, HooksConfig, LoadOpts, McpServerConfig, McpTransport,
    ModelProviderConfig, PartialConfig, SessionsConfig, WireApi,
};
pub use hooks::{HookConfig, HookCtx, HookEvent, HookOutcome, run_hooks};
pub use model::{ModelError, ModelRequest, ModelStreamEvent};
pub use permissions::{ApprovalRequest, Approver, PermissionEngine, PermissionKey};
pub use providers::ModelClient;
pub use recorder::{JsonlRecorder, NullRecorder, Recorder};
pub use replay::{SessionState, replay};
pub use session::{TurnConfig, TurnEvent, TurnResult, run_turn};
pub use skills::{SkillDef, load_skills, skills_prompt};
pub use tools::{Tool, ToolCtx, ToolError, ToolOutput, ToolRegistry};
