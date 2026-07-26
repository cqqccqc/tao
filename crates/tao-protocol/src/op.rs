//! Op:前端 → core 的操作(见 docs/design/protocol.md §2)。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{CheckpointId, SessionId};
use crate::permission::{PermissionMode, PermissionRule, RuleScope};

/// 前端发给 core 的操作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Op {
    /// wire 模式首条消息:协议握手。
    Hello {
        protocol_version: u32,
    },

    // ---- 会话主流程 ----
    /// 发起一个 turn。id 由客户端生成(Submission.id),事件回显它。
    UserTurn {
        turn_id: String,
        input: Vec<UserInput>,
    },
    /// 中断当前 turn。abandon_queued 默认 true:同时丢弃排队的 turn。
    Interrupt {
        #[serde(default = "default_true")]
        abandon_queued: bool,
    },
    /// 主动压缩上下文(可带指令,如"重点保留 X")。
    Compact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instruction: Option<String>,
    },
    Shutdown,

    // ---- 审批应答 ----
    /// 对应 EventMsg::ApprovalRequest,call_id 必须精确回显。
    ApprovalResponse {
        call_id: String,
        decision: ReviewDecision,
    },

    // ---- 会话管理 ----
    ListSessions,
    ResumeSession {
        session_id: SessionId,
        fork: bool,
    },
    /// 回滚文件 + 对话到指定 checkpoint。
    CheckpointRollback {
        checkpoint_id: CheckpointId,
    },

    // ---- 模式与规则 ----
    SetPermissionMode {
        mode: PermissionMode,
    },
    /// 固化一条规则(scope 决定写内存还是 config 文件)。
    AddPermissionRule {
        rule: PermissionRule,
        scope: RuleScope,
    },
    /// 会话内切换模型(/model)。
    SetModel {
        model: String,
    },

    // ---- 查询(响应为 SessionQuery* 事件,id 回显配对)----
    GetHistory {
        after_seq: u64,
        limit: u32,
    },
    /// serve 模式重连补拉。
    ResumeEvents {
        after_seq: u64,
    },
    ListMcpTools,
    ListModels,
}

fn default_true() -> bool {
    true
}

/// 用户输入块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserInput {
    Text { text: String },
    Image { path: PathBuf },
}

/// 审批决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// 本次允许。
    Approve,
    /// 本次 + 同规则加入会话级 allow(写 LogEvent::PermissionGrant)。
    ApproveForSession,
    /// 拒绝该调用;模型收到拒绝结果后继续。
    Deny,
    /// 拒绝并中断整个 turn。
    Abort,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_tagged_snake_case() {
        let op = Op::SetPermissionMode {
            mode: PermissionMode::Plan,
        };
        let s = serde_json::to_string(&op).unwrap();
        assert!(s.contains("\"type\":\"set_permission_mode\""), "got: {s}");
    }

    #[test]
    fn interrupt_default_abandon_true() {
        let op: Op = serde_json::from_str("{\"type\":\"interrupt\"}").unwrap();
        assert_eq!(
            op,
            Op::Interrupt {
                abandon_queued: true
            }
        );
    }

    #[test]
    fn review_decision_roundtrip() {
        for d in [
            ReviewDecision::Approve,
            ReviewDecision::ApproveForSession,
            ReviewDecision::Deny,
            ReviewDecision::Abort,
        ] {
            let s = serde_json::to_string(&d).unwrap();
            let back: ReviewDecision = serde_json::from_str(&s).unwrap();
            assert_eq!(back, d);
        }
    }
}
