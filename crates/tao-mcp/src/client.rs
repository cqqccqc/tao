//! MCP 客户端:JSON-RPC 2.0 over stdio 或 HTTP(见 docs/design/tools.md §5)。
//!
//! v1:stdio + HTTP(每个 request 一个 POST,不 SSE)。
//! - F4 惰性启动(`mcp_lazy=false`):load 时仅短暂 spawn 拿 tool list(schema 必需),
//!   随后 drop;持久 client cell 留空,首次 call 时按需 spawn。
//!   dispatcher 模式(`mcp_lazy=true`)彻底免除此"list spawn":只注册
//!   `mcp__toolsearch`(发现)+ `mcp__call`(调用)两个元工具,模型先 search 再 call。
//! - F3 重连(仅半惰性模式):call 失败 → reset cell → 重新 spawn 一次 → 重试。
//! - F2 预算(`mcp_lazy=false` 时生效):`mcp_tool_budget` 超出则折叠(只注册前 budget 个 + warn),
//!   并额外注册 `mcp__toolsearch` 元工具(call 时遍历 servers list + 模糊匹配 query)。
//!
//! 工具映射 `mcp__server__tool`。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use tao_core::config::{McpServerConfig, McpTransport};
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

/// 底层传输:stdio(子进程管道)或 HTTP(每个 JSON-RPC 一个 POST)。
#[allow(clippy::large_enum_variant)]
enum Transport {
    Stdio {
        /// 仅保持存活;drop 时 kill_on_drop 终止子进程。
        #[allow(dead_code)]
        process: Child,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
    },
    Http {
        client: reqwest::Client,
        url: String,
    },
}

/// MCP 客户端(JSON-RPC over stdio 或 HTTP)。
pub struct McpClient {
    transport: Transport,
    request_id: AtomicU64,
}

