//! OpenAI Responses API codec(OpenAI 首选)。
//! 见 docs/design/providers.md §3.2。
//!
//! 端点:`POST {base}/v1/responses`,`store: false`(core 自持历史)。
//! SSE:`response.output_item.added` / `response.output_item.done` /
//! `response.output_text.delta` / `response.function_call_arguments.delta` /
//! `response.function_call_arguments.done` / `response.reasoning_summary_text.delta` /
//! `response.completed`。

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::{Client, Method};
use serde_json::{Value, json};
use tao_protocol::content::{ReasoningEffort, StopReason, TokenUsage};
use tao_protocol::ids::CallId;
use tokio_util::sync::CancellationToken;

use crate::model::{ModelContent, ModelError, ModelMessage, ModelRequest, ModelStreamEvent};
use crate::providers::ModelClient;
use crate::providers::common::{HttpSseClient, SseEvent};

pub struct OpenAiResponsesClient {
    http: HttpSseClient,
    base_url: String,
    auth_header: reqwest::header::HeaderValue,
}

impl OpenAiResponsesClient {
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
impl ModelClient for OpenAiResponsesClient {
    async fn stream(
        &self,
        req: &ModelRequest,
        cancel: &CancellationToken,
    ) -> Result<futures::stream::BoxStream<'static, Result<ModelStreamEvent, ModelError>>, ModelError>
    {
        let body = encode_request(req)?;
        let url = format!("{}/v1/responses", self.base_url.trim_end_matches('/'));
        let auth_header = self.auth_header.clone();
        let stream = self
            .http
            .sse_stream(
                move |client: &Client| {
                    let mut headers = reqwest::header::HeaderMap::new();
                    headers.insert(reqwest::header::AUTHORIZATION, auth_header.clone());
                    client
                        .request(Method::POST, &url)
                        .headers(headers)
                        .json(&body)
                },
                cancel,
            )
            .await?;

        Ok(OpenAiResponsesDecoder::new().decode(stream).boxed())
    }
}

// ---- 请求编码 ----

fn encode_request(req: &ModelRequest) -> Result<Value, ModelError> {
    // system blocks → instructions 字段
    let instructions: Option<String> = if req.system.is_empty() {
        None
    } else {
        Some(
            req.system
                .iter()
                .map(|b| b.text.clone())
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    };

    // messages → input items
    let mut input: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m {
            ModelMessage::User { content } => {
                let parts: Vec<Value> = content.iter().map(encode_user_part).collect();
                input.push(json!({ "type": "message", "role": "user", "content": parts }));
            }
            ModelMessage::Assistant { content } => {
                for c in content {
                    match c {
                        ModelContent::Text(t) => {
                            input.push(json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": t }] }));
                        }
                        ModelContent::Thinking { text, signature: _ } => {
                            // Responses API 的 reasoning item(签名由 provider 内部管理,store:false 下无需回传)。
                            input.push(json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": text }] }));
                        }
                        ModelContent::ToolUse {
                            call_id,
                            name,
                            input: args,
                        } => {
                            input.push(json!({
                                "type": "function_call",
                                "call_id": call_id.as_ref(),
                                "name": name,
                                "arguments": input_to_string(args),
                            }));
                        }
                        ModelContent::Image { .. } => {} // assistant 一般不发图
                    }
                }
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
                        _ => "[unsupported content]".to_owned(),
                    })
                    .collect();
                let body = if *is_error {
                    format!("[ERROR] {text}")
                } else {
                    text
                };
                input.push(json!({ "type": "function_call_output", "call_id": call_id.as_ref(), "output": body }));
            }
        }
    }

    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.schema, "strict": false }))
        .collect();

    let mut body = json!({
        "model": req.model,
        "input": input,
        "stream": true,
        "store": false,
    });
    if let Some(instr) = instructions {
        body["instructions"] = json!(instr);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(eff) = req.reasoning {
        body["reasoning"] = json!({ "effort": match eff {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "high", // Responses API 当前最高 high
        }});
    }
    // max_output_tokens 是 Responses API 的字段名
    body["max_output_tokens"] = json!(req.max_output_tokens);

    Ok(body)
}

