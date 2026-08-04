//! Edit 工具:精确字符串替换(模式 A,见 docs/design/tools.md §3)。
//!
//! - `old_string` 在文件中唯一(除非 `replace_all`),否则失败提示用更多上下文。
//! - 成功输出 unified diff 片段。
//! - v1 不强制"先 Read"(靠唯一性 + diff 防盲改;TODO:ToolCtx 加 read_files 跟踪)。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::fs;

use crate::model::ToolSpec;
use crate::permissions::PermissionKey;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Edit".into(),
            description: "精确编辑:用 new_string 替换文件中的 old_string。\
                          old_string 须在文件中唯一(除非 replace_all=true),否则失败并提示用更多上下文。\
                          已有文件的小改动用此工具;新文件用 Write;多文件/大改动用 Patch。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径(相对 cwd 或绝对)" },
                    "old_string": { "type": "string", "description": "要替换的文本(须唯一,除非 replace_all)" },
                    "new_string": { "type": "string", "description": "替换为的文本" },
                    "replace_all": { "type": "boolean", "description": "替换所有匹配(默认 false)", "default": false }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn permission_key(&self, args: &Value, cwd: &Path) -> Option<PermissionKey> {
        let p = args.get("path")?.as_str()?;
        Some(PermissionKey::Path {
            path: resolve_path(cwd, p),
        })
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("path 必须是字符串".into()))?;
        let old_string = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("old_string 必须是字符串".into()))?;
        let new_string = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("new_string 必须是字符串".into()))?;
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old_string == new_string {
            return Ok(ToolOutput::error("old_string 与 new_string 相同,无需编辑"));
        }

        let path = resolve_path(&ctx.cwd, path_str);
        // 校验"先 Read":防盲改。read_files 为 turn 级共享(由 run_turn 注入)。
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        let read_ok = ctx
            .read_files
            .lock()
            .map(|s| s.contains(&canon))
            .unwrap_or(false);
        if !read_ok {
            return Ok(ToolOutput::error(format!(
                "编辑前须先 Read 该文件(防盲改): {}",
                path.display()
            )));
        }
        let old = match fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput::error(format!("文件不存在: {}", path.display())));
            }
            Err(e) => return Err(ToolError::Io(e)),
        };

        let count = old.matches(old_string).count();
        if count == 0 {
            return Ok(ToolOutput::error(format!(
                "old_string 未在文件中找到: {}\n请先 Read 确认内容,或检查空白/缩进。",
                path.display()
            )));
        }
        if count > 1 && !replace_all {
            return Ok(ToolOutput::error(format!(
                "old_string 在文件中出现 {count} 次,无法唯一定位。\
                 请提供更多上下文使 old_string 唯一,或设 replace_all=true 替换全部。"
            )));
        }

        let new = if replace_all {
            old.replace(old_string, new_string)
        } else {
            old.replacen(old_string, new_string, 1)
        };

        fs::write(&path, &new).await.map_err(ToolError::Io)?;
        let diff = make_diff(&path, &old, &new);
        Ok(ToolOutput::ok(format!(
            "已编辑 {}\n{}",
            path.display(),
            diff
        )))
    }
}

fn resolve_path(cwd: &Path, p: &str) -> PathBuf {
    let path = PathBuf::from(p);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// 生成 unified diff(similar)。
fn make_diff(path: &Path, old: &str, new: &str) -> String {
    let p = path.display().to_string();
    similar::udiff::unified_diff(similar::Algorithm::Myers, old, new, 3, Some((&p, &p)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;
    use tempfile::TempDir;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx::new(dir.to_path_buf())
    }

    #[tokio::test]
    async fn edit_unique_replace() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("f.rs");
        fs::write(&p, "fn a() {}\nfn b() {}\n").await.unwrap();
        let c = ctx(dir.path());
        // 模拟先 Read(注入 read_files)
        c.read_files
            .lock()
            .unwrap()
            .insert(p.canonicalize().unwrap());
        let out = EditTool
            .call(
                &json!({"path": "f.rs", "old_string": "fn a() {}", "new_string": "fn a() { return; }"}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        let result = fs::read_to_string(&p).await.unwrap();
        assert!(result.contains("fn a() { return; }"));
        assert!(out.content.contains("-fn a() {}"));
        assert!(out.content.contains("+fn a() { return; }"));
    }

    #[tokio::test]
    async fn edit_non_unique_rejected() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.rs"), "x\nx\n").await.unwrap();
        let out = EditTool
            .call(
                &json!({"path": "f.rs", "old_string": "x", "new_string": "y"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        // 未写盘
        assert_eq!(
            fs::read_to_string(dir.path().join("f.rs")).await.unwrap(),
            "x\nx\n"
        );
    }

    #[tokio::test]
    async fn edit_replace_all() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("f.rs");
        fs::write(&p, "x\nx\n").await.unwrap();
        let c = ctx(dir.path());
        c.read_files
            .lock()
            .unwrap()
            .insert(p.canonicalize().unwrap());
        let out = EditTool
            .call(
                &json!({"path": "f.rs", "old_string": "x", "new_string": "y", "replace_all": true}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(fs::read_to_string(&p).await.unwrap(), "y\ny\n");
    }

    #[tokio::test]
    async fn edit_not_found_error() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.rs"), "fn a() {}\n")
            .await
            .unwrap();
        let out = EditTool
            .call(
                &json!({"path": "f.rs", "old_string": "nope", "new_string": "y"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
