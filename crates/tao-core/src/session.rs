//! Turn loop:agent 循环的核心。
//! 见 docs/design/agent-loop.md。
//!
//! 一个 turn = 从用户输入到模型停止调用工具的完整交互。
//! 内部可能包含多轮"模型流 → 工具调用 → 工具结果 → 模型流"。
//!
//! `recorder` 在关键点记 `LogEvent` 落盘(见 docs/design/sessions.md §1);
//! `UserInput`/`SessionMeta`/`ModeChange` 由调用方(exec/tui)在 run_turn 外记录。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use serde_json::Value;
use tao_protocol::content::{Content, StopReason, TokenUsage};
use tao_protocol::event::{ApprovalDetail, ApprovalKind};
use tao_protocol::ids::{CallId, TurnId};
use tao_protocol::log::LogEvent;
use tao_protocol::op::ReviewDecision;
use tao_protocol::permission::{Verdict, VerdictSource};
use tokio_util::sync::CancellationToken;

use crate::config::HooksConfig;
use crate::hooks::{HookCtx, HookEvent, HookOutcome, run_hooks};
use crate::model::{ModelContent, ModelMessage, ModelRequest, ModelStreamEvent};
use crate::permissions::{Approver, PermissionEngine, approval_request};
use crate::providers::ModelClient;
use crate::recorder::Recorder;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput, ToolRegistry};

/// turn loop 产出的流式事件(供 TUI / exec 消费)。
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// 模型文本增量。
    TextDelta(String),
    /// 模型思考增量。
    ThinkingDelta(String),
    /// 工具调用开始(模型要求调用某工具)。
    ToolCallBegin { call_id: CallId, tool: String },
    /// 工具调用结束(参数已完整)。
    ToolCallEnd { call_id: CallId },
    /// 工具执行开始。
    ToolExecBegin { call_id: CallId },
    /// 工具执行结束(含输出摘要)。
    ToolExecEnd {
        call_id: CallId,
        ok: bool,
        summary: String,
    },
    /// 本轮模型流结束(可能还有下一轮工具调用)。
    ModelMessageEnd { stop_reason: StopReason },
    /// 整个 turn 结束。
    TurnComplete { stop_reason: StopReason, steps: u32 },
    /// 需要用户审批(Ask 判定):前端渲染弹窗,等待按键应答。
    ApprovalRequest {
        call_id: CallId,
        kind: ApprovalKind,
        detail: ApprovalDetail,
    },
    /// 审批已解决(用户做了决定)。
    ApprovalResolved {
        call_id: CallId,
        decision: ReviewDecision,
    },
    /// 错误(turn 终止)。
    Error(String),
}

/// turn 执行结果。
#[derive(Debug)]
pub struct TurnResult {
    pub stop_reason: StopReason,
    pub steps: u32,
    /// 完整的对话历史(含本轮新增的 user/assistant/tool_result)。
    pub messages: Vec<ModelMessage>,
}

/// turn loop 配置。
#[derive(Debug, Clone)]
pub struct TurnConfig {
    /// 最大"模型流 → 工具"轮次,防失控。默认 100。
    pub max_steps: u32,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self { max_steps: 100 }
    }
}

