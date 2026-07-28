//! Patch 工具:apply-patch DSL(模式 B,见 docs/design/tools.md §3)。
//!
//! 多文件/大改动的事务性补丁。调 `tao_apply_patch::parse` + `apply`:
//! 解析层(有错即拒)→ L1 文本 fuzz 寻址 → 全部成功才写盘(原子)。
//! 输出 unified diff。
//!
//! permission_key 返回 None:Patch 涉及多文件,单一路径无法代表;走 mode 默认
//! (Patch 属 write 类,Default=Ask/Plan=Deny/AcceptEdits=Allow)。审批弹窗显示
//! tool=Patch(v1 不展开文件列表,TODO: ApprovalDetail.files 多文件)。

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};
use tao_apply_patch::{apply, parse};

use crate::model::ToolSpec;
use crate::permissions::PermissionKey;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct PatchTool;

#[async_trait]
impl Tool for PatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Patch".into(),
            description: "应用 apply-patch DSL 补丁:多文件事务性增/改/删/移动。\
                          语法:`*** Begin Patch` / `*** Add File: <p>` / `*** Update File: <p>` / \
                          `*** Delete File: <p>` / `*** Move File: <from> to <to>` / `*** End Patch`;\
                          Update 块内 `@@ <锚>`、` <context>`、`-<remove>`、`+<add>`。\
                          全部 hunk 寻址成功才写盘(失败即拒不猜测)。多文件/大改动用此工具。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "apply-patch DSL 文本" }
                },
                "required": ["patch"]
            }),
        }
    }

    fn permission_key(&self, _args: &Value, _cwd: &Path) -> Option<PermissionKey> {
        None
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let patch = args
            .get("patch")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs("patch 必须是字符串".into()))?;

        let hunks = match parse(patch) {
            Ok(h) => h,
            Err(e) => return Ok(ToolOutput::error(format!("patch 解析失败: {e}"))),
        };
        if hunks.is_empty() {
            return Ok(ToolOutput::error("patch 不含任何 hunk"));
        }
        let n = hunks.len();
        // apply 是同步(blocking fs);小文件直接调,大 patch 可考虑 spawn_blocking。
        let diff = match apply(&hunks, &ctx.cwd) {
            Ok(d) => d,
            Err(e) => return Ok(ToolOutput::error(format!("patch 应用失败: {e}"))),
        };
        Ok(ToolOutput::ok(format!("已应用 patch({n} 个 hunk)\n{diff}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCtx;
    use tempfile::TempDir;
    use tokio::fs;

    fn ctx(dir: &Path) -> ToolCtx {
        ToolCtx::new(dir.to_path_buf())
    }

    #[tokio::test]
    async fn patch_update_and_add() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "fn old() {}\n")
            .await
            .unwrap();
        let patch = "*** Begin Patch\n\
            *** Update File: a.rs\n\
            -fn old() {}\n\
            +fn new() {}\n\
            *** Add File: b.rs\n\
            +fn added() {}\n\
            *** End Patch\n";
        let out = PatchTool
            .call(&json!({"patch": patch}), &ctx(dir.path()))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.rs")).await.unwrap(),
            "fn new() {}\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("b.rs")).await.unwrap(),
            "fn added() {}\n"
        );
    }

    #[tokio::test]
    async fn patch_parse_error_reported() {
        let dir = TempDir::new().unwrap();
        let out = PatchTool
            .call(&json!({"patch": "not a patch"}), &ctx(dir.path()))
            .await
            .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn patch_apply_failure_no_write() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}\n")
            .await
            .unwrap();
        // 寻址失败(文件无 old)→ 不写盘
        let patch = "*** Begin Patch\n\
            *** Update File: a.rs\n\
            -fn nonexistent() {}\n\
            +fn x() {}\n\
            *** End Patch\n";
        let out = PatchTool
            .call(&json!({"patch": patch}), &ctx(dir.path()))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(
            fs::read_to_string(dir.path().join("a.rs")).await.unwrap(),
            "fn a() {}\n"
        );
    }
}
