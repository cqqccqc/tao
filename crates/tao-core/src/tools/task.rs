//! Task 工具:调用子 agent(spec only;实际执行在 run_turn 特殊处理)。
//!
//! 模型看到 Task spec,调用时 run_turn 拦截(不调 `Tool::call`),
//! spawn 子 run_turn(只读权限,独立会话),返回报告。

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::model::ToolSpec;
use crate::permissions::PermissionKey;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Task".into(),
            description: "调用子 agent(subagent)处理隔离的子任务,返回报告。\
                          用于上下文隔离的探索/评审。子 agent 在 ~/.tao/agents/ 或 .tao/agents/ 定义。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "subagent": { "type": "string", "description": "子 agent 名(~/.tao/agents/<name>.md)" },
                    "prompt": { "type": "string", "description": "给子 agent 的任务描述" }
                },
                "required": ["subagent", "prompt"]
            }),
        }
    }

    fn permission_key(&self, _args: &Value, _cwd: &Path) -> Option<PermissionKey> {
        None // Task 走 run_turn 特殊处理,不走权限判定
    }

    async fn call(&self, _args: &Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        // 不应被调(run_turn 拦截 Task)
        Err(ToolError::Failed(
            "Task 工具应由 run_turn 特殊处理,不应直接调用".into(),
        ))
    }
}
