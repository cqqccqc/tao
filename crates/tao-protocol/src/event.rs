//! Event:core → 前端的事件(见 docs/design/protocol.md §3)。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::{PlanItem, StopReason, TokenUsage};
use crate::ids::{CallId, SessionId, TurnId};
use crate::permission::PermissionMode;

/// core 发给前端的事件。三个关联字段分工:
/// `id` 请求-应答配对;`seq` 排序与断点补拉;`turn` 事件归属。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// 回显触发它的 Submission.id(流事件回显 UserTurn 的 id)。
    pub id: String,
    /// 会话内单调递增。
    pub seq: u64,
    /// 所属 turn_id;与 turn 无关的事件为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnId>,
    pub msg: EventMsg,
}

impl Event {
    /// 构造一个事件(seq/turn 由 core 在发送前填充)。
    pub fn new(id: impl Into<String>, msg: EventMsg) -> Self {
        Self {
            id: id.into(),
            seq: 0,
            turn: None,
            msg,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMsg {
    // ---- 会话配置(握手后第一个事件)----
    SessionConfigured {
        protocol_version: u32,
        session_id: SessionId,
        model: String,
        permission_mode: PermissionMode,
        cwd: PathBuf,
    },

    // ---- turn 边界 ----
    TurnStarted {
        turn_id: TurnId,
    },
    TurnComplete {
        turn_id: TurnId,
        usage: TokenUsage,
        stop_reason: StopReason,
    },

    // ---- 助手流式输出 ----
    AgentMessageDelta {
        text: String,
    },
    /// 本条消息流结束(完整文本)。
    AgentMessage {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    Reasoning {
        text: String,
    },

    // ---- 内置 exec ----
    ExecCommandBegin {
        call_id: CallId,
        command: Vec<String>,
        cwd: PathBuf,
    },
    ExecCommandOutputDelta {
        call_id: CallId,
        stream: ExecStream,
        chunk: String,
    },
    ExecCommandEnd {
        call_id: CallId,
        exit_code: i32,
        duration_ms: u64,
        truncated: bool,
    },

    // ---- 补丁 ----
    PatchApplyBegin {
        call_id: CallId,
        files: Vec<PathBuf>,
    },
    PatchApplyEnd {
        call_id: CallId,
        success: bool,
        diff: String,
    },

    // ---- 通用工具边界(读/写/搜索/MCP/子 agent 等)----
    ToolCallBegin {
        call_id: CallId,
        tool: String,
        summary: String,
    },
    ToolCallEnd {
        call_id: CallId,
        ok: bool,
        summary: String,
    },

    // ---- 审批(见 protocol.md §4 时序)----
    ApprovalRequest {
        call_id: CallId,
        kind: ApprovalKind,
        detail: ApprovalDetail,
    },

    // ---- 计划 / 状态 ----
    PlanUpdated {
        items: Vec<PlanItem>,
    },
    TokenCount {
        used: u64,
        window: u64,
    },

    // ---- 后台 / 系统 ----
    BackgroundEvent {
        message: String,
    },
    /// 模式切换广播(多前端同步)。
    PermissionModeChanged {
        mode: PermissionMode,
    },
    Error {
        message: String,
        retryable: bool,
    },
    /// 单次流失败(可能自动重试)。
    StreamError {
        message: String,
    },

    // ---- 查询响应(对应 List*/Get* Op,id 回显配对)----
    SessionQueryHistory {
        events: Vec<Event>,
        done: bool,
    },
    SessionQuerySessions {
        sessions: Vec<SessionSummary>,
    },
    SessionQueryMcpTools {
        tools: Vec<McpToolInfo>,
        health: Vec<McpServerHealth>,
    },
    SessionQueryModels {
        models: Vec<ModelInfo>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Exec,
    Patch,
    Tool,
    McpTool,
}

/// 审批请求详情:展示命令全文 / 文件 diff、命中规则、建议 allow pattern。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalDetail {
    /// 命中的 ask 规则原文(便于用户理解为何被问)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_matched: Option<String>,
    /// kind = Exec 时的完整 argv。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// kind = Patch 时的涉及文件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<PathBuf>>,
    /// kind = Tool/McpTool 时的工具名 + 参数摘要。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_summary: Option<Value>,
    /// 建议的 allow 规则,如 "Bash(cargo test *)";审批 UI 可一键固化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern_suggestion: Option<String>,
}

// ---- 查询响应的载荷类型 ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    /// fork 树父节点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
    pub title: Option<String>,
    pub cwd: PathBuf,
    pub updated_at_ms: u64,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerHealth {
    pub server: String,
    pub status: McpServerStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerStatus {
    Connected,
    Connecting,
    Failed,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// "provider/model-id"。
    pub id: String,
    pub provider: String,
    pub display: String,
    pub context_window: u64,
    #[serde(default)]
    pub supports_thinking: bool,
    #[serde(default)]
    pub supports_images: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_new_defaults() {
        let e = Event::new("req-1", EventMsg::AgentMessageDelta { text: "hi".into() });
        assert_eq!(e.seq, 0);
        assert_eq!(e.turn, None);
    }

    #[test]
    fn event_msg_tagged() {
        let m = EventMsg::TurnStarted {
            turn_id: TurnId::new("t-9"),
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"type\":\"turn_started\""), "got: {s}");
    }

    #[test]
    fn optional_fields_skipped() {
        let d = ApprovalDetail {
            rule_matched: None,
            command: Some(vec!["cargo".into(), "test".into()]),
            files: None,
            tool: None,
            args_summary: None,
            pattern_suggestion: None,
        };
        let s = serde_json::to_string(&d).unwrap();
        assert!(s.contains("command"));
        assert!(!s.contains("rule_matched"), "None 字段应跳过: {s}");
    }
}
