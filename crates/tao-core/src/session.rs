//! Turn loop:agent 循环的核心。
//! 见 docs/design/agent-loop.md。
//!
//! 一个 turn = 从用户输入到模型停止调用工具的完整交互。
//! 内部可能包含多轮"模型流 → 工具调用 → 工具结果 → 模型流"。
//!
//! `recorder` 在关键点记 `LogEvent` 落盘(见 docs/design/sessions.md §1);
//! `UserInput`/`SessionMeta`/`ModeChange` 由调用方(exec/tui)在 run_turn 外记录。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use tao_protocol::content::{Content, StopReason, TokenUsage};
use tao_protocol::event::{ApprovalDetail, ApprovalKind};
use tao_protocol::ids::{CallId, CheckpointId, SessionId, TurnId};
use tao_protocol::log::LogEvent;
use tao_protocol::op::ReviewDecision;
use tao_protocol::permission::{PermissionMode, Verdict, VerdictSource};
use tokio_util::sync::CancellationToken;

use crate::agents::load_agents;
use crate::checkpoint::ShadowRepo;
use crate::config::HooksConfig;
use crate::hooks::{HookCtx, HookEvent, HookOutcome, run_hooks};
use crate::model::{
    ModelContent, ModelMessage, ModelRequest, ModelStreamEvent, RequestMeta, SystemBlock,
};
use crate::permissions::{ApprovalRequest, Approver, PermissionEngine, approval_request};
use crate::providers::ModelClient;
use crate::recorder::{JsonlRecorder, Recorder};
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
    /// 本轮 token 用量(在 ModelMessageEnd 后发出,供 /cost 展示)。
    Usage(TokenUsage),
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
    /// 后台通知消息(如"未信任项目"提示),非错误,非 turn 边界。
    BackgroundEvent(String),
}

/// turn 执行结果。
#[derive(Debug)]
pub struct TurnResult {
    pub stop_reason: StopReason,
    pub steps: u32,
    /// 本轮最后一次模型流的 token 用量(供成本展示)。
    pub usage: TokenUsage,
    /// 完整的对话历史(含本轮新增的 user/assistant/tool_result)。
    pub messages: Vec<ModelMessage>,
}

