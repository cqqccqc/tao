//! ACP 适配层:JSON-RPC 2.0 over stdio(nd-json),被 Zed 等编辑器拉起。
//! v1:initialize/session/new/prompt/cancel + session/update 流式通知。
//! 自实现 JSON-RPC(不引入 agent_client_protocol 重依赖)。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use tao_core::config::{Config, LoadOpts};
use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::permissions::{ApprovalRequest, Approver, PermissionEngine};
use tao_core::providers::ModelClient;
use tao_core::providers::registry::resolve;
use tao_core::recorder::{JsonlRecorder, Recorder};
use tao_core::session::{TurnConfig, TurnEvent, run_turn};
use tao_core::tools::ToolRegistry;

/// ACP server(stdio JSON-RPC)。
pub struct AcpServer {
    session: Option<AcpSession>,
}

struct AcpSession {
    cwd: PathBuf,
    model: String,
    client: Arc<dyn ModelClient>,
    messages: Vec<ModelMessage>,
    engine: Arc<PermissionEngine>,
    recorder: Arc<JsonlRecorder>,
    tools: Arc<ToolRegistry>,
    hooks: tao_core::config::HooksConfig,
    session_id: String,
    cancel: Option<CancellationToken>,
}

/// ACP 审批器:v1 bypass(auto-approve)。
struct AcpApprover;
#[async_trait::async_trait]
impl Approver for AcpApprover {
    async fn request(&self, _req: ApprovalRequest) -> tao_protocol::op::ReviewDecision {
        tao_protocol::op::ReviewDecision::Approve
    }
}

impl Default for AcpServer {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpServer {
    pub fn new() -> Self {
        Self { session: None }
    }

