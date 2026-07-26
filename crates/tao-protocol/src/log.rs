//! LogEvent:append-only 会话事件日志(见 docs/design/sessions.md §1)。
//! fold(LogEvent*) = SessionState,日志即真相。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::{Content, StopReason, TokenUsage};
use crate::ids::{CallId, CheckpointId, SessionId, TurnId};
use crate::op::ReviewDecision;
use crate::permission::{Decision, PermissionMode};

/// 日志文件中的一行(JSONL)。`seq` 单调递增;写策略:每条 append + 关键事件 fsync。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    pub seq: u64,
    /// Unix epoch 毫秒。
    pub ts: u64,
    #[serde(flatten)]
    pub event: LogEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogEvent {
    SessionMeta {
        id: SessionId,
        /// fork 来源。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<SessionId>,
        cwd: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        git_head: Option<String>,
        /// 指令文件(TAO.md 等)hash 集,resume 时检测漂移。
        config_fingerprint: String,
        created_at_ms: u64,
    },
    /// 会话标题:small_model 首轮后自动生成,/rename 可改。
    SessionTitle {
        title: String,
    },

    UserInput {
        content: Vec<Content>,
        turn_id: TurnId,
    },
    AssistantMessage {
        content: Vec<Content>,
        usage: TokenUsage,
        turn_id: TurnId,
    },
    /// 决策前记录意图。
    ToolCall {
        call_id: CallId,
        tool: String,
        args: Value,
    },
    ToolResult {
        call_id: CallId,
        output: Value,
        duration_ms: u64,
    },

    // ---- 权限 ----
    Approval {
        call_id: CallId,
        verdict: ReviewDecision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule_suggestion: Option<String>,
    },
    /// 会话级授权(ApproveForSession),resume 时重放恢复。
    PermissionGrant {
        tool: String,
        pattern: String,
    },
    /// 每次判定的审计记录。
    PermissionDecision {
        tool: String,
        decision: Decision,
    },
    ModeChange {
        mode: PermissionMode,
    },

    // ---- 压缩与快照 ----
    /// 摘要 + 覆盖范围:重放时 seq <= covers_through_seq 的对话被摘要替代。
    Compaction {
        summary: Vec<Content>,
        covers_through_seq: u64,
    },
    Checkpoint {
        checkpoint_id: CheckpointId,
        shadow_commit: String,
    },

    TurnBoundary {
        turn_id: TurnId,
        stop_reason: StopReason,
    },
    Error {
        message: String,
        retryable: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_line_flatten_shape() {
        let line = LogLine {
            seq: 3,
            ts: 1_720_000_000_000,
            event: LogEvent::ModeChange {
                mode: PermissionMode::Plan,
            },
        };
        let s = serde_json::to_string(&line).unwrap();
        // flatten 后 seq/ts/type 同层,无嵌套 "event" 字段
        assert!(s.contains("\"seq\":3"), "got: {s}");
        assert!(s.contains("\"type\":\"mode_change\""), "got: {s}");
        assert!(!s.contains("\"event\""), "不应有嵌套 event: {s}");
        let back: LogLine = serde_json::from_str(&s).unwrap();
        assert_eq!(back, line);
    }
}
