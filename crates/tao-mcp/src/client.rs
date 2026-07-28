//! MCP 客户端:JSON-RPC 2.0 over stdio(见 docs/design/tools.md §5)。
//!
//! v1:stdio only;不 HTTP/ToolSearch/预算/重连。spawn MCP server + initialize +
//! tools/list + tools/call。工具映射 `mcp__server__tool`。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use tao_core::config::McpServerConfig;
use tao_core::model::ToolSpec;
use tao_core::permissions::PermissionKey;
use tao_core::tools::{Tool, ToolCtx, ToolError, ToolOutput, ToolRegistry};

/// MCP 工具信息(tools/list 结果)。
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// MCP 工具调用结果。
#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub content: String,
    pub is_error: bool,
}

/// MCP 客户端(JSON-RPC over stdio)。
pub struct McpClient {
    #[allow(dead_code)]
    process: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    request_id: AtomicU64,
}

impl McpClient {
    /// spawn MCP server + initialize。
    pub async fn spawn(config: &McpServerConfig) -> Result<Self> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        let mut process = cmd
            .spawn()
            .context(format!("启动 MCP server 失败: {}", config.command))?;
        let stdin = process.stdin.take().context("no stdin")?;
        let stdout = process.stdout.take().context("no stdout")?;
        let mut client = Self {
            process,
            stdin,
            stdout: BufReader::new(stdout),
            request_id: AtomicU64::new(0),
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req)? + "\n";
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        loop {
            let mut buf = String::new();
            let n = self.stdout.read_line(&mut buf).await?;
            if n == 0 {
                anyhow::bail!("MCP server 关闭连接");
            }
            let resp: Value = serde_json::from_str(buf.trim())?;
            if resp.get("id") == Some(&json!(id)) {
                if let Some(err) = resp.get("error") {
                    anyhow::bail!("MCP 错误: {err}");
                }
                return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
            }
            // notification(无 id),跳过
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req)? + "\n";
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn initialize(&mut self) -> Result<()> {
        let _ = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "tao", "version": "0.1.0" },
                }),
            )
            .await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    pub async fn list_tools(&mut self) -> Result<Vec<McpToolInfo>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools
            .into_iter()
            .map(|t| McpToolInfo {
                name: t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                description: t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_schema: t.get("inputSchema").cloned().unwrap_or(json!({})),
            })
            .collect())
    }

    pub async fn call_tool(&mut self, name: &str, args: &Value) -> Result<McpToolResult> {
        let result = self
            .request(
                "tools/call",
                json!({
                    "name": name,
                    "arguments": args,
                }),
            )
            .await?;
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let content = result
            .get("content")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let text: String = content
            .iter()
            .filter_map(|c| {
                if c.get("type").and_then(|v| v.as_str()) == Some("text") {
                    c.get("text").and_then(|v| v.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(McpToolResult {
            content: text,
            is_error,
        })
    }
}

/// MCP 工具(Tool trait,`mcp__server__tool`)。
pub struct McpTool {
    server: String,
    name: String,
    description: String,
    schema: Value,
    client: Arc<Mutex<McpClient>>,
}

impl McpTool {
    pub fn new(server: &str, info: &McpToolInfo, client: Arc<Mutex<McpClient>>) -> Self {
        Self {
            server: server.to_string(),
            name: info.name.clone(),
            description: info.description.clone(),
            schema: info.input_schema.clone(),
            client,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: format!("mcp__{}__{}", self.server, self.name),
            description: self.description.clone(),
            schema: self.schema.clone(),
        }
    }

    fn permission_key(&self, _args: &Value, _cwd: &Path) -> Option<PermissionKey> {
        None
    }

    async fn call(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let mut client = self.client.lock().await;
        let result = client
            .call_tool(&self.name, args)
            .await
            .map_err(|e| ToolError::Failed(format!("MCP 调用失败: {e}")))?;
        Ok(ToolOutput {
            content: result.content,
            is_error: result.is_error,
        })
    }
}

/// 加载所有 MCP server + 注册工具到 ToolRegistry。启动失败 skip + warn(不阻断)。
pub async fn load_mcp_tools(
    registry: &mut ToolRegistry,
    servers: &HashMap<String, McpServerConfig>,
) {
    for (name, config) in servers {
        match McpClient::spawn(config).await {
            Ok(mut client) => match client.list_tools().await {
                Ok(tools) => {
                    let client = Arc::new(Mutex::new(client));
                    for info in &tools {
                        let tool = McpTool::new(name, info, client.clone());
                        registry.register(Arc::new(tool));
                    }
                    tracing::info!("MCP server {name} 加载 {} 个工具", tools.len());
                }
                Err(e) => {
                    tracing::warn!("MCP server {name} list_tools 失败: {e}");
                }
            },
            Err(e) => {
                tracing::warn!("MCP server {name} 启动失败(skip): {e}");
            }
        }
    }
}