impl McpClient {
    /// spawn MCP server(stdio)或连接 HTTP endpoint,initialize。
    pub async fn spawn(config: &McpServerConfig) -> Result<Self> {
        let transport = match &config.transport {
            McpTransport::Stdio => {
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
                Transport::Stdio {
                    process,
                    stdin,
                    stdout: BufReader::new(stdout),
                }
            }
            McpTransport::Http { url } => Transport::Http {
                client: reqwest::Client::builder()
                    .build()
                    .context("构建 reqwest client 失败")?,
                url: url.clone(),
            },
        };
        let mut client = Self {
            transport,
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
        match &mut self.transport {
            Transport::Stdio { stdin, stdout, .. } => {
                let line = serde_json::to_string(&req)? + "\n";
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
                loop {
                    let mut buf = String::new();
                    let n = stdout.read_line(&mut buf).await?;
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
            Transport::Http { client, url } => {
                let resp = client
                    .post(url.as_str())
                    .json(&req)
                    .send()
                    .await?
                    .error_for_status()
                    .context(format!("HTTP MCP request {method} 失败"))?
                    .json::<Value>()
                    .await?;
                if let Some(err) = resp.get("error") {
                    anyhow::bail!("MCP 错误: {err}");
                }
                Ok(resp.get("result").cloned().unwrap_or(Value::Null))
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match &mut self.transport {
            Transport::Stdio { stdin, .. } => {
                let line = serde_json::to_string(&req)? + "\n";
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
            }
            Transport::Http { client, url } => {
                // HTTP notify:同样 POST(无 id,不期望响应;丢弃响应体)。
                let _ = client.post(url.as_str()).json(&req).send().await?;
            }
        }
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
///
/// - F4 惰性启动:为拿 schema 必须 spawn 一次 list_tools,但该 client 随即 drop
///   (kill_on_drop 终止子进程);持久 client cell 留空,首次 call 时按需 spawn。
///   TODO:dispatcher/ToolSearch 模式可彻底免除此"list spawn"(见 docs/design/tools.md)。
/// - F3 重连:call 失败 → reset cell → 重新 spawn 一次 → 重试(最多一次重连)。
///   同一 server 的所有工具共享同一个 cell(`Arc`)。
pub struct McpTool {
    server: String,
    name: String,
    description: String,
    schema: Value,
    config: McpServerConfig,
    /// `None` = 未 spawn / 失败后已重置;`Some` = 活跃客户端。
    client: Arc<Mutex<Option<Arc<Mutex<McpClient>>>>>,
}

impl McpTool {
    pub fn new(
        server: &str,
        info: &McpToolInfo,
        config: McpServerConfig,
        client: Arc<Mutex<Option<Arc<Mutex<McpClient>>>>>,
    ) -> Self {
        Self {
            server: server.to_string(),
            name: info.name.clone(),
            description: info.description.clone(),
            schema: info.input_schema.clone(),
            config,
            client,
        }
    }

    /// 按需 spawn(若 cell 为空)。返回内层 client Arc。
    async fn ensure_client(&self) -> Result<Arc<Mutex<McpClient>>, anyhow::Error> {
        let mut slot = self.client.lock().await;
        if let Some(c) = slot.as_ref() {
            return Ok(c.clone());
        }
        let c = McpClient::spawn(&self.config).await?;
        let c = Arc::new(Mutex::new(c));
        *slot = Some(c.clone());
        Ok(c)
    }

    /// 重置 cell(`None`),下次 `ensure_client` 会重新 spawn。
    async fn reset_client(&self) {
        let mut slot = self.client.lock().await;
        *slot = None;
    }

    /// spawn(若需要)+ 调用工具。
    async fn try_call(&self, args: &Value) -> Result<McpToolResult, anyhow::Error> {
        let client = self.ensure_client().await?;
        let mut c = client.lock().await;
        c.call_tool(&self.name, args).await
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
        // 第一次尝试(惰性 spawn 若需要)
        match self.try_call(args).await {
            Ok(result) => Ok(ToolOutput {
                content: result.content,
                is_error: result.is_error,
            }),
            Err(first_err) => {
                // F3 重连:重置 cell + 重新 spawn 一次 + 重试
                tracing::warn!(
                    "MCP {}/{} 调用失败,尝试重连: {}",
                    self.server,
                    self.name,
                    first_err
                );
                self.reset_client().await;
                match self.try_call(args).await {
                    Ok(result) => Ok(ToolOutput {
                        content: result.content,
                        is_error: result.is_error,
                    }),
                    Err(second_err) => Err(ToolError::Failed(format!(
                        "MCP 调用失败(重连后仍失败): {second_err}"
                    ))),
                }
            }
        }
    }
}

/// ToolSearch 元工具(`mcp__toolsearch`):超 budget 折叠后,按 query 关键词
/// 遍历所有已 config 的 MCP server(spawn 一次 list_tools),模糊匹配 name/description,
/// 返回候选工具 spec(`mcp__server__tool — desc | schema`)。简化:call 时遍历 + filter。
///
/// TODO(#4):与 dispatcher 模式合流后可免 list-spawn(动态 schema)。
pub struct ToolSearchTool {
    servers: Arc<HashMap<String, McpServerConfig>>,
}

impl ToolSearchTool {
    pub fn new(servers: Arc<HashMap<String, McpServerConfig>>) -> Self {
        Self { servers }
    }
}

#[async_trait::async_trait]
impl Tool for ToolSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "mcp__toolsearch".into(),
            description: "搜索已配置(可能被折叠)的 MCP 工具。按 query 模糊匹配 \
                          工具名/描述,返回候选 spec(含 server 名 + input schema)。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "搜索关键词(模糊匹配工具名/描述)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn permission_key(&self, _args: &Value, _cwd: &Path) -> Option<PermissionKey> {
        None
    }

    async fn call(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let q = query.to_lowercase();
        let mut hits: Vec<String> = Vec::new();
        for (server, config) in self.servers.iter() {
            // 简化:call 时 spawn 一次拿 list_tools,filter by query。
            let tools = match McpClient::spawn(config).await {
                Ok(mut client) => match client.list_tools().await {
                    Ok(t) => t,
                    Err(e) => {
                        hits.push(format!("[{server}: list_tools 失败: {e}]"));
                        continue;
                    }
                },
                Err(e) => {
                    hits.push(format!("[{server}: spawn 失败: {e}]"));
                    continue;
                }
            };
            for t in tools {
                let name_l = t.name.to_lowercase();
                let desc_l = t.description.to_lowercase();
                if q.is_empty() || name_l.contains(&q) || desc_l.contains(&q) {
                    hits.push(format!(
                        "mcp__{server}__{} — {}\n  schema: {}",
                        t.name, t.description, t.input_schema
                    ));
                }
            }
        }
        let content = if hits.is_empty() {
            format!("未匹配到 MCP 工具(query={query})")
        } else {
            hits.join("\n")
        };
        Ok(ToolOutput::ok(content))
    }
}

/// MCP dispatcher 工具(`mcp__call`):lazy 模式下唯一调用入口。
///
/// 模型先用 `mcp__toolsearch` 发现 server/tool 名,再以本工具按名调用:
/// `mcp__call { server, tool, arguments }`。每次调用 spawn 一次 McpClient,
/// 调用后即 drop(无持久 cell,无重连)。彻底免启动 list-spawn。
pub struct McpCallTool {
    servers: Arc<HashMap<String, McpServerConfig>>,
}

impl McpCallTool {
    pub fn new(servers: Arc<HashMap<String, McpServerConfig>>) -> Self {
        Self { servers }
    }
}

#[async_trait::async_trait]
impl Tool for McpCallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "mcp__call".into(),
            description: "动态调用 MCP 工具(先用 mcp__toolsearch 发现 server/tool 名,\
                          再以此工具按名调用)。每次 spawn 一次 MCP server,调用后即释放。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "MCP server 名(见 mcp_servers 配置 / toolsearch 结果)"
                    },
                    "tool": {
                        "type": "string",
                        "description": "该 server 上的工具名(toolsearch 返回的去掉 mcp__server__ 前缀后的部分)"
                    },
                    "arguments": {
                        "type": "object",
                        "description": "工具参数(JSON 对象,schema 见 toolsearch 结果)",
                        "additionalProperties": true
                    }
                },
                "required": ["server", "tool"]
            }),
        }
    }

    fn permission_key(&self, _args: &Value, _cwd: &Path) -> Option<PermissionKey> {
        None
    }

    async fn call(&self, args: &Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let server = args.get("server").and_then(|v| v.as_str()).unwrap_or("");
        let tool = args.get("tool").and_then(|v| v.as_str()).unwrap_or("");
        if server.is_empty() || tool.is_empty() {
            return Ok(ToolOutput::error(
                "缺少 server 或 tool 参数(需同时提供 server 与 tool)",
            ));
        }
        let arguments = args.get("arguments").cloned().unwrap_or(json!({}));
        let config = match self.servers.get(server) {
            Some(c) => c.clone(),
            None => {
                let available: Vec<&str> = self.servers.keys().map(|s| s.as_str()).collect();
                return Ok(ToolOutput::error(format!(
                    "未知 MCP server: {server}(可用: {})",
                    available.join(", ")
                )));
            }
        };
        let mut client = match McpClient::spawn(&config).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "MCP server {server} 启动失败: {e}"
                )));
            }
        };
        let result = match client.call_tool(tool, &arguments).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "MCP {server}/{tool} 调用失败: {e}"
                )));
            }
        };
        Ok(ToolOutput {
            content: result.content,
            is_error: result.is_error,
        })
    }
}

