//! # tao-exec
//!
//! headless 单次执行:`tao exec "fix the tests"`。M1 实现。
//! Ask 审批默认 deny(`--on-ask approve` 可改),保证脚本可预期。

use std::path::PathBuf;
use std::sync::Arc;

use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::providers::ModelClient;
use tao_core::providers::anthropic::AnthropicClient;
use tao_core::providers::openai_chat::OpenAiChatClient;
use tao_core::providers::openai_responses::OpenAiResponsesClient;
use tao_core::session::{TurnConfig, TurnEvent, run_turn};
use tao_core::tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

/// exec 的运行选项。
pub struct ExecOpts {
    pub prompt: String,
    pub cwd: PathBuf,
    /// 覆盖默认模型(从环境推断)。
    pub model: Option<String>,
    pub json: bool,
}

/// 从环境变量推断 provider + model。
/// 优先级:ANTHROPIC_API_KEY > OPENAI_API_KEY(配合 OPENAI_BASE_URL 判断 chat vs responses)。
fn resolve_provider() -> anyhow::Result<(Arc<dyn ModelClient>, String)> {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY")
        && !key.is_empty()
    {
        let base = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());
        let model = std::env::var("TAO_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());
        let client = AnthropicClient::with_api_key(base, key);
        return Ok((Arc::new(client), model));
    }

    if let Ok(key) = std::env::var("OPENAI_API_KEY")
        && !key.is_empty()
    {
        let base =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com".into());
        let model = std::env::var("TAO_MODEL").unwrap_or_else(|_| "gpt-4o".into());
        // OPENAI_BASE_URL 非默认 → 兼容生态(DeepSeek/Qwen/Kimi 等)走 chat。
        // 默认 openai.com → 走 responses(OpenAI 首选)。
        let is_default_openai = base.trim_end_matches('/').ends_with("api.openai.com");
        let client: Arc<dyn ModelClient> = if is_default_openai {
            Arc::new(OpenAiResponsesClient::new(base, key))
        } else {
            Arc::new(OpenAiChatClient::new(base, key))
        };
        return Ok((client, model));
    }

    anyhow::bail!(
        "未配置 API key:请设置 ANTHROPIC_API_KEY 或 OPENAI_API_KEY 环境变量。\n\
         详见 docs/design/config.md §3"
    )
}

pub async fn run(opts: ExecOpts) -> anyhow::Result<()> {
    let (client, default_model) = resolve_provider()?;
    let model = opts.model.unwrap_or(default_model);

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
        messages: vec![], // run_turn 用 messages 参数
        tools: vec![],
        reasoning: None,
        max_output_tokens: 4096,
        temperature: None,
        metadata: RequestMeta::default(),
    };

    let mut messages = messages;
    let config = TurnConfig::default();
    let cancel = CancellationToken::new();

    // ctrl-c → cancel
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
            &config,
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
            &config,
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
