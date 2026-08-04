//! Anthropic Messages API codec。
//! 见 docs/design/providers.md §3.1。
//!
//! 端点:`POST {base}/v1/messages`
//! SSE 事件:`message_start` / `content_block_start` / `content_block_delta` /
//! `content_block_stop` / `message_delta` / `message_stop` / `ping` / `error`。

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::{Value, json};
use tao_protocol::content::{ReasoningEffort, StopReason, TokenUsage};
use tao_protocol::ids::CallId;
use tokio_util::sync::CancellationToken;

use crate::model::{
    CacheBreakpoint, ModelContent, ModelError, ModelMessage, ModelRequest, ModelStreamEvent,
    RequestMeta, ToolSpec,
};
use crate::providers::ModelClient;
use crate::providers::common::{HttpSseClient, SseEvent};

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicClient {
    http: HttpSseClient,
    base_url: String,
    /// `Authorization: Bearer ...` 或 `x-api-key: ...`(根据 auth 类型选择)。
    auth_header: reqwest::header::HeaderValue,
    auth_kind: AuthKind,
}

#[derive(Clone, Copy)]
enum AuthKind {
    ApiKey,
    Bearer,
}

impl AnthropicClient {
    /// 用 Anthropic API key(`x-api-key` 头)。
    pub fn with_api_key(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let key = api_key.into();
        Self {
            http: HttpSseClient::default(),
            base_url: base_url.into(),
            auth_header: reqwest::header::HeaderValue::from_str(&key)
                .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
            auth_kind: AuthKind::ApiKey,
        }
    }

    /// 用 OAuth bearer token(`Authorization: Bearer ...`)。
    pub fn with_bearer(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let token = token.into();
        Self {
            http: HttpSseClient::default(),
            base_url: base_url.into(),
            auth_header: reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
            auth_kind: AuthKind::Bearer,
        }
    }

    pub fn with_http(mut self, http: HttpSseClient) -> Self {
        self.http = http;
        self
    }
}

#[async_trait]
impl ModelClient for AnthropicClient {
    /// 按 model 名返回上下文窗口:glm-* → 128k(meituan 等),claude-* / 其余 → 200k。
    fn context_window(&self, model: &str) -> u64 {
        if model.contains("glm") {
            128_000
        } else {
            200_000
        }
    }

    async fn stream(
        &self,
        req: &ModelRequest,
        cancel: &CancellationToken,
    ) -> Result<futures::stream::BoxStream<'static, Result<ModelStreamEvent, ModelError>>, ModelError>
    {
        let body = encode_request(req)?;
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let auth_header = self.auth_header.clone();
        let auth_kind = self.auth_kind;
        let stream = self
            .http
            .sse_stream(
                move |client: &Client| build_request(client, &url, &body, auth_kind, &auth_header),
                cancel,
            )
            .await?;

        let decoder = AnthropicDecoder::new();
        Ok(decoder.decode(stream).boxed())
    }
}

fn build_request(
    client: &Client,
    url: &str,
    body: &Value,
    auth_kind: AuthKind,
    auth_header: &reqwest::header::HeaderValue,
) -> RequestBuilder {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-version",
        reqwest::header::HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    match auth_kind {
        AuthKind::ApiKey => {
            headers.insert("x-api-key", auth_header.clone());
        }
        AuthKind::Bearer => {
            headers.insert("authorization", auth_header.clone());
        }
    }
    client
        .request(Method::POST, url)
        .headers(headers)
        .json(body)
}

// ---- 请求编码 ----

fn encode_request(req: &ModelRequest) -> Result<Value, ModelError> {
    let system: Vec<Value> = req
        .system
        .iter()
        .map(|b| {
            let mut v = json!({ "type": "text", "text": b.text });
            if let Some(bp) = b.cache_breakpoint {
                v["cache_control"] = match bp {
                    CacheBreakpoint::Ephemeral => json!({ "type": "ephemeral" }),
                    CacheBreakpoint::OneHour => json!({ "type": "ephemeral", "ttl": "1h" }),
                };
            }
            v
        })
        .collect();

    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(encode_message)
        .collect::<Result<_, _>>()?;

    let tools: Vec<Value> = req.tools.iter().map(encode_tool).collect();

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "max_tokens": req.max_output_tokens,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if let Some(eff) = req.reasoning {
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": thinking_budget(eff) });
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(meta) = meta_json(&req.metadata) {
        body["metadata"] = meta;
    }
    Ok(body)
}

fn encode_message(m: &ModelMessage) -> Result<Value, ModelError> {
    Ok(match m {
        ModelMessage::User { content } => {
            json!({ "role": "user", "content": content.iter().map(encode_user_content).collect::<Vec<_>>() })
        }
        ModelMessage::Assistant { content } => {
            json!({ "role": "assistant", "content": content.iter().map(encode_asst_content).collect::<Vec<_>>() })
        }
        ModelMessage::ToolResult {
            call_id,
            content,
            is_error,
        } => json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": call_id.as_ref(),
                "content": content.iter().map(encode_user_content).collect::<Vec<_>>(),
                "is_error": is_error,
            }]
        }),
    })
}

