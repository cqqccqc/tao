//! 工具系统:Tool trait、注册表、内置工具。
//! 见 docs/design/tools.md。
//!
//! 设计要点:
//! - `ToolError` 与 `ToolOutput` 都会变成 `ToolResult` 消息回到模型——
//!   拒绝和失败都是模型可见的信息,不是异常。
//! - 工具调用前后经 hooks 与权限判定(M2 实现),本模块只定义接口。

pub mod bash;
pub mod edit;
pub mod fs;
pub mod glob;
pub mod grep;
pub mod patch;
pub mod task;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::model::ToolSpec;
use crate::permissions::PermissionKey;

/// 工具调用上下文。M2 起会扩展 permissions / event_sink / subagent_factory。
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    /// 本 turn 内已 Read 过的文件(canonical 路径),供 Edit 校验"先 Read"防盲改。
    pub read_files: Arc<Mutex<HashSet<PathBuf>>>,
}

impl ToolCtx {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            cancel: CancellationToken::new(),
            read_files: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// 测试用:带自定义 cancel。
    pub fn with_cancel(cwd: impl Into<PathBuf>, cancel: CancellationToken) -> Self {
        Self {
            cwd: cwd.into(),
            cancel,
            read_files: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// 带共享 read_files(跨工具调用持久,run_turn 用)。
    pub fn with_cancel_and_reads(
        cwd: impl Into<PathBuf>,
        cancel: CancellationToken,
        read_files: Arc<Mutex<HashSet<PathBuf>>>,
    ) -> Self {
        Self {
            cwd: cwd.into(),
            cancel,
            read_files,
        }
    }
}

/// 工具输出:作为 `ToolResult` 的 content 回到模型。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// 文本内容(最常见);M2 起可扩展为 Vec<Content> 支持图片等。
    pub content: String,
    /// 是否为错误结果(对应 `is_error: true` 的 tool_result)。
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// 工具错误:不会直接回模型,而是被 turn loop 捕获后转成 `ToolOutput::error`。
/// 例外:`Deny` / `Reject` 直接作为 `ToolResult.is_error=true` 的原因文本。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("参数无效: {0}")]
    InvalidArgs(String),
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("超时({0}ms)")]
    Timeout(u64),
    #[error("已取消")]
    Cancelled,
    #[error("工具内部错误: {0}")]
    Failed(String),
}

/// 工具 trait。所有内置工具与 MCP 工具都实现它。
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具规范(名/描述/JSON Schema),发给模型作 tools 字段。
    fn spec(&self) -> ToolSpec;

    /// 执行工具。返回 `ToolOutput`(成功或失败都是输出)或 `ToolError`(基础设施级失败)。
    async fn call(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;

    /// 提取权限维度(供 `PermissionEngine` 判定)。
    /// 默认 `None`:未知工具(如未实现的 MCP)走模式默认值。
    /// builtin 工具各自覆盖:Bash 提取 command argv,Read/Write 提取 path。
    fn permission_key(&self, _args: &Value, _cwd: &Path) -> Option<PermissionKey> {
        None
    }
}

/// 工具注册表:按名分发。
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 内置工具集(M2:Bash + Read + Write + Edit + Patch + Grep + Glob)。
    pub fn builtin() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(bash::BashTool));
        r.register(Arc::new(fs::ReadTool));
        r.register(Arc::new(fs::WriteTool));
        r.register(Arc::new(edit::EditTool));
        r.register(Arc::new(patch::PatchTool));
        r.register(Arc::new(grep::GrepTool));
        r.register(Arc::new(glob::GlobTool));
        r.register(Arc::new(task::TaskTool));
        r
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name;
        self.tools.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// 收集所有工具的 spec(供 ModelRequest.tools 使用)。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec()).collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// 只读工具子集(子 agent 用):按 names 过滤,只保留 Read/Grep/Glob。
    pub fn readonly_subset(&self, names: &[String]) -> Self {
        let mut r = Self::new();
        for name in names {
            if matches!(name.as_str(), "Read" | "Grep" | "Glob")
                && let Some(t) = self.tools.get(name)
            {
                r.register(t.clone());
            }
        }
        r
    }
}
