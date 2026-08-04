//! MCP server:把 tao 自身暴露为 MCP server(stdio JSON-RPC 2.0)。
//! 见 docs/design/tools.md §5。
//!
//! 暴露三工具:
//! - `list_sessions`:列出本机会话(fork 树 + title + 消息数 + 更新时间)
//! - `send_message`:发一条消息跑一个 turn,返回 agent 的最终文本回复
//! - `read_session`:读取指定会话的 transcript
//!
//! v1:stdio only;审批 auto-approve(`PermissionMode::Bypass`)——信任本地驱动方,
//! 仅用于受信任的自动化(等价 `tao exec --on-ask approve`,但常驻 + 多 turn fork)。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use tao_core::config::{Config, LoadOpts};
use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::permissions::{ApprovalRequest, Approver, PermissionEngine};
use tao_core::providers::registry::resolve;
use tao_core::recorder::{JsonlRecorder, Recorder, session_dir, session_file_path};
use tao_core::replay::replay;
use tao_core::session::{TurnConfig, TurnEvent, run_turn};
use tao_core::tools::ToolRegistry;
use tao_protocol::content::Content;
use tao_protocol::ids::{SessionId, TurnId};
use tao_protocol::log::LogEvent;
use tao_protocol::op::ReviewDecision;
use tao_protocol::permission::PermissionMode;

/// JSON-RPC 错误码:方法/参数/工具未找到等统一用 -32603(internal error)。
const ERR_INTERNAL: i64 = -32603;

/// MCP 协议版本(与 client.rs 保持一致)。
const PROTOCOL_VERSION: &str = "2024-11-05";

/// mcp-serve 审批器:v1 auto-approve(bypass)。信任本地驱动方。
struct AutoApprover;
#[async_trait]
impl Approver for AutoApprover {
    async fn request(&self, _req: ApprovalRequest) -> ReviewDecision {
        ReviewDecision::Approve
    }
}

/// `tao mcp-serve` 入口:stdio JSON-RPC 主循环。
///
/// stderr 留日志(由调用方配 tracing),stdout 只走 JSON-RPC。读到 EOF 即退出。
pub async fn run_server() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut buf = String::new();

    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            break;
        }
        let line = buf.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.get("id").cloned();
        let method = req
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let result = handle(&method, &params).await;
        // notification(无 id)无响应
        if let Some(id) = id {
            let resp = match result {
                Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
                Err(msg) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": ERR_INTERNAL, "message": msg }
                }),
            };
            let line = serde_json::to_string(&resp)? + "\n";
            stdout.write_all(line.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

async fn handle(method: &str, params: &Value) -> Result<Value, String> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": { "name": "tao", "version": "0.1.0" },
            "capabilities": { "tools": {} },
        })),
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => Ok(tools_list()),
        "tools/call" => tools_call(params).await,
        _ => Err(format!("未知方法: {method}")),
    }
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "list_sessions",
                "description": "列出本机 tao 会话(fork 树、标题、消息数、更新时间、体积)。默认列当前工作目录下的会话。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cwd": { "type": "string", "description": "会话所在工作目录(默认服务进程当前目录)" }
                    },
                    "additionalProperties": false,
                },
            },
            {
                "name": "send_message",
                "description": "向 tao 发一条消息并跑一个 turn,返回 agent 的最终文本回复。可选 session_id 从已有会话 fork(继承历史)。权限模式 bypass,工具调用自动批准——仅用于受信任的本地自动化。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "message": { "type": "string", "description": "用户消息" },
                        "cwd": { "type": "string", "description": "工作目录(默认当前目录)" },
                        "session_id": { "type": "string", "description": "可选:从该会话 fork(继承历史),不传则新建会话" },
                    },
                    "required": ["message"],
                    "additionalProperties": false,
                },
            },
            {
                "name": "read_session",
                "description": "读取指定会话的完整 transcript(用户/助手/工具结果消息)。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": { "type": "string", "description": "会话 id" },
                        "cwd": { "type": "string", "description": "工作目录(默认当前目录)" },
                    },
                    "required": ["session_id"],
                    "additionalProperties": false,
                },
            },
        ]
    })
}

async fn tools_call(params: &Value) -> Result<Value, String> {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match name {
        "list_sessions" => tool_list_sessions(&args),
        "send_message" => tool_send_message(&args).await,
        "read_session" => tool_read_session(&args),
        other => Err(format!("未知工具: {other}")),
    }
}

// ---- 工具实现 ----

