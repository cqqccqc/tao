//! 文件系统工具:Read + Write。
//! 见 docs/design/tools.md §2。

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::fs;

use crate::model::ToolSpec;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

// ---- Read ----

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Read".into(),
            description: "读取文件内容,带行号。支持 offset/limit 分页。\
                          二进制/图片文件返回元信息而非内容。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径(相对 cwd 或绝对)" },
                    "offset": { "type": "integer", "description": "起始行号(1-based,默认 1)", "minimum": 1 },
                    "limit": { "type": "integer", "description": "读取行数(默认 2000)", "minimum": 1 }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("path 必须是字符串".into()))?;
        let path = resolve_path(&ctx.cwd, path_str);

        let offset = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000) as usize;

        let meta = fs::metadata(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError::Failed(format!("文件不存在: {}", path.display()))
            } else {
                ToolError::Io(e)
            }
        })?;

        // 二进制检测:简单启发式(含 NUL 字节则视为二进制)。
        if meta.is_dir() {
            return Ok(ToolOutput::error(format!(
                "路径是目录,不是文件: {}",
                path.display()
            )));
        }

        let content = fs::read(&path).await.map_err(ToolError::Io)?;

        if is_binary(&content) {
            return Ok(ToolOutput::ok(format!(
                "[二进制文件,{} bytes,不显示内容]\n路径: {}",
                content.len(),
                path.display()
            )));
        }

        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        // 1-based 行号 → 0-based 索引
        let start_idx = offset.saturating_sub(1).min(total);
        let end_idx = (start_idx + limit).min(total);

        let mut out = String::new();
        for (i, line) in lines[start_idx..end_idx].iter().enumerate() {
            let lineno = start_idx + i + 1;
            out.push_str(&format!("{:>6}\t{}\n", lineno, line));
        }
        // 显示的行号范围(1-based inclusive)
        let first_line = start_idx + 1;
        let last_line = end_idx;
        out.push_str(&format!(
            "\n[{}-{} of {} lines]",
            first_line, last_line, total
        ));

        Ok(ToolOutput::ok(out))
    }
}

// ---- Write ----

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Write".into(),
            description: "创建或覆盖文件。新文件用此工具;已有文件的小改动用 Edit(M2)。\
                          写入前会创建父目录。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径" },
                    "content": { "type": "string", "description": "文件完整内容" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("path 必须是字符串".into()))?;
        let path = resolve_path(&ctx.cwd, path_str);

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("content 必须是字符串".into()))?;

        // 创建父目录
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            fs::create_dir_all(parent).await.map_err(ToolError::Io)?;
        }

        let existed = path.exists();
        let bytes = content.len();
        fs::write(&path, content).await.map_err(ToolError::Io)?;

        Ok(ToolOutput::ok(format!(
            "{} {} ({} bytes)",
            if existed { "overwrote" } else { "created" },
            path.display(),
            bytes
        )))
    }
}

// ---- helpers ----

fn resolve_path(cwd: &Path, p: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// 简单二进制检测:含 NUL 字节即视为二进制(与 ripgrep/file 启发式一致)。
fn is_binary(bytes: &[u8]) -> bool {
    // 只检查前 8KB,避免大文件全扫。
    let head = &bytes[..bytes.len().min(8192)];
    head.contains(&0)
}