/// 运行一个 turn。
///
/// `messages` 是已有历史(可空);本函数会向其追加 assistant 消息、tool_result 等,
/// 循环直到模型不再调用工具。
///
/// `engine` 做权限判定,`approver` 在 Ask 判定时等待前端应答,
/// `recorder` 落盘 LogEvent。`on_event` 是同步回调(消费 TurnEvent)。
#[allow(clippy::too_many_arguments)]
pub async fn run_turn<F>(
    client: &dyn ModelClient,
    tools: &ToolRegistry,
    engine: &PermissionEngine,
    approver: &dyn Approver,
    recorder: &dyn Recorder,
    hooks: &HooksConfig,
    request: &ModelRequest,
    messages: &mut Vec<ModelMessage>,
    config: &TurnConfig,
    cwd: &Path,
    cancel: &CancellationToken,
    mut on_event: F,
) -> Result<TurnResult, crate::model::ModelError>
where
    F: FnMut(TurnEvent) + Send,
{
    let mut steps: u32 = 0;
    let turn_id = TurnId::new(request.metadata.turn_id.clone().unwrap_or_default());
    let session_id_str = request.metadata.session_id.clone().unwrap_or_default();

    loop {
        steps += 1;
        if steps > config.max_steps {
            on_event(TurnEvent::Error(format!(
                "max_steps({}) 超限",
                config.max_steps
            )));
            record_turn_boundary(recorder, &turn_id, StopReason::MaxSteps);
            return Ok(TurnResult {
                stop_reason: StopReason::MaxSteps,
                steps: steps - 1,
                messages: messages.clone(),
            });
        }

        if cancel.is_cancelled() {
            record_turn_boundary(recorder, &turn_id, StopReason::Interrupted);
            return Ok(TurnResult {
                stop_reason: StopReason::Interrupted,
                steps: steps - 1,
                messages: messages.clone(),
            });
        }

        // 构建本次请求(完整历史 + tools)。
        let req = ModelRequest {
            model: request.model.clone(),
            system: request.system.clone(),
            messages: messages.clone(),
            tools: tools.specs(),
            reasoning: request.reasoning,
            max_output_tokens: request.max_output_tokens,
            temperature: request.temperature,
            metadata: request.metadata.clone(),
        };

        let mut stream = client.stream(&req, cancel).await?;

        // 累积本轮模型消息。
        let mut text_buf = String::new();
        let mut thinking_buf = String::new();
        let mut tool_inputs: HashMap<CallId, ToolInputAccum> = HashMap::new();
        let mut tool_order: Vec<CallId> = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut last_usage = TokenUsage::default();

        while let Some(ev) = stream.next().await {
            if cancel.is_cancelled() {
                record_turn_boundary(recorder, &turn_id, StopReason::Interrupted);
                return Ok(TurnResult {
                    stop_reason: StopReason::Interrupted,
                    steps: steps - 1,
                    messages: messages.clone(),
                });
            }
            match ev? {
                ModelStreamEvent::TextDelta(t) => {
                    on_event(TurnEvent::TextDelta(t.clone()));
                    text_buf.push_str(&t);
                }
                ModelStreamEvent::ThinkingDelta(t) => {
                    on_event(TurnEvent::ThinkingDelta(t.clone()));
                    thinking_buf.push_str(&t);
                }
                ModelStreamEvent::ToolUseBegin { call_id, name } => {
                    on_event(TurnEvent::ToolCallBegin {
                        call_id: call_id.clone(),
                        tool: name.clone(),
                    });
                    tool_inputs
                        .entry(call_id.clone())
                        .or_insert_with(|| ToolInputAccum {
                            name,
                            json_fragments: Vec::new(),
                        });
                    tool_order.push(call_id);
                }
                ModelStreamEvent::ToolUseInputDelta {
                    call_id,
                    json_fragment,
                } => {
                    if let Some(acc) = tool_inputs.get_mut(&call_id) {
                        acc.json_fragments.push(json_fragment);
                    }
                }
                ModelStreamEvent::ToolUseEnd { call_id } => {
                    on_event(TurnEvent::ToolCallEnd { call_id });
                }
                ModelStreamEvent::MessageEnd {
                    stop_reason: sr,
                    usage,
                } => {
                    stop_reason = sr;
                    last_usage = usage;
                    break;
                }
            }
        }

        // 构建 assistant 消息并入历史。
        let mut asst_content: Vec<ModelContent> = Vec::new();
        if !thinking_buf.is_empty() {
            asst_content.push(ModelContent::Thinking {
                text: thinking_buf,
                signature: None,
            });
        }
        if !text_buf.is_empty() {
            asst_content.push(ModelContent::Text(text_buf));
        }
        // 解析工具调用参数。
        let mut tool_calls: Vec<(CallId, String, Value)> = Vec::new();
        for call_id in &tool_order {
            if let Some(acc) = tool_inputs.remove(call_id) {
                let input_str: String = acc.json_fragments.concat();
                let input: Value = if input_str.is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&input_str).unwrap_or_else(|e| {
                        Value::String(format!("[参数解析失败: {e}]\n原始: {input_str}"))
                    })
                };
                asst_content.push(ModelContent::ToolUse {
                    call_id: call_id.clone(),
                    name: acc.name.clone(),
                    input: input.clone(),
                });
                tool_calls.push((call_id.clone(), acc.name, input));
            }
        }
        // 记录 assistant 消息(落盘)
        recorder.record(LogEvent::AssistantMessage {
            content: asst_content.iter().map(model_content_to_content).collect(),
            usage: last_usage,
            turn_id: turn_id.clone(),
        });
        messages.push(ModelMessage::Assistant {
            content: asst_content,
        });

        on_event(TurnEvent::ModelMessageEnd { stop_reason });

        // 无工具调用 → turn 结束。
        if tool_calls.is_empty() || stop_reason != StopReason::ToolUse {
            record_turn_boundary(recorder, &turn_id, stop_reason);
            return Ok(TurnResult {
                stop_reason,
                steps,
                messages: messages.clone(),
            });
        }

        // 执行工具(先过权限判定),结果入历史。
        for (call_id, tool_name, input) in &tool_calls {
            let tool_arc = tools.get(tool_name);
            let key = tool_arc.as_ref().and_then(|t| t.permission_key(input, cwd));
            // 记录工具调用意图(决策前)
            recorder.record(LogEvent::ToolCall {
                call_id: call_id.clone(),
                tool: tool_name.clone(),
                args: input.clone(),
            });
            let decision = engine.decide(tool_name, key.as_ref());
            let rule_matched = match &decision.source {
                VerdictSource::Rule { rule } => Some(rule.pattern.as_str()),
                _ => None,
            };

            let mut duration_ms: u64 = 0;
            let output: Result<ToolOutput, ToolError> = match decision.verdict {
                Verdict::Deny => {
                    let msg = format!("被权限策略拒绝(工具: {tool_name})");
                    on_event(TurnEvent::ToolExecEnd {
                        call_id: call_id.clone(),
                        ok: false,
                        summary: "权限拒绝".into(),
                    });
                    messages.push(ModelMessage::ToolResult {
                        call_id: call_id.clone(),
                        content: vec![ModelContent::Text(msg.clone())],
                        is_error: true,
                    });
                    record_tool_result(recorder, call_id, &msg, true, duration_ms);
                    continue;
                }
                Verdict::Allow => {
                    // PreToolUse hook(Allow 路径;Approve/ApproveForSession v1 跳过,TODO)
                    let hook_ctx = HookCtx {
                        session_id: session_id_str.clone(),
                        cwd: cwd.to_path_buf(),
                        tool_name: Some(tool_name.to_string()),
                        tool_input: Some(input.clone()),
                    };
                    if let HookOutcome::Block(reason) = run_hooks(
                        &HookEvent::PreToolUse {
                            tool: tool_name.to_string(),
                        },
                        &hooks.pre_tool_use,
                        &hook_ctx,
                    )
                    .await
                    {
                        let msg = format!("被 hook 阻断: {reason}");
                        on_event(TurnEvent::ToolExecEnd {
                            call_id: call_id.clone(),
                            ok: false,
                            summary: "hook 阻断".into(),
                        });
                        messages.push(ModelMessage::ToolResult {
                            call_id: call_id.clone(),
                            content: vec![ModelContent::Text(msg.clone())],
                            is_error: true,
                        });
                        record_tool_result(recorder, call_id, &msg, true, duration_ms);
                        continue;
                    }
                    on_event(TurnEvent::ToolExecBegin {
                        call_id: call_id.clone(),
                    });
                    let start = Instant::now();
                    let r = exec_tool(tools, tool_name, input, cwd, cancel).await;
                    duration_ms = start.elapsed().as_millis() as u64;
                    r
                }
                Verdict::Ask => {
                    let req =
                        approval_request(call_id.clone(), tool_name, key.as_ref(), rule_matched);
                    let suggestion = req.detail.pattern_suggestion.clone();
                    on_event(TurnEvent::ApprovalRequest {
                        call_id: call_id.clone(),
                        kind: req.kind,
                        detail: req.detail.clone(),
                    });
                    let resp = approver.request(req).await;
                    on_event(TurnEvent::ApprovalResolved {
                        call_id: call_id.clone(),
                        decision: resp,
                    });
                    recorder.record(LogEvent::Approval {
                        call_id: call_id.clone(),
                        verdict: resp,
                        rule_suggestion: suggestion,
                    });
                    match resp {
                        ReviewDecision::Approve => {
                            on_event(TurnEvent::ToolExecBegin {
                                call_id: call_id.clone(),
                            });
                            let start = Instant::now();
                            let r = exec_tool(tools, tool_name, input, cwd, cancel).await;
                            duration_ms = start.elapsed().as_millis() as u64;
                            r
                        }
                        ReviewDecision::ApproveForSession => {
                            if let Some(k) = &key {
                                let pat = k.pattern_string();
                                engine.grant(tool_name, &pat);
                                recorder.record(LogEvent::PermissionGrant {
                                    tool: tool_name.to_string(),
                                    pattern: pat,
                                });
                            }
                            on_event(TurnEvent::ToolExecBegin {
                                call_id: call_id.clone(),
                            });
                            let start = Instant::now();
                            let r = exec_tool(tools, tool_name, input, cwd, cancel).await;
                            duration_ms = start.elapsed().as_millis() as u64;
                            r
                        }
                        ReviewDecision::Deny => {
                            let msg = format!("用户拒绝执行(工具: {tool_name})");
                            on_event(TurnEvent::ToolExecEnd {
                                call_id: call_id.clone(),
                                ok: false,
                                summary: "用户拒绝".into(),
                            });
                            messages.push(ModelMessage::ToolResult {
                                call_id: call_id.clone(),
                                content: vec![ModelContent::Text(msg.clone())],
                                is_error: true,
                            });
                            record_tool_result(recorder, call_id, &msg, true, duration_ms);
                            continue;
                        }
                        ReviewDecision::Abort => {
                            record_turn_boundary(recorder, &turn_id, StopReason::Interrupted);
                            return Ok(TurnResult {
                                stop_reason: StopReason::Interrupted,
                                steps,
                                messages: messages.clone(),
                            });
                        }
                    }
                }
            };

            let (ok, summary, content, is_error) = match output {
                Ok(o) => {
                    let ok = !o.is_error;
                    let summary = summarize(&o.content);
                    (ok, summary, o.content, o.is_error)
                }
                Err(e) => {
                    let msg = e.to_string();
                    (false, msg.clone(), format!("[工具执行失败: {msg}]"), true)
                }
            };
            on_event(TurnEvent::ToolExecEnd {
                call_id: call_id.clone(),
                ok,
                summary,
            });
            messages.push(ModelMessage::ToolResult {
                call_id: call_id.clone(),
                content: vec![ModelContent::Text(content.clone())],
                is_error,
            });
            record_tool_result(recorder, call_id, &content, is_error, duration_ms);
            // PostToolUse hook(非阻断,不影响结果)
            let post_ctx = HookCtx {
                session_id: session_id_str.clone(),
                cwd: cwd.to_path_buf(),
                tool_name: Some(tool_name.to_string()),
                tool_input: Some(input.clone()),
            };
            let _ = run_hooks(
                &HookEvent::PostToolUse {
                    tool: tool_name.to_string(),
                },
                &hooks.post_tool_use,
                &post_ctx,
            )
            .await;
        }
        // 循环:带着工具结果再问模型。
    }
}