/// 加载所有 MCP server + 注册工具到 ToolRegistry。启动/list 失败 skip + warn(不阻断)。
///
/// - `lazy=true`(dispatcher 模式):**不 spawn 任何 server**(免启动 list-spawn),
///   只注册 `mcp__toolsearch`(发现)+ `mcp__call`(调用)两个元工具。模型先 search
///   发现 server/tool 名,再以 mcp__call 按名动态调用。budget 在此模式下不生效。
/// - `lazy=false`(半惰性,F4):为拿 schema 必须 spawn 一次 list_tools,随后 drop client;
///   cell 留空,首次 call 时按需 spawn。
/// - F2 预算(仅 `lazy=false`):`budget != 0` 时,只注册前 budget 个工具,超出折叠 + warn,
///   并额外注册 `mcp__toolsearch` 元工具(按 query 遍历 servers 模糊发现)。
///   `budget == 0` = 不限制(不注册 toolsearch,无折叠)。
pub async fn load_mcp_tools(
    registry: &mut ToolRegistry,
    servers: &HashMap<String, McpServerConfig>,
    budget: usize,
    lazy: bool,
) {
    if lazy {
        // dispatcher 模式:彻底免启动 spawn。不 list 任何 server,
        // 只注册 ToolSearch(发现)+ McpCall(调用)两个元工具。
        let servers = Arc::new(servers.clone());
        registry.register(Arc::new(ToolSearchTool::new(servers.clone())));
        registry.register(Arc::new(McpCallTool::new(servers)));
        return;
    }
    let mut total: usize = 0;
    for (name, config) in servers {
        // 惰性:仅短暂 spawn 拿 tool list(schema 必需),随后 drop。
        let tools = match McpClient::spawn(config).await {
            Ok(mut client) => match client.list_tools().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("MCP server {name} list_tools 失败: {e}");
                    continue;
                }
            },
            Err(e) => {
                tracing::warn!("MCP server {name} 启动失败(skip): {e}");
                continue;
            }
        };
        // client 在此 drop(kill_on_drop 终止子进程);cell 留空,首次 call 重 spawn。
        let shared_cell: Arc<Mutex<Option<Arc<Mutex<McpClient>>>>> = Arc::new(Mutex::new(None));
        tracing::info!("MCP server {name} 列出 {} 个工具", tools.len());
        for info in &tools {
            if budget != 0 && total >= budget {
                tracing::warn!(
                    "MCP 工具数已达 budget({budget}),{name} 剩余工具折叠未注册;\
                     注册 mcp__toolsearch 元工具按关键词发现"
                );
                registry.register(Arc::new(ToolSearchTool::new(Arc::new(servers.clone()))));
                return;
            }
            let tool = McpTool::new(name, info, config.clone(), shared_cell.clone());
            registry.register(Arc::new(tool));
            total += 1;
        }
    }
}
