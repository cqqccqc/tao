//! # tao-exec
//!
//! headless 单次执行:`tao exec "fix the tests"`。
//! Ask 审批默认 deny(`--on-ask approve` 可改)。会话落盘 JSONL(`--resume`/`--fork`)。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tao_core::config::{Config, LoadOpts};
use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::permissions::{ApprovalRequest, Approver, PermissionEngine};
use tao_core::providers::ModelClient;
use tao_core::providers::registry::resolve;
use tao_core::recorder::{JsonlRecorder, Recorder, session_file_path};
use tao_core::replay::replay;
use tao_core::session::{TurnConfig, TurnEvent, run_turn};
use tao_core::tools::ToolRegistry;
use tao_protocol::content::Content;
use tao_protocol::ids::{SessionId, TurnId};
use tao_protocol::log::LogEvent;
use tao_protocol::op::ReviewDecision;
use tokio_util::sync::CancellationToken;

/// headless 模式下 Ask 审批的处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnAsk {
    /// 拒绝(默认):模型收到"用户拒绝",通常换方案。保证脚本可预期。
    #[default]
    Deny,
    /// 批准:等价自动 yes,仅用于受信任的自动化。
    Approve,
}

/// headless 审批器:Ask 时按 on_ask 直接决策,无 IO。
struct HeadlessApprover {
    on_ask: OnAsk,
}

#[async_trait]
impl Approver for HeadlessApprover {
    async fn request(&self, _req: ApprovalRequest) -> ReviewDecision {
        match self.on_ask {
            OnAsk::Deny => ReviewDecision::Deny,
            OnAsk::Approve => ReviewDecision::Approve,
        }
    }
}

/// exec 的运行选项。
pub struct ExecOpts {
    pub prompt: String,
    pub cwd: PathBuf,
    /// 覆盖默认模型(从 config 推断)。
    pub model: Option<String>,
    pub json: bool,
    pub on_ask: OnAsk,
    /// resume 指定 session id;配合 `fork` 分叉新会话。
    pub resume: Option<String>,
    pub fork: bool,
    pub load_opts: LoadOpts,
}