fn encode_user_part(c: &ModelContent) -> Value {
    match c {
        ModelContent::Text(t) => json!({ "type": "input_text", "text": t }),
        ModelContent::Image { mime, data_base64 } => json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{}", mime, data_base64)
        }),
        _ => json!({ "type": "input_text", "text": "[unsupported content]" }),
    }
}

fn input_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---- 响应解码 ----

struct OpenAiResponsesDecoder {
    /// item_index → call_id
    tools: std::collections::HashMap<u32, CallId>,
}

impl OpenAiResponsesDecoder {
    fn new() -> Self {
        Self {
            tools: Default::default(),
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
        // Responses API 的 SSE event 字段就是 type;data 里也有 type 字段(冗余)。
        let event_type = ev.event.as_deref().unwrap_or("");
        let data: Value = if ev.data.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&ev.data)
                .map_err(|e| ModelError::Stream(format!("解析 responses 事件失败: {e}")))?
        };

        let mut out = Vec::new();
        match event_type {
            "response.output_text.delta" => {
                if let Some(t) = data.get("delta").and_then(|v| v.as_str()) {
                    out.push(ModelStreamEvent::TextDelta(t.to_owned()));
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(t) = data.get("delta").and_then(|v| v.as_str()) {
                    out.push(ModelStreamEvent::ThinkingDelta(t.to_owned()));
                }
            }
            "response.output_item.added" => {
                let item = data.get("item").cloned().unwrap_or(json!({}));
                if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                    let idx = data
                        .get("output_index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let call_id = CallId::new(
                        item.get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("c-?"),
                    );
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    self.tools.insert(idx, call_id.clone());
                    out.push(ModelStreamEvent::ToolUseBegin { call_id, name });
                }
            }
            "response.function_call_arguments.delta" => {
                let idx = data
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                if let Some(call_id) = self.tools.get(&idx).cloned()
                    && let Some(frag) = data.get("delta").and_then(|v| v.as_str())
                {
                    out.push(ModelStreamEvent::ToolUseInputDelta {
                        call_id,
                        json_fragment: frag.to_owned(),
                    });
                }
            }
            "response.output_item.done" => {
                let item = data.get("item").cloned().unwrap_or(json!({}));
                if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                    let idx = data
                        .get("output_index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    if let Some(call_id) = self.tools.remove(&idx) {
                        out.push(ModelStreamEvent::ToolUseEnd { call_id });
                    }
                }
            }
            "response.completed" => {
                let resp = data.get("response").cloned().unwrap_or(json!({}));
                let stop_reason = resp
                    .get("status")
                    .and_then(|v| v.as_str())
                    .map(map_status)
                    .unwrap_or(StopReason::EndTurn);
                let usage = resp.get("usage").map(map_usage).unwrap_or_default();
                out.push(ModelStreamEvent::MessageEnd { stop_reason, usage });

                // 清理:未 done 的 tool(异常情况)
                for id in self.tools.values() {
                    out.push(ModelStreamEvent::ToolUseEnd {
                        call_id: id.clone(),
                    });
                }
                self.tools.clear();
            }
            "error" => {
                let msg = data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                return Err(ModelError::Stream(format!("provider error: {msg}")));
            }
            "response.created"
            | "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done" => {}
            _ => {
                // 忽略未知事件(向前兼容)
            }
        }
        Ok(out)
    }
}

fn map_status(s: &str) -> StopReason {
    match s {
        "completed" => StopReason::EndTurn,
        "incomplete" => StopReason::MaxTokens,
        "failed" | "cancelled" => StopReason::Error,
        _ => StopReason::EndTurn,
    }
}

fn map_usage(u: &Value) -> TokenUsage {
    TokenUsage {
        input: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cached_input: u
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        reasoning: u
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    }
}