/// list_sessions:扫描会话目录,replay 每个 JSONL,返回摘要数组。
fn tool_list_sessions(args: &Value) -> Result<Value, String> {
    let cwd = parse_cwd(args);
    let dir = session_dir(&cwd).ok_or("HOME 未设置,无法定位会话目录")?;
    if !dir.exists() {
        return Ok(text_result("[]".into()));
    }

    let mut entries: Vec<Value> = Vec::new();
    for e in std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let path = e.path();
        if !path.extension().is_some_and(|x| x == "jsonl") {
            continue;
        }
        let meta = e.metadata().ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let Ok(state) = replay(&path) else {
            continue;
        };
        let title = state.title.clone().unwrap_or_else(|| {
            first_user_text(&state.messages).unwrap_or_else(|| "(无标题)".into())
        });
        entries.push(json!({
            "id": state.id.as_ref(),
            "parent": state.parent.as_ref().map(|p| p.as_ref()),
            "title": title,
            "messages": state.messages.len(),
            "updated_at_ms": mtime,
            "size_bytes": size,
        }));
    }
    // 按 updated_at_ms 倒序
    entries.sort_by(|a, b| {
        b.get("updated_at_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .cmp(&a.get("updated_at_ms").and_then(|v| v.as_u64()).unwrap_or(0))
    });

    Ok(text_result(
        serde_json::to_string_pretty(&entries).unwrap_or_default(),
    ))
}

/// read_session:replay 指定会话,返回消息数组。
fn tool_read_session(args: &Value) -> Result<Value, String> {
    let cwd = parse_cwd(args);
    let id_str = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or("缺少 session_id")?;
    let sid = SessionId::new(id_str.to_string());
    let path = session_file_path(&cwd, &sid).ok_or("HOME 未设置,无法定位会话日志")?;
    let state = replay(&path).map_err(|e| e.to_string())?;

    let arr: Vec<Value> = state
        .messages
        .iter()
        .map(|m| {
            let (role, text) = message_role_text(m);
            json!({ "role": role, "text": text })
        })
        .collect();
    Ok(text_result(
        serde_json::to_string_pretty(&arr).unwrap_or_default(),
    ))
}