    /// 主循环:读 stdin JSON-RPC → 处理 → 写 stdout。
    pub async fn run(mut self) -> Result<()> {
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
            let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let params = req.get("params").cloned().unwrap_or(Value::Null);

            let response = self.handle(method, &params).await;
            if let Some(id) = id {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": response,
                });
                let line = serde_json::to_string(&resp)? + "\n";
                stdout.write_all(line.as_bytes()).await?;
                stdout.flush().await?;
            }
        }
        Ok(())
    }

    async fn handle(&mut self, method: &str, params: &Value) -> Value {
        match method {
            "initialize" => json!({
                "protocolVersion": "0.1",
                "agent": { "name": "tao", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": {},
            }),
            "session/new" => self.session_new(params).await,
            "session/prompt" => self.session_prompt(params).await,
            "session/cancel" => self.session_cancel().await,
            _ => Value::Null,
        }
    }

    async fn session_new(&mut self, params: &Value) -> Value {
        let cwd_str = params.get("cwd").and_then(|v| v.as_str()).unwrap_or(".");
        let cwd = PathBuf::from(cwd_str);

        let config = match Config::load(&LoadOpts::default()) {
            Ok(c) => c,
            Err(e) => return json!({"error": format!("config 加载失败: {e}")}),
        };
        let (client, model) = match resolve(&config) {
            Ok(c) => c,
            Err(e) => return json!({"error": format!("provider 解析失败: {e}")}),
        };

        let mut tools = ToolRegistry::builtin();
        tao_mcp::load_mcp_tools(
            &mut tools,
            &config.mcp_servers,
            config.mcp_tool_budget,
            config.mcp_lazy,
        )
        .await;
        let tools = Arc::new(tools);

        let engine = Arc::new(PermissionEngine::new(
            tao_protocol::permission::PermissionMode::Bypass,
            config.permissions.rules.clone(),
        ));

        let (recorder, session_id) =
            match JsonlRecorder::create(&cwd, config.config_fingerprint(&model)) {
                Ok(r) => r,
                Err(e) => return json!({"error": format!("会话创建失败: {e}")}),
            };
        let recorder = Arc::new(recorder);

        self.session = Some(AcpSession {
            cwd,
            model,
            client,
            messages: Vec::new(),
            engine,
            recorder,
            tools,
            hooks: config.hooks.clone(),
            session_id: session_id.to_string(),
            cancel: None,
        });

        json!({"sessionId": session_id})
    }

    async fn session_prompt(&mut self, params: &Value) -> Value {
        let session = match self.session.as_mut() {
            Some(s) => s,
            None => return json!({"error": "no session"}),
        };

        let text = params
            .get("message")
            .and_then(|m| m.as_array())
            .and_then(|arr| arr.first())
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");

        let turn_id = uuid::Uuid::new_v4().to_string();
        session
            .recorder
            .record(tao_protocol::log::LogEvent::UserInput {
                content: vec![tao_protocol::content::Content::text(text)],
                turn_id: tao_protocol::ids::TurnId::new(turn_id.clone()),
            });
        session.messages.push(ModelMessage::User {
            content: vec![ModelContent::text(text)],
        });

        let mut system: Vec<SystemBlock> = Vec::new();
        if let Some(instr) = tao_core::instructions::load(&session.cwd) {
            system.push(SystemBlock {
                text: instr,
                cache_breakpoint: None,
            });
        }
        if let Some(p) =
            tao_core::skills::skills_prompt(&tao_core::skills::load_skills(&session.cwd))
        {
            system.push(SystemBlock {
                text: p,
                cache_breakpoint: None,
            });
        }
        system.push(SystemBlock {
            text: "你是 tao,一个 Rust 编写的 coding agent。".into(),
            cache_breakpoint: None,
        });

        let req = ModelRequest {
            model: session.model.clone(),
            system,
            messages: vec![],
            tools: session.tools.specs(),
            reasoning: None,
            max_output_tokens: 4096,
            temperature: None,
            metadata: RequestMeta {
                session_id: Some(session.session_id.clone()),
                turn_id: Some(turn_id.clone()),
            },
        };

        let cancel = CancellationToken::new();
        session.cancel = Some(cancel.clone());

        let hooks = session.hooks.clone();
        let config_turn = TurnConfig {
            max_steps: 100,
            trusted_projects: Vec::new(),
        };
        let sid = session.session_id.clone();

        let result = run_turn(
            session.client.as_ref(),
            &session.tools,
            &session.engine,
            &AcpApprover,
            session.recorder.as_ref(),
            &hooks,
            None,
            &req,
            &mut session.messages,
            &config_turn,
            &session.cwd,
            &cancel,
            move |ev| {
                let notification = match ev {
                    TurnEvent::TextDelta(t) => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": {
                                "type": "agent_message_chunk",
                                "content": { "type": "text", "text": t },
                            }
                        }
                    })),
                    TurnEvent::ToolCallBegin { tool, call_id } => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": {
                                "type": "tool_call",
                                "tool_name": tool,
                                "call_id": call_id.to_string(),
                                "state": "running",
                            }
                        }
                    })),
                    TurnEvent::ToolExecEnd {
                        ok,
                        summary,
                        call_id,
                    } => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": {
                                "type": "tool_call_update",
                                "call_id": call_id.to_string(),
                                "state": if ok { "completed" } else { "failed" },
                                "output": { "type": "text", "text": summary },
                            }
                        }
                    })),
                    TurnEvent::TurnComplete { stop_reason, .. } => Some(json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": sid,
                            "update": {
                                "type": "stop",
                                "stop_reason": format!("{stop_reason:?}").to_lowercase(),
                            }
                        }
                    })),
                    _ => None,
                };
                if let Some(n) = notification {
                    let line = serde_json::to_string(&n).unwrap_or_default() + "\n";
                    // on_event 是同步回调,用 std::io::Write 同步写 stdout
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(line.as_bytes());
                    let _ = stdout.flush();
                }
            },
        )
        .await;

        session.cancel = None;

        match result {
            Ok(r) => json!({"stopReason": format!("{:?}", r.stop_reason)}),
            Err(e) => json!({"error": e.to_string()}),
        }
    }

    async fn session_cancel(&mut self) -> Value {
        if let Some(session) = &self.session
            && let Some(cancel) = &session.cancel
        {
            cancel.cancel();
        }
        Value::Null
    }
}
