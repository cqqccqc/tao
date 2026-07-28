//! Glob 工具:文件名模式搜索(见 docs/design/tools.md §2)。
//!
//! 用 `glob` crate 遍历 + 匹配(支持 `**`)。结果按 mtime 倒序(最近优先),
//! 限 100 条。permission_key 返回 None(read 类,默认 Allow)。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::model::ToolSpec;
use crate::permissions::PermissionKey;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

const MAX_MATCHES: usize = 100;

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Glob".into(),
            description: "按文件名模式查找文件(支持 `*` / `**` / `?`)。\
                          知道文件名/路径模式用此工具;按内容搜用 Grep。\
                          结果按修改时间倒序,最多 100 条。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "文件名 glob,如 \"src/**/*.rs\"" }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn permission_key(&self, _args: &Value, _cwd: &Path) -> Option<PermissionKey> {
        None
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("pattern 必须是字符串".into()))?;

        // glob 相对 ctx.cwd;绝对 pattern 直接用
        let full_pattern = if PathBuf::from(pattern).is_absolute() {
            pattern.to_string()
        } else {
            ctx.cwd.join(pattern).to_string_lossy().into_owned()
        };

        let cwd = ctx.cwd.clone();
        let full = full_pattern.clone();
        // glob 遍历是同步 IO,放 spawn_blocking 避免阻塞 runtime
        let mut matches = tokio::task::spawn_blocking(
            move || -> Result<Vec<(PathBuf, SystemTime)>, ToolError> {
                let iter = glob::glob(&full)
                    .map_err(|e| ToolError::InvalidArgs(format!("glob pattern 无效: {e}")))?;
                let mut out = Vec::new();
                for entry in iter {
                    let path = entry.map_err(|e| ToolError::Failed(format!("glob 遍历: {e}")))?;
                    let mtime = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    out.push((path, mtime));
                    if out.len() >= MAX_MATCHES {
                        break;
                    }
                }
                Ok(out)
            },
        )
        .await
        .map_err(|e| ToolError::Failed(format!("glob 任务失败: {e}")))??;

        // 按 mtime 倒序
        matches.sort_by_key(|b| std::cmp::Reverse(b.1));

        let lines: Vec<String> = matches
            .iter()
            .map(|(p, _)| {
                p.strip_prefix(&cwd)
                    .map(|r| r.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| p.to_string_lossy().into_owned())
            })
            .collect();

        if lines.is_empty() {
            return Ok(ToolOutput::ok(format!("未找到匹配 {pattern:?} 的文件")));
        }
        Ok(ToolOutput::ok(format!(
            "找到 {} 个匹配:\n{}",
            lines.len(),
            lines.join("\n")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::fs;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx::new(dir.to_path_buf())
    }

    #[tokio::test]
    async fn glob_finds_rs_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "x").await.unwrap();
        fs::write(dir.path().join("b.txt"), "x").await.unwrap();
        fs::create_dir(dir.path().join("sub")).await.unwrap();
        fs::write(dir.path().join("sub").join("c.rs"), "x")
            .await
            .unwrap();
        let out = GlobTool
            .call(&json!({"pattern": "**/*.rs"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("a.rs"));
        assert!(out.content.contains("sub/c.rs"));
        assert!(!out.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn glob_no_matches() {
        let dir = TempDir::new().unwrap();
        let out = GlobTool
            .call(&json!({"pattern": "**/*.nonexistent"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("未找到"));
    }

    #[tokio::test]
    async fn glob_invalid_pattern() {
        let dir = TempDir::new().unwrap();
        let res = GlobTool
            .call(&json!({"pattern": "[unclosed"}), &ctx(dir.path()))
            .await;
        assert!(res.is_err());
    }
}
