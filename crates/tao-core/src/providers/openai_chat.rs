//! OpenAI Chat Completions codec(也覆盖 DeepSeek/Qwen/Kimi/OpenRouter 等兼容生态)。
//! 见 docs/design/providers.md §3.3。
//!
//! 端点:`POST {base}/v1/chat/completions`
//! SSE:`chat.completion.chunk`,delta 含 `content` / `tool_calls[i]` / `reasoning_content`。
//! usage 需 `stream_options: {include_usage: true}`,且通常在最后一个 chunk(空 delta)里。

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Value, json};
use tao_protocol::content::{StopReason, TokenUsage};
use tao_protocol::ids::CallId;
use tokio_util::sync::CancellationToken;

use crate::model::{
    CacheBreakpoint, ModelContent, ModelError, ModelMessage, ModelRequest, ModelStreamEvent,
    SystemBlock,
};
use crate::providers::ModelClient;
use crate::providers::common::{HttpSseClient, SseEvent};

pub struct OpenAiChatClient {
    http: HttpSseClient,
    base_url: String,
    auth_header: reqwest::header::HeaderValue,
}

impl OpenAiChatClient {
    /// `Authorization: Bearer <token>`(OpenAI 及大多数兼容生态)。
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let key = api_key.into();
        Self {
            http: HttpSseClient::default(),
            base_url: base_url.into(),
            auth_header: reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
        }
    }

    pub fn with_http(mut self, http: HttpSseClient) -> Self {
        self.http = http;
        self
    }
}

#[async_trait]
impl ModelClient for OpenAiChatClient {
    async fn stream(
        &self,
        req: &ModelRequest,
        cancel: &CancellationToken,
    ) -> Result<futures::stream::BoxStream<'static, Result<ModelStreamEvent, ModelError>>, ModelError>
    {
        let body = encode_request(req)?;
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );
        let auth_header = self.auth_header.clone();
        let stream = self
            .http
            .sse_stream(
                move |client: &Client| {
                    client
                        .request(Method::POST, &url)
                        .bearer_auth_header(&auth_header)
                        .json(&body)
                },
                cancel,
            )
            .await?;

        Ok(OpenAiChatDecoder::new().decode(stream).boxed())
    }
}

// ---- 请求编码 ----

fn encode_request(req: &ModelRequest) -> Result<Value, ModelError> {
    let mut messages: Vec<Value> = Vec::new();

    // system blocks 合并为一条 system 消息(chat 协议无独立 system 字段)。
    if !req.system.is_empty() {
        let text: String = req
            .system
            .iter()
            .map(|b| {
                let _ = b.cache_breakpoint; // chat 协议自动前缀缓存,无需显式 breakpoint
                b.text.clone()
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        messages.push(json!({ "role": "system", "content": text }));
    }

    for m in &req.messages {
        messages.push(encode_message(m));
    }

    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.schema } }))
        .collect();

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": req.max_output_tokens,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    // chat 协议的 reasoning_effort(OpenAI o 系列 / DeepSeek 等)。
    if let Some(eff) = req.reasoning {
        body["reasoning_effort"] = json!(match eff {
            tao_protocol::content::ReasoningEffort::Minimal => "minimal",
            tao_protocol::content::ReasoningEffort::Low => "low",
            tao_protocol::content::ReasoningEffort::Medium => "medium",
            tao_protocol::content::ReasoningEffort::High => "high",
            tao_protocol::content::ReasoningEffort::Max => "max",
        });
    }
    Ok(body)
}

fn encode_message(m: &ModelMessage) -> Value {
    match m {
        ModelMessage::User { content } => {
            let parts: Vec<Value> = content.iter().map(encode_user_part).collect();
            json!({ "role": "user", "content": parts })
        }
        ModelMessage::Assistant { content } => {
            // chat 协议:assistant 消息有 content + tool_calls 两字段。
            let mut text = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for c in content {
                match c {
                    ModelContent::Text(t) => text.push_str(t),
                    ModelContent::Thinking { .. } => {} // chat 协议无法回传 thinking,丢弃
                    ModelContent::ToolUse {
                        call_id,
                        name,
                        input,
                    } => {
                        tool_calls.push(json!({
                            "id": call_id.as_ref(),
                            "type": "function",
                            "function": { "name": name, "arguments": input_to_string(input) },
                        }));
                    }
                    ModelContent::Image { .. } => {} // assistant 一般不发图
                }
            }
            let mut v = json!({ "role": "assistant" });
            if !text.is_empty() {
                v["content"] = json!(text);
            } else {
                v["content"] = Value::Null;
            }
            if !tool_calls.is_empty() {
                v["tool_calls"] = json!(tool_calls);
            }
            v
        }
        ModelMessage::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            let text: String = content
                .iter()
                .map(|c| match c {
                    ModelContent::Text(t) => t.clone(),
                    _ => format!("[unsupported content: {:?}]", c),
                })
                .collect();
            // chat 协议没有 is_error 字段;在文本前缀标注。
            let body = if *is_error {
                format!("[ERROR] {text}")
            } else {
                text
            };
            json!({ "role": "tool", "tool_call_id": call_id.as_ref(), "content": body })
        }
    }
}

fn encode_user_part(c: &ModelContent) -> Value {
    match c {
        ModelContent::Text(t) => json!({ "type": "text", "text": t }),
        ModelContent::Image { mime, data_base64 } => json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", mime, data_base64) }
        }),
        _ => json!({ "type": "text", "text": "[unsupported content]" }),
    }
}

