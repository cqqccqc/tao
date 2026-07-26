//! # tao-protocol
//!
//! tao agent 的线协议类型:core 与所有前端之间唯一的消息契约。
//!
//! - **Op / Event**:前端 ↔ core 的双向消息(见 `docs/design/protocol.md`)。
//! - **LogEvent**:append-only 会话事件日志(见 `docs/design/sessions.md`)。
//! - **Content / TokenUsage / StopReason**:规范模型格式(见 `docs/design/providers.md`)。
//! - **PermissionMode / PermissionRule / Decision**:权限模型(见 `docs/design/permissions.md`)。
//!
//! 纯 serde 类型,不依赖 tao-core;in-process(tokio mpsc)与
//! wire(stdio/socket JSONL)两种传输共用同一份类型。

pub mod content;
pub mod error;
pub mod event;
pub mod ids;
pub mod log;
pub mod op;
pub mod permission;
pub mod wire;

// ---- 信封 ----

use serde::{Deserialize, Serialize};

/// 前端 → core。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Submission {
    pub id: String,
    pub op: Op,
}

impl Submission {
    pub fn new(id: impl Into<String>, op: Op) -> Self {
        Self { id: id.into(), op }
    }
}

// ---- 常用再导出 ----

pub use content::{Content, PlanItem, PlanItemStatus, ReasoningEffort, StopReason, TokenUsage};
pub use error::ProtocolError;
pub use event::{
    ApprovalDetail, ApprovalKind, Event, EventMsg, ExecStream, McpServerHealth, McpServerStatus,
    McpToolInfo, ModelInfo, SessionSummary,
};
pub use ids::{CallId, CheckpointId, SessionId, TurnId};
pub use log::{LogEvent, LogLine};
pub use op::{Op, ReviewDecision, UserInput};
pub use permission::{
    Decision, PermissionMode, PermissionRule, RuleAction, RuleScope, Verdict, VerdictSource,
};
pub use wire::PROTOCOL_VERSION;