fn encode_user_content(c: &ModelContent) -> Value {
    match c {
        ModelContent::Text(t) => json!({ "type": "text", "text": t }),
        ModelContent::Image { mime, data_base64 } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": mime, "data": data_base64 }
        }),
        _ => json!({ "type": "text", "text": "[unsupported content]" }),
    }
}

fn encode_asst_content(c: &ModelContent) -> Value {
    match c {
        ModelContent::Text(t) => json!({ "type": "text", "text": t }),
        ModelContent::Thinking { text, signature } => {
            let mut v = json!({ "type": "thinking", "thinking": text });
            if let Some(sig) = signature {
                v["signature"] = json!(sig);
            }
            v
        }
        ModelContent::ToolUse {
            call_id,
            name,
            input,
        } => json!({
            "type": "tool_use",
            "id": call_id.as_ref(),
            "name": name,
            "input": input,
        }),
        _ => json!({ "type": "text", "text": "[unsupported content]" }),
    }
}

fn encode_tool(t: &ToolSpec) -> Value {
    json!({ "name": t.name, "description": t.description, "input_schema": t.schema })
}

fn thinking_budget(eff: ReasoningEffort) -> u32 {
    match eff {
        ReasoningEffort::Minimal => 1024,
        ReasoningEffort::Low => 4096,
        ReasoningEffort::Medium => 8192,
        ReasoningEffort::High => 16384,
        ReasoningEffort::Max => 32768,
    }
}

fn meta_json(m: &RequestMeta) -> Option<Value> {
    if m.session_id.is_none() && m.turn_id.is_none() {
        return None;
    }
    Some(json!({ "user_id": m.session_id.as_deref().unwrap_or("tao") }))
}

// ---- 响应解码 ----

struct AnthropicDecoder {
    /// call_id → (index,) 用于把 delta 路由到正确的 tool_use。
    blocks: std::collections::HashMap<u32, CallId>,
}

impl AnthropicDecoder {
    fn new() -> Self {
        Self {
            blocks: Default::default(),
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
        let event_type = ev.event.as_deref().unwrap_or("message");
        let data: Value = if ev.data.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&ev.data)
                .map_err(|e| ModelError::Stream(format!("解析 {} 数据失败: {e}", event_type)))?
        };

        let mut out = Vec::new();
        match event_type {
            "message_start" => {
                // usage 中的 input_tokens 在 message_delta 时更新,这里不产出。
            }
            "content_block_start" => {
                let index = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let block = data.get("content_block").cloned().unwrap_or(json!({}));
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("tool_use") => {
                        let call_id =
                            CallId::new(block.get("id").and_then(|v| v.as_str()).unwrap_or("c-?"));
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        self.blocks.insert(index, call_id.clone());
                        out.push(ModelStreamEvent::ToolUseBegin { call_id, name });
                    }
                    Some("thinking") => {
                        // 块开始时无 payload,后续 thinking_delta 推。
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let delta = data.get("delta").cloned().unwrap_or(json!({}));
                match delta.get("type").and_then(|v| v.as_str()) {
                    Some("text_delta") => {
                        let t = delta
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        out.push(ModelStreamEvent::TextDelta(t));
                    }
                    Some("thinking_delta") => {
                        let t = delta
                            .get("thinking")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        out.push(ModelStreamEvent::ThinkingDelta(t));
                    }
                    Some("input_json_delta") => {
                        if let Some(call_id) = self.blocks.get(&index).cloned() {
                            let frag = delta
                                .get("partial_json")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_owned();
                            out.push(ModelStreamEvent::ToolUseInputDelta {
                                call_id,
                                json_fragment: frag,
                            });
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if let Some(call_id) = self.blocks.remove(&index) {
                    out.push(ModelStreamEvent::ToolUseEnd { call_id });
                }
            }
            "message_delta" => {
                let delta = data.get("delta").cloned().unwrap_or(json!({}));
                let stop_reason = delta
                    .get("stop_reason")
                    .and_then(|v| v.as_str())
                    .map(map_stop_reason);
                let usage = data.get("usage").map(map_usage);
                if let (Some(sr), Some(u)) = (stop_reason, usage) {
                    out.push(ModelStreamEvent::MessageEnd {
                        stop_reason: sr,
                        usage: u,
                    });
                }
            }
            "message_stop" => {
                // message_delta 已产出 MessageEnd;若无,补一个默认。
            }
            "ping" => {}
            "error" => {
                let msg = data
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_owned();
                return Err(ModelError::Stream(format!("provider error: {msg}")));
            }
            _ => {
                // 未知事件忽略(向前兼容)。
            }
        }
        Ok(out)
    }
}

fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "stop_sequence" => StopReason::EndTurn,
        "refusal" => StopReason::Refused,
        _ => StopReason::EndTurn,
    }
}

fn map_usage(u: &Value) -> TokenUsage {
    TokenUsage {
        input: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cached_input: u
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        reasoning: 0,
    }
}