fn input_to_string(v: &Value) -> String {
    // OpenAI 要求 function.arguments 是字符串(JSON 字符串)。
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---- 响应解码 ----

struct OpenAiChatDecoder {
    /// index → call_id:chat 协议按 tool_calls[i].index 累积,首块带 id 与 name。
    tool_calls: std::collections::HashMap<u32, CallId>,
    /// 已发出 ToolUseBegin 的 call_id 集合,避免重复发。
    begun: std::collections::HashSet<CallId>,
    /// 累积 stop_reason:chat 协议 finish_reason 在非空 chunk,usage 在末尾空 chunk。
    pending_stop: Option<StopReason>,
}

impl OpenAiChatDecoder {
    fn new() -> Self {
        Self {
            tool_calls: Default::default(),
            begun: Default::default(),
            pending_stop: None,
        }
    }

    fn decode(
        mut self,
        mut stream: futures::stream::BoxStream<'static, Result<SseEvent, ModelError>>,
    ) -> futures::stream::BoxStream<'static, Result<ModelStreamEvent, ModelError>> {
        async_stream::try_stream! {
            while let Some(ev) = stream.next().await {
                let ev = ev?;
                for item in self.handle_event(&ev)? {
                    yield item;
                }
            }
        }
        .boxed()
    }

    fn handle_event(&mut self, ev: &SseEvent) -> Result<Vec<ModelStreamEvent>, ModelError> {
        let data = if ev.data.is_empty() || ev.data == "[DONE]" {
            return Ok(vec![]);
        } else {
            serde_json::from_str::<Value>(&ev.data)
                .map_err(|e| ModelError::Stream(format!("解析 chat chunk 失败: {e}")))?
        };

        let mut out = Vec::new();
        let choices = data.get("choices").and_then(|v| v.as_array());
        if let Some(choices) = choices
            && let Some(choice) = choices.first()
        {
            let delta = choice.get("delta").cloned().unwrap_or(json!({}));

            // 文本
            if let Some(text) = delta.get("content").and_then(|v| v.as_str())
                && !text.is_empty()
            {
                out.push(ModelStreamEvent::TextDelta(text.to_owned()));
            }

            // 思考流(DeepSeek reasoning_content / 部分 OpenAI 兼容实现)
            if let Some(r) = delta.get("reasoning_content").and_then(|v| v.as_str())
                && !r.is_empty()
            {
                out.push(ModelStreamEvent::ThinkingDelta(r.to_owned()));
            }

            // 工具调用:按 index 累积
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tool_calls {
                    let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    let function = tc.get("function").cloned().unwrap_or(json!({}));

                    // 首块:带 id 与 name → ToolUseBegin
                    let call_id = if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        let id = CallId::new(id);
                        self.tool_calls.insert(index, id.clone());
                        if !self.begun.contains(&id) {
                            let name = function
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_owned();
                            self.begun.insert(id.clone());
                            out.push(ModelStreamEvent::ToolUseBegin {
                                call_id: id.clone(),
                                name,
                            });
                        }
                        id
                    } else {
                        self.tool_calls
                            .get(&index)
                            .cloned()
                            .unwrap_or_else(|| CallId::new("c-?"))
                    };

                    // arguments 片段
                    if let Some(frag) = function.get("arguments").and_then(|v| v.as_str())
                        && !frag.is_empty()
                    {
                        out.push(ModelStreamEvent::ToolUseInputDelta {
                            call_id,
                            json_fragment: frag.to_owned(),
                        });
                    }
                }
            }

            // finish_reason
            if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                self.pending_stop = Some(map_stop_reason(reason));
            }
        }

        // usage(末尾空 chunk)
        if let Some(usage) = data.get("usage") {
            let stop = self.pending_stop.take().unwrap_or(StopReason::EndTurn);
            out.push(ModelStreamEvent::MessageEnd {
                stop_reason: stop,
                usage: map_usage(usage),
            });

            // 发出所有已 begin 但未 end 的 ToolUseEnd(chat 协议无显式 end)。
            let ids: Vec<CallId> = self.tool_calls.values().cloned().collect();
            for id in ids {
                out.push(ModelStreamEvent::ToolUseEnd { call_id: id });
            }
            self.tool_calls.clear();
            self.begun.clear();
        }

        Ok(out)
    }
}

fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" => StopReason::Refused,
        _ => StopReason::EndTurn,
    }
}

fn map_usage(u: &Value) -> TokenUsage {
    TokenUsage {
        input: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cached_input: u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output: u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        reasoning: 0,
    }
}

// helper trait:让 RequestBuilder 接受 HeaderValue 形式的 bearer
trait BearerAuthExt {
    fn bearer_auth_header(self, header: &reqwest::header::HeaderValue) -> Self;
}

impl BearerAuthExt for RequestBuilder {
    fn bearer_auth_header(mut self, header: &reqwest::header::HeaderValue) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, header.clone());
        self = self.headers(headers);
        self
    }
}

// 静默 unused:CacheBreakpoint/SystemBlock 在 chat 编码里被引用(即便不用 breakpoint)
#[allow(unused)]
fn _silence_unused() {
    let _ = CacheBreakpoint::Ephemeral;
    let _: Option<SystemBlock> = None;
}
