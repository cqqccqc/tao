//! # tao-exec
//!
//! headless 单次执行:`tao exec "fix the tests"`。M1 实现。
//! Ask 审批默认 deny(`--on-ask approve` 可改),保证脚本可预期。

use std::path::PathBuf;
use std::sync::Arc;

use tao_core::config::{Config, LoadOpts};
use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::providers::ModelClient;
use tao_core::providers::registry::resolve;
use tao_core::session::{TurnConfig, TurnEvent, run_turn};
use tao_core::tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

/// exec 的运行选项。
pub struct ExecOpts {
    pub prompt: String,
    pub cwd: PathBuf,
    /// 覆盖默认模型(从 config 推断)。
    pub model: Option<String>,
    pub json: bool,
    pub load_opts: LoadOpts,
}

pub async fn run(opts: ExecOpts) -> anyhow::Result<()> {
    let config = Config::load(&opts.load_opts)?;
    let (client, model) = resolve(&config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let model = opts.model.unwrap_or(model);

    let tools = ToolRegistry::builtin();
    let system = vec![SystemBlock {
        text: "你是 tao,一个 Rust 编写的 coding agent。\
               你可以通过 Bash / Read / Write 工具帮助用户完成任务。\
               优先用最少的步骤解决问题。"
            .into(),
        cache_breakpoint: None,
    }];

    let messages = vec![ModelMessage::User {
        content: vec![ModelContent::text(opts.prompt.clone())],
    }];

    let req = ModelRequest {
        model,
        system,
        messages: vec![],
        tools: vec![],
        reasoning: config.reasoning_effort,
        max_output_tokens: 4096,
        temperature: None,
        metadata: RequestMeta::default(),
    };

    let mut messages = messages;
    let config_turn = TurnConfig {
        max_steps: config.max_turn_steps,
    };
    let cancel = CancellationToken::new();

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
            &req,
            &mut messages,
            &config_turn,
            &opts.cwd,
            &cancel,
        )
        .await
    } else {
        run_text(
            client,
            &tools,
            &req,
            &mut messages,
            &config_turn,
            &opts.cwd,
            &cancel,
        )
        .await
    }
}

async fn run_text(
    client: Arc<dyn ModelClient>,
    tools: &ToolRegistry,
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

async fn run_json(
    client: Arc<dyn ModelClient>,
    tools: &ToolRegistry,
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
        req,
        messages,
        config,
        cwd,
        cancel,
        |ev| {
            let json = match ev {
                TurnEvent::TextDelta(t) => json!({"type": "text_delta", "text": t}),
                TurnEvent::ThinkingDelta(t) => json!({"type": "thinking_delta", "text": t}),
                TurnEvent::ToolCallBegin { call_id, tool } => json!({"type": "tool_call_begin", "call_id": call_id.to_string(), "tool": tool}),
                TurnEvent::ToolCallEnd { call_id } => json!({"type": "tool_call_end", "call_id": call_id.to_string()}),
                TurnEvent::ToolExecBegin { call_id } => json!({"type": "tool_exec_begin", "call_id": call_id.to_string()}),
                TurnEvent::ToolExecEnd { call_id, ok, summary } => json!({"type": "tool_exec_end", "call_id": call_id.to_string(), "ok": ok, "summary": summary}),
                TurnEvent::ModelMessageEnd { stop_reason } => json!({"type": "model_message_end", "stop_reason": format!("{:?}", stop_reason)}),
                TurnEvent::TurnComplete { stop_reason, steps } => json!({"type": "turn_complete", "stop_reason": format!("{:?}", stop_reason), "steps": steps}),
                TurnEvent::Error(msg) => json!({"type": "error", "message": msg}),
            };
            println!("{json}");
        },
    )
    .await?;

    println!(
        "{}",
        json!({"type": "turn_result", "stop_reason": format!("{:?}", result.stop_reason), "steps": result.steps})
    );
    Ok(())
}