/// send_message:跑一个 turn,返回 agent 最终文本回复。
///
/// 接线参考 tao-exec / tao-acp:Config + provider + tools + system + recorder +
/// auto-compact + run_turn。权限 bypass + auto-approve(信任本地驱动方)。
async fn tool_send_message(args: &Value) -> Result<Value, String> {
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or("缺少 message")?;
    let cwd = parse_cwd(args);

    let config = Config::load(&LoadOpts::default()).map_err(|e| format!("config 加载失败: {e}"))?;
    let (client, model) = resolve(&config).map_err(|e| format!("provider 解析失败: {e}"))?;

    let mut tools = ToolRegistry::builtin();
    crate::load_mcp_tools(
        &mut tools,
        &config.mcp_servers,
        config.mcp_tool_budget,
        config.mcp_lazy,
    )
    .await;
    let tools = Arc::new(tools);

    let engine = Arc::new(PermissionEngine::new(
        PermissionMode::Bypass,
        config.permissions.rules.clone(),
    ));

    // 新建会话 或 从 session_id fork(继承历史)
    let (recorder, mut messages, session_id) = match args.get("session_id").and_then(|v| v.as_str())
    {
        Some(id_str) => {
            let parent = SessionId::new(id_str.to_string());
            let fp = config.config_fingerprint(&model);
            let (r, new_id) = JsonlRecorder::create_fork(&cwd, &parent, fp)
                .map_err(|e| format!("创建 fork 会话失败: {e}"))?;
            let path = session_file_path(&cwd, &parent).ok_or("HOME 未设置")?;
            let state = replay(&path).map_err(|e| format!("重放会话失败: {e}"))?;
            (r, state.messages, new_id)
        }
        None => {
            let fp = config.config_fingerprint(&model);
            let (r, id) =
                JsonlRecorder::create(&cwd, fp).map_err(|e| format!("创建会话失败: {e}"))?;
            (r, Vec::new(), id)
        }
    };
    let recorder = Arc::new(recorder);

    // system prompt:指令文件 + 技能 + 兜底
    let mut system: Vec<SystemBlock> = Vec::new();
    if let Some(instr) = tao_core::instructions::load(&cwd) {
        system.push(SystemBlock {
            text: instr,
            cache_breakpoint: None,
        });
    }
    if let Some(p) = tao_core::skills::skills_prompt(&tao_core::skills::load_skills(&cwd)) {
        system.push(SystemBlock {
            text: p,
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

    let turn_id = TurnId::new(uuid::Uuid::new_v4().to_string());
    recorder.record(LogEvent::UserInput {
        content: vec![Content::text(message)],
        turn_id: turn_id.clone(),
    });
    messages.push(ModelMessage::User {
        content: vec![ModelContent::text(message)],
    });

    // auto compact
    let threshold = (client.context_window(&model) as f32 * config.auto_compact_at) as u64;
    if tao_core::approx_tokens(&messages) > threshold {
        let cm = config.small_model.as_deref().unwrap_or(&model);
        messages = tao_core::compact(
            client.as_ref(),
            cm,
            &messages,
            tao_core::DEFAULT_KEEP_LAST,
            recorder.as_ref(),
        )
        .await
        .map_err(|e| format!("上下文压缩失败: {e}"))?;
    }

    let req = ModelRequest {
        model,
        system,
        messages: vec![],
        tools: tools.specs(),
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
        trusted_projects: config.trusted_projects.clone(),
    };
    let cancel = CancellationToken::new();
    let shadow = tao_core::ShadowRepo::init(&cwd).ok();

    // 累积流式文本作为回复;空则回退取最后一条 assistant 文本
    let mut agent_text = String::new();
    let _result = run_turn(
        client.as_ref(),
        &tools,
        &engine,
        &AutoApprover,
        recorder.as_ref(),
        &config.hooks,
        shadow.as_ref(),
        &req,
        &mut messages,
        &config_turn,
        &cwd,
        &cancel,
        |ev| {
            if let TurnEvent::TextDelta(t) = ev {
                agent_text.push_str(&t);
            }
        },
    )
    .await
    .map_err(|e| format!("turn 失败: {e}"))?;

    let reply = if !agent_text.is_empty() {
        agent_text
    } else {
        messages
            .iter()
            .rev()
            .find_map(|m| match m {
                ModelMessage::Assistant { content } => content.iter().find_map(|c| match c {
                    ModelContent::Text(t) => Some(t.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_default()
    };

    Ok(text_result(reply))
}

// ---- 辅助 ----

fn parse_cwd(args: &Value) -> PathBuf {
    args.get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// 构造 MCP tools/call 成功结果(text content)。
fn text_result(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

fn first_user_text(messages: &[ModelMessage]) -> Option<String> {
    messages.iter().find_map(|m| match m {
        ModelMessage::User { content } => content.iter().find_map(|c| match c {
            ModelContent::Text(t) => Some(t.clone()),
            _ => None,
        }),
        _ => None,
    })
}

/// 返回 (角色, 可读文本)。工具结果带 ✓/✗ 前缀便于 transcript 阅读。
fn message_role_text(m: &ModelMessage) -> (&'static str, String) {
    match m {
        ModelMessage::User { content } => ("user", collect_text(content)),
        ModelMessage::Assistant { content } => ("assistant", collect_text(content)),
        ModelMessage::ToolResult {
            content, is_error, ..
        } => (
            "tool",
            format!(
                "[{}] {}",
                if *is_error { "✗" } else { "✓" },
                collect_text(content)
            ),
        ),
    }
}

fn collect_text(content: &[ModelContent]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ModelContent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_list_has_three_tools() {
        let v = tools_list();
        let tools = v
            .get("tools")
            .and_then(|t| t.as_array())
            .expect("tools 数组");
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(|n| n.as_str()).unwrap_or(""))
            .collect();
        assert_eq!(names, ["list_sessions", "send_message", "read_session"]);
        // 每个工具都有 inputSchema
        for t in tools {
            assert!(t.get("inputSchema").is_some(), "缺 inputSchema");
        }
    }

    #[test]
    fn text_result_shape() {
        let v = text_result("hi".into());
        assert_eq!(v["isError"], json!(false));
        assert_eq!(v["content"][0]["type"], json!("text"));
        assert_eq!(v["content"][0]["text"], json!("hi"));
    }

    #[test]
    fn parse_cwd_defaults_to_current() {
        let v = json!({});
        let cwd = parse_cwd(&v);
        assert!(cwd.is_absolute(), "默认 cwd 应是绝对路径");
    }

    #[test]
    fn parse_cwd_uses_arg() {
        let v = json!({ "cwd": "/tmp/tao-test" });
        assert_eq!(parse_cwd(&v), PathBuf::from("/tmp/tao-test"));
    }
}