pub async fn run(opts: ExecOpts) -> anyhow::Result<()> {
    let config = Config::load(&opts.load_opts)?;
    let (client, model) = resolve(&config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let model = opts.model.unwrap_or(model);

    let tools = ToolRegistry::builtin();
    let mut system: Vec<SystemBlock> = Vec::new();
    if let Some(instr) = tao_core::instructions::load(&opts.cwd) {
        system.push(SystemBlock {
            text: instr,
            cache_breakpoint: None,
        });
    }
    system.push(SystemBlock {
        text: "你是 tao,一个 Rust 编写的 coding agent。\
               你可以通过 Bash / Read / Write / Edit / Patch / Grep / Glob 工具帮助用户完成任务。\
               优先用最少的步骤解决问题。"
            .into(),
        cache_breakpoint: None,
    });

    let cwd = opts.cwd.clone();

    // 构造 recorder + messages + engine(新会话 / resume / fork)
    let (recorder, mut messages, engine, session_id) = if let Some(id_str) = &opts.resume {
        let parent_id = SessionId::new(id_str.clone());
        let path = session_file_path(&cwd, &parent_id)
            .ok_or_else(|| anyhow::anyhow!("无法定位会话日志(HOME 未设置)"))?;
        let state = replay(&path).map_err(|e| anyhow::anyhow!("重放会话失败: {e}"))?;
        let eng = PermissionEngine::new(state.mode, config.permissions.rules.clone());
        for (t, p) in &state.session_grants {
            eng.grant(t, p);
        }
        if opts.fork {
            let (r, new_id) = JsonlRecorder::create_fork(&cwd, &parent_id)
                .map_err(|e| anyhow::anyhow!("创建 fork 会话失败: {e}"))?;
            (r, state.messages, eng, new_id)
        } else {
            let r = JsonlRecorder::open_existing(&parent_id, &cwd)
                .map_err(|e| anyhow::anyhow!("打开会话失败: {e}"))?;
            (r, state.messages, eng, parent_id)
        }
    } else {
        let (r, id) =
            JsonlRecorder::create(&cwd).map_err(|e| anyhow::anyhow!("创建会话失败: {e}"))?;
        let eng = PermissionEngine::new(config.permission_mode, config.permissions.rules.clone());
        (r, Vec::new(), eng, id)
    };

    if !opts.json {
        eprintln!("[tao] session: {}", session_id.as_ref());
    }

    let turn_id = TurnId::new(uuid::Uuid::new_v4().to_string());
    recorder.record(LogEvent::UserInput {
        content: vec![Content::text(&opts.prompt)],
        turn_id: turn_id.clone(),
    });
    messages.push(ModelMessage::User {
        content: vec![ModelContent::text(&opts.prompt)],
    });

    let req = ModelRequest {
        model,
        system,
        messages: vec![],
        tools: vec![],
        reasoning: config.reasoning_effort,
        max_output_tokens: 4096,
        temperature: None,
        metadata: RequestMeta {
            session_id: Some(session_id.as_ref().to_string()),
            turn_id: Some(turn_id.as_ref().to_string()),
        },
    };
    let config_turn = TurnConfig {
        max_steps: config.max_turn_steps,
    };
    let cancel = CancellationToken::new();
    let approver = HeadlessApprover {
        on_ask: opts.on_ask,
    };

    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n[tao] 收到 Ctrl-C,正在中断...");
            cancel2.cancel();
        }
    });

    if opts.json {
        run_json(
            client,
            &tools,
            &engine,
            &approver,
            &recorder,
            &req,
            &mut messages,
            &config_turn,
            &cwd,
            &cancel,
        )
        .await
    } else {
        run_text(
            client,
            &tools,
            &engine,
            &approver,
            &recorder,
            &req,
            &mut messages,
            &config_turn,
            &cwd,
            &cancel,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_text(
    client: Arc<dyn ModelClient>,
    tools: &ToolRegistry,
    engine: &PermissionEngine,
    approver: &dyn Approver,
    recorder: &dyn Recorder,
    req: &ModelRequest,
    messages: &mut Vec<ModelMessage>,
    config: &TurnConfig,
    cwd: &std::path::Path,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};

    let mut stdout = std::io::stdout();
    let is_tty = std::io::stdout().is_terminal();

    let result = run_turn(
        client.as_ref(),
        tools,
        engine,
        approver,
        recorder,
        req,
        messages,
        config,
        cwd,
        cancel,
        |ev| match ev {
            TurnEvent::TextDelta(t) => {
                let _ = stdout.write_all(t.as_bytes());
                let _ = stdout.flush();
            }
            TurnEvent::ToolCallBegin { tool, .. } => {
                if is_tty {
                    let _ = writeln!(stdout, "\n[tool: {tool}]");
                }
            }
            TurnEvent::ToolExecEnd { ok, summary, .. } => {
                if is_tty {
                    let mark = if ok { "✓" } else { "✗" };
                    let _ = writeln!(stdout, "[{mark} {summary}]");
                }
            }
            TurnEvent::ApprovalRequest { kind, detail, .. } => {
                if is_tty {
                    let tool = detail.tool.as_deref().unwrap_or("?");
                    let _ = writeln!(stdout, "\n[审批请求 {:?}: {tool}]", kind);
                    if let Some(cmd) = &detail.command {
                        let _ = writeln!(stdout, "  命令: {}", cmd.join(" "));
                    }
                }
            }
            TurnEvent::ApprovalResolved { decision, .. } => {
                if is_tty {
                    let _ = writeln!(stdout, "  审批决定: {:?}", decision);
                }
            }
            TurnEvent::Error(msg) => {
                let _ = writeln!(stdout, "\n[error: {msg}]");
            }
            _ => {}
        },
    )
    .await?;

    if is_tty {
        let _ = writeln!(
            stdout,
            "\n[turn complete: {:?}, {} steps]",
            result.stop_reason, result.steps
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_json(
    client: Arc<dyn ModelClient>,
    tools: &ToolRegistry,
    engine: &PermissionEngine,
    approver: &dyn Approver,
    recorder: &dyn Recorder,
    req: &ModelRequest,
    messages: &mut Vec<ModelMessage>,
    config: &TurnConfig,
    cwd: &std::path::Path,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    use serde_json::json;

    let result = run_turn(
        client.as_ref(),
        tools,
        engine,
        approver,
        recorder,
        req,
        messages,
        config,
        cwd,
        cancel,
        |ev| {
            let j = match ev {
                TurnEvent::TextDelta(t) => json!({"type": "text_delta", "text": t}),
                TurnEvent::ThinkingDelta(t) => json!({"type": "thinking_delta", "text": t}),
                TurnEvent::ToolCallBegin { call_id, tool } => json!({"type": "tool_call_begin", "call_id": call_id.to_string(), "tool": tool}),
                TurnEvent::ToolCallEnd { call_id } => json!({"type": "tool_call_end", "call_id": call_id.to_string()}),
                TurnEvent::ToolExecBegin { call_id } => json!({"type": "tool_exec_begin", "call_id": call_id.to_string()}),
                TurnEvent::ToolExecEnd { call_id, ok, summary } => json!({"type": "tool_exec_end", "call_id": call_id.to_string(), "ok": ok, "summary": summary}),
                TurnEvent::ApprovalRequest { call_id, kind, detail } => json!({"type": "approval_request", "call_id": call_id.to_string(), "kind": format!("{:?}", kind), "tool": detail.tool}),
                TurnEvent::ApprovalResolved { call_id, decision } => json!({"type": "approval_resolved", "call_id": call_id.to_string(), "decision": format!("{:?}", decision)}),
                TurnEvent::ModelMessageEnd { stop_reason } => json!({"type": "model_message_end", "stop_reason": format!("{:?}", stop_reason)}),
                TurnEvent::TurnComplete { stop_reason, steps } => json!({"type": "turn_complete", "stop_reason": format!("{:?}", stop_reason), "steps": steps}),
                TurnEvent::Error(msg) => json!({"type": "error", "message": msg}),
            };
            println!("{j}");
        },
    )
    .await?;

    println!(
        "{}",
        json!({"type": "turn_result", "stop_reason": format!("{:?}", result.stop_reason), "steps": result.steps})
    );
    Ok(())
}