struct ToolInputAccum {
    name: String,
    json_fragments: Vec<String>,
}

async fn exec_tool(
    tools: &ToolRegistry,
    name: &str,
    input: &Value,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let tool: Arc<dyn Tool> = match tools.get(name) {
        Some(t) => t,
        None => return Ok(ToolOutput::error(format!("未知工具: {name}"))),
    };
    let ctx = ToolCtx::with_cancel(cwd, cancel.clone());
    tool.call(input, &ctx).await
}

fn summarize(s: &str) -> String {
    let first_line = s.lines().next().unwrap_or("");
    if first_line.len() > 80 {
        format!("{}...", &first_line[..77])
    } else if s.lines().count() > 1 {
        format!("{first_line} ...")
    } else {
        first_line.to_owned()
    }
}

/// 记录 turn 边界(各 return 点前调)。
fn record_turn_boundary(recorder: &dyn Recorder, turn_id: &TurnId, stop_reason: StopReason) {
    recorder.record(LogEvent::TurnBoundary {
        turn_id: turn_id.clone(),
        stop_reason,
    });
}

/// 记录工具结果(output 约定为 {"content": <str>, "is_error": <bool>},replay 侧解析)。
fn record_tool_result(
    recorder: &dyn Recorder,
    call_id: &CallId,
    content: &str,
    is_error: bool,
    duration_ms: u64,
) {
    recorder.record(LogEvent::ToolResult {
        call_id: call_id.clone(),
        output: serde_json::json!({"content": content, "is_error": is_error}),
        duration_ms,
    });
}

/// ModelContent(core)→ Content(protocol),落盘用。
fn model_content_to_content(mc: &ModelContent) -> Content {
    match mc {
        ModelContent::Text(t) => Content::Text { text: t.clone() },
        ModelContent::Thinking { text, signature } => Content::Thinking {
            text: text.clone(),
            signature: signature.clone(),
        },
        ModelContent::ToolUse {
            call_id,
            name,
            input,
        } => Content::ToolUse {
            call_id: call_id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ModelContent::Image { mime, data_base64 } => Content::Image {
            mime: mime.clone(),
            data_base64: data_base64.clone(),
        },
    }
}
