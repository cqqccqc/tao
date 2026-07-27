//! 规范模型格式:agent loop / 历史 / 日志共用的 provider 无关表示。
//! 见 docs/design/providers.md §2。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use tao_protocol::content::{ReasoningEffort, StopReason, TokenUsage};
use tao_protocol::ids::CallId;
/// 工具定义(wire 无关)。provider codec 翻译成各家的 tools 字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema,描述工具参数。
    pub schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemBlock {
    pub text: String,
    /// 在此块结尾打 prompt-cache breakpoint(支持 caching 的 provider)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_breakpoint: Option<CacheBreakpoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheBreakpoint {
    /// 默认 5 分钟 TTL。
    Ephemeral,
    /// 1 小时 TTL(provider 支持时)。
    OneHour,
}

/// 模型请求:agent loop 喂给 ModelClient 的输入。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    pub model: String,
    pub system: Vec<SystemBlock>,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<ToolSpec>,
    pub reasoning: Option<ReasoningEffort>,
    pub max_output_tokens: u32,
    pub temperature: Option<f32>,
    /// 会话标识(透传给 provider 作遥测/idiempotency,不参与消息构建)。
    pub metadata: RequestMeta,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RequestMeta {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelMessage {
    User {
        content: Vec<ModelContent>,
    },
    Assistant {
        content: Vec<ModelContent>,
    },
    ToolResult {
        call_id: CallId,
        content: Vec<ModelContent>,
        is_error: bool,
    },
}

/// 规范内容块(对应 tao_protocol::Content,但 model 侧独立以避免循环依赖)。
#[derive(Debug, Clone, PartialEq)]
pub enum ModelContent {
    Text(String),
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse {
        call_id: CallId,
        name: String,
        input: Value,
    },
    Image {
        mime: String,
        data_base64: String,
    },
}

impl ModelContent {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
}

/// 规范流式事件:provider codec 把 SSE 翻译成这个序列。
#[derive(Debug, Clone, PartialEq)]
pub enum ModelStreamEvent {
    /// 一条消息/工具块的开始(provider 给出 call_id 与 name)。
    ToolUseBegin {
        call_id: CallId,
        name: String,
    },
    /// 工具参数的 JSON 片段(累积式,见 docs/design/providers.md §3)。
    ToolUseInputDelta {
        call_id: CallId,
        json_fragment: String,
    },
    ToolUseEnd {
        call_id: CallId,
    },
    TextDelta(String),
    ThinkingDelta(String),
    /// 流结束:stop reason 与最终 usage。
    MessageEnd {
        stop_reason: StopReason,
        usage: TokenUsage,
    },
}

/// provider 的错误分级:agent loop 据此决定重试还是上抛。
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("可重试的传输/限流错误: {0}")]
    Retryable(String),
    #[error("认证失败: {0}")]
    Auth(String),
    #[error("上下文超长: {0}")]
    ContextLength(String),
    #[error("provider fatal: {0}")]
    Fatal(String),
    #[error("流解析失败: {0}")]
    Stream(String),
    #[error("请求构建失败: {0}")]
    Build(String),
}