/// turn loop 配置。
#[derive(Debug, Clone)]
pub struct TurnConfig {
    /// 最大"模型流 → 工具"轮次,防失控。默认 100。
    pub max_steps: u32,
    /// 受信任的项目 cwd 列表(从 Config.trusted_projects 传入)。
    /// 若 cwd 不在列表且 session_start hooks 非空,emit BackgroundEvent 提示。
    pub trusted_projects: Vec<String>,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self {
            max_steps: 100,
            trusted_projects: Vec::new(),
        }
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
    shadow: Option<&ShadowRepo>,
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
    // turn 级已读文件集(跨工具调用持久,供 Edit 校验"先 Read")
    let read_files: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));

    // UserPromptSubmit hook:turn 执行前跑,可阻断 prompt(v1 不支持 Modify,仅 Block)
    let prompt_text = messages
        .iter()
        .rev()
        .find_map(|m| match m {
            ModelMessage::User { content } => content.iter().find_map(|c| match c {
                ModelContent::Text(t) => Some(t.clone()),
                _ => None,
            }),
            _ => None,
        })
        .unwrap_or_default();
    let up_ctx = HookCtx {
        session_id: session_id_str.clone(),
        cwd: cwd.to_path_buf(),
        tool_name: None,
        tool_input: Some(Value::String(prompt_text.clone())),
    };
    if let HookOutcome::Block(reason) = run_hooks(
        &HookEvent::UserPromptSubmit { text: prompt_text },
        &hooks.user_prompt_submit,
        &up_ctx,
    )
    .await
    {
        on_event(TurnEvent::Error(format!(
            "UserPromptSubmit hook 阻断: {reason}"
        )));
        record_turn_boundary(recorder, &turn_id, StopReason::Interrupted);
        return Ok(TurnResult {
            stop_reason: StopReason::Interrupted,
            steps: 0,
            usage: TokenUsage::default(),
            messages: messages.clone(),
        });
    }

    // SessionStart hook:turn 执行前跑(fire-and-forget,不阻断)。
    // 若 cwd 不在 trusted_projects 且 session_start hooks 非空,提示未信任项目。
    let is_trusted = config.trusted_projects.iter().any(|p| {
        let p = std::path::Path::new(p);
        p == cwd
    });
    if !is_trusted && !hooks.session_start.is_empty() {
        on_event(TurnEvent::BackgroundEvent(format!(
            "⚠ 未信任项目: {}。session_start hooks 将执行,请确认项目安全。",
            cwd.display()
        )));
    }
    let ss_ctx = HookCtx {
        session_id: session_id_str.clone(),
        cwd: cwd.to_path_buf(),
        tool_name: None,
        tool_input: None,
    };
    let _ = run_hooks(&HookEvent::SessionStart, &hooks.session_start, &ss_ctx).await;

    loop {
        steps += 1;
        let mut last_usage = TokenUsage::default();
        if steps > config.max_steps {
            on_event(TurnEvent::Error(format!(
                "max_steps({}) 超限",
                config.max_steps
            )));
            run_notification(hooks, "", &session_id_str, cwd).await;
            record_turn_boundary(recorder, &turn_id, StopReason::MaxSteps);
            return Ok(TurnResult {
                stop_reason: StopReason::MaxSteps,
                steps: steps - 1,
                usage: last_usage,
                messages: messages.clone(),
            });
        }

        if cancel.is_cancelled() {
            run_notification(hooks, "", &session_id_str, cwd).await;
            record_turn_boundary(recorder, &turn_id, StopReason::Interrupted);
            return Ok(TurnResult {
                stop_reason: StopReason::Interrupted,
                steps: steps - 1,
                usage: last_usage,
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

        while let Some(ev) = stream.next().await {
            if cancel.is_cancelled() {
                run_notification(hooks, &text_buf, &session_id_str, cwd).await;
                record_turn_boundary(recorder, &turn_id, StopReason::Interrupted);
                return Ok(TurnResult {
                    stop_reason: StopReason::Interrupted,
                    steps: steps - 1,
                    usage: last_usage,
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
            asst_content.push(ModelContent::Text(text_buf.clone()));
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
        on_event(TurnEvent::Usage(last_usage));

        // 无工具调用 → turn 结束。
        if tool_calls.is_empty() || stop_reason != StopReason::ToolUse {
            // Notification hook:turn 结束前跑(fire-and-forget),message 取最后 assistant text。
            run_notification(hooks, &text_buf, &session_id_str, cwd).await;
            // SessionEnd / Stop hook:正常结束 turn 前跑(若非空)。
            run_session_end_stop(hooks, &session_id_str, cwd).await;
            record_turn_boundary(recorder, &turn_id, stop_reason);
            return Ok(TurnResult {
                stop_reason,
                steps,
                usage: last_usage,
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

            // Task 工具:特殊处理(spawn 子 agent run_turn)
            if tool_name == "Task" {
                let output = exec_task(
                    client,
                    tools,
                    hooks,
                    request,
                    cwd,
                    cancel,
                    &session_id_str,
                    input,
                    1,
                )
                .await;
                let (ok, summary, content, is_error) = match output {
                    Ok(o) => (!o.is_error, summarize(&o.content), o.content, o.is_error),
                    Err(e) => {
                        let msg = e.to_string();
                        (false, msg.clone(), format!("[子 agent 失败: {msg}]"), true)
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
                record_tool_result(recorder, call_id, &content, is_error, 0);
                continue;
            }

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
                    // shadow-git 快照(Edit/Write 前)
                    if let Some(shadow) = shadow
                        && let Some(crate::permissions::PermissionKey::Path { path }) = &key
                    {
                        match shadow.snapshot(std::slice::from_ref(path)) {
                            Ok(Some(hash)) => {
                                recorder.record(LogEvent::Checkpoint {
                                    checkpoint_id: CheckpointId::new(
                                        uuid::Uuid::new_v4().to_string(),
                                    ),
                                    shadow_commit: hash,
                                });
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!("shadow 快照失败(skip): {e}"),
                        }
                    }
                    // PreToolUse hook
                    if let Some(reason) =
                        run_pre_tool_use(hooks, tool_name, input, &session_id_str, cwd).await
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
                    let r = exec_tool(tools, tool_name, input, cwd, cancel, &read_files).await;
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
                            if let Some(reason) =
                                run_pre_tool_use(hooks, tool_name, input, &session_id_str, cwd)
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
                            let r =
                                exec_tool(tools, tool_name, input, cwd, cancel, &read_files).await;
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
                            if let Some(reason) =
                                run_pre_tool_use(hooks, tool_name, input, &session_id_str, cwd)
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
                            let r =
                                exec_tool(tools, tool_name, input, cwd, cancel, &read_files).await;
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
                            run_notification(hooks, &text_buf, &session_id_str, cwd).await;
                            record_turn_boundary(recorder, &turn_id, StopReason::Interrupted);
                            return Ok(TurnResult {
                                stop_reason: StopReason::Interrupted,
                                steps,
                                usage: last_usage,
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

/// 子 agent 审批器:Plan 只读不触发审批,Deny 兜底(不应被调)。
struct NullApprover;
#[async_trait]
impl Approver for NullApprover {
    async fn request(&self, _req: ApprovalRequest) -> ReviewDecision {
        ReviewDecision::Deny
    }
}

async fn exec_tool(
    tools: &ToolRegistry,
    name: &str,
    input: &Value,
    cwd: &Path,
    cancel: &CancellationToken,
    read_files: &Arc<Mutex<HashSet<PathBuf>>>,
) -> Result<ToolOutput, ToolError> {
    let tool: Arc<dyn Tool> = match tools.get(name) {
        Some(t) => t,
        None => return Ok(ToolOutput::error(format!("未知工具: {name}"))),
    };
    let ctx = ToolCtx::with_cancel_and_reads(cwd, cancel.clone(), read_files.clone());
    tool.call(input, &ctx).await
}

/// 跑 PreToolUse hook。返回 `Some(reason)` 表示被阻断(Block)。
/// Allow / Approve / ApproveForSession 三路径统一调用,保证 hook 一致性。
async fn run_pre_tool_use(
    hooks: &HooksConfig,
    tool_name: &str,
    input: &Value,
    session_id: &str,
    cwd: &Path,
) -> Option<String> {
    let ctx = HookCtx {
        session_id: session_id.to_string(),
        cwd: cwd.to_path_buf(),
        tool_name: Some(tool_name.to_string()),
        tool_input: Some(input.clone()),
    };
    match run_hooks(
        &HookEvent::PreToolUse {
            tool: tool_name.to_string(),
        },
        &hooks.pre_tool_use,
        &ctx,
    )
    .await
    {
        HookOutcome::Block(reason) => Some(reason),
        HookOutcome::Pass => None,
    }
}

/// 跑 Notification hook(fire-and-forget)。`message` 为最后 assistant text 或空。
/// 在 run_turn 各 return 点(max_steps / 中断 / abort / end turn)前调用。
async fn run_notification(hooks: &HooksConfig, message: &str, session_id: &str, cwd: &Path) {
    if hooks.notification.is_empty() {
        return;
    }
    let ctx = HookCtx {
        session_id: session_id.to_string(),
        cwd: cwd.to_path_buf(),
        tool_name: None,
        tool_input: None,
    };
    let _ = run_hooks(
        &HookEvent::Notification {
            message: message.to_string(),
        },
        &hooks.notification,
        &ctx,
    )
    .await;
}

/// 跑 SessionEnd + Stop hook(正常 turn 结束前,fire-and-forget,仅 end-turn return)。
async fn run_session_end_stop(hooks: &HooksConfig, session_id: &str, cwd: &Path) {
    if hooks.session_end.is_empty() && hooks.stop.is_empty() {
        return;
    }
    let ctx = HookCtx {
        session_id: session_id.to_string(),
        cwd: cwd.to_path_buf(),
        tool_name: None,
        tool_input: None,
    };
    if !hooks.session_end.is_empty() {
        let _ = run_hooks(&HookEvent::SessionEnd, &hooks.session_end, &ctx).await;
    }
    if !hooks.stop.is_empty() {
        let _ = run_hooks(&HookEvent::Stop, &hooks.stop, &ctx).await;
    }
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

/// Task 工具:spawn 子 agent run_turn,返回报告。
#[allow(clippy::too_many_arguments)]
async fn exec_task(
    client: &dyn ModelClient,
    tools: &ToolRegistry,
    hooks: &HooksConfig,
    request: &ModelRequest,
    cwd: &Path,
    cancel: &CancellationToken,
    session_id: &str,
    input: &Value,
    depth: u32,
) -> Result<ToolOutput, ToolError> {
    // 防递归失控:子 agent 嵌套深度上限 8(v1 子只读无 Task,实际不嵌套;预防)
    if depth > 8 {
        return Ok(ToolOutput::error(format!(
            "子 agent 嵌套深度超限({depth}),已拒绝"
        )));
    }
    let subagent = input.get("subagent").and_then(|v| v.as_str()).unwrap_or("");
    let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    if subagent.is_empty() {
        return Ok(ToolOutput::error("Task 缺少 subagent 参数"));
    }
    let agents = load_agents(cwd);
    let def = match agents.iter().find(|a| a.name == subagent) {
        Some(d) => d,
        None => return Ok(ToolOutput::error(format!("未知子 agent: {subagent}"))),
    };
    let sub_tools = tools.readonly_subset(&def.tools);
    let sub_engine =
        PermissionEngine::new(def.permission_mode.unwrap_or(PermissionMode::Plan), vec![]);
    let (sub_recorder, _sub_id) =
        JsonlRecorder::create_fork(cwd, &SessionId::new(session_id.to_string()), String::new())
            .map_err(|e| ToolError::Failed(format!("子会话创建失败: {e}")))?;
    let sub_model = def.model.clone().unwrap_or_else(|| request.model.clone());
    let sub_system = vec![SystemBlock {
        text: def.system_prompt.clone(),
        cache_breakpoint: None,
    }];
    let mut sub_messages = vec![ModelMessage::User {
        content: vec![ModelContent::text(prompt)],
    }];
    let sub_req = ModelRequest {
        model: sub_model,
        system: sub_system,
        messages: vec![],
        tools: sub_tools.specs(),
        reasoning: None,
        max_output_tokens: 4096,
        temperature: None,
        metadata: RequestMeta::default(),
    };
    let sub_config = TurnConfig {
        max_steps: 20,
        trusted_projects: Vec::new(),
    };
    let sub_result = Box::pin(run_turn(
        client,
        &sub_tools,
        &sub_engine,
        &NullApprover,
        &sub_recorder,
        hooks,
        None, // 子 agent 不快照(v1)
        &sub_req,
        &mut sub_messages,
        &sub_config,
        cwd,
        cancel,
        |_ev| {},
    ))
    .await;
    let report = sub_messages
        .iter()
        .rev()
        .find_map(|m| {
            if let ModelMessage::Assistant { content } = m {
                content.iter().find_map(|c| match c {
                    ModelContent::Text(t) => Some(t.clone()),
                    _ => None,
                })
            } else {
                None
            }
        })
        .unwrap_or_else(|| match sub_result {
            Ok(r) => format!("[子 agent 无文本输出: {:?}]", r.stop_reason),
            Err(e) => format!("[子 agent 错误: {e}]"),
        });
    let _ = run_hooks(
        &HookEvent::SubagentStop {
            name: subagent.to_string(),
        },
        &hooks.subagent_stop,
        &HookCtx {
            session_id: session_id.to_string(),
            cwd: cwd.to_path_buf(),
            tool_name: None,
            tool_input: None,
        },
    )
    .await;
    Ok(ToolOutput::ok(report))
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
