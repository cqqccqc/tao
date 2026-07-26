//! 规范模型内容块:wire 无关的会话内容表示(见 docs/design/providers.md §2)。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::CallId;

/// 规范化推理强度。各 provider codec 映射到自家字段(thinking budget / reasoning effort)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    Max,
}

/// 消息内容块。provider 特性(thinking / 图片)是一等字段,不被抹平。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        /// Anthropic 思考块回传签名;其他 provider 为 None。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolUse {
        call_id: CallId,
        name: String,
        input: Value,
    },
    Image {
        mime: String,
        /// base64 编码的图像数据。
        data_base64: String,
    },
}

impl Content {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }
}

/// 模型停止原因(规范化)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Refused,
    Interrupted,
    Error,
    /// 达到 max_turn_steps(agent loop 侧终止)。
    MaxSteps,
}

/// 归一化 token 用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input + self.output
    }
}

/// 计划项(Plan 工具 / ACP plan 共用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub content: String,
    pub status: PlanItemStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatus {
    Pending,
    InProgress,
    Completed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_tagged_lowercase() {
        let c = Content::Thinking {
            text: "hmm".into(),
            signature: None,
        };
        let s = serde_json::to_string(&c).unwrap();
        assert!(s.contains("\"type\":\"thinking\""), "got: {s}");
        assert!(!s.contains("signature"), "None 应跳过序列化: {s}");
    }

    #[test]
    fn reasoning_effort_kebab() {
        let s = serde_json::to_string(&ReasoningEffort::Minimal).unwrap();
        assert_eq!(s, "\"minimal\"");
    }

    #[test]
    fn usage_total() {
        let u = TokenUsage {
            input: 10,
            cached_input: 4,
            output: 5,
            reasoning: 2,
        };
        assert_eq!(u.total(), 15);
    }
}
