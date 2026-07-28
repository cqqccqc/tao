//! 会话重放:JSONL 日志 → SessionState fold(见 docs/design/sessions.md §1)。
//!
//! `fold(LogEvent*) = SessionState`。replay 用于 resume(重建 messages/grants/mode)。
//! v1:Compaction 事件识别但未应用(M2-5);ToolCall/Approval/PermissionDecision 等审计事件
//! 不影响 messages/grants/mode 投影。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tao_protocol::content::Content;
use tao_protocol::ids::SessionId;
use tao_protocol::log::{LogEvent, LogLine};
use tao_protocol::permission::PermissionMode;

use crate::model::{ModelContent, ModelMessage};

/// 重放后的会话状态(模型上下文用)。
#[derive(Debug, Clone)]
pub struct SessionState {
    pub id: SessionId,
    pub parent: Option<SessionId>,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub messages: Vec<ModelMessage>,
    pub session_grants: HashSet<(String, String)>,
    pub mode: PermissionMode,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            id: SessionId::new(String::new()),
            parent: None,
            cwd: PathBuf::new(),
            title: None,
            messages: Vec::new(),
            session_grants: HashSet::new(),
            mode: PermissionMode::Default,
        }
    }
}

/// 重放 JSONL 日志为 SessionState。
pub fn replay(path: &Path) -> Result<SessionState> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取会话日志失败: {}", path.display()))?;
    let mut state = SessionState::default();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let log_line: LogLine =
            serde_json::from_str(line).with_context(|| format!("解析日志行失败: {line}"))?;
        apply(&mut state, log_line.event);
    }
    Ok(state)
}

fn apply(state: &mut SessionState, event: LogEvent) {
    match event {
        LogEvent::SessionMeta {
            id, parent, cwd, ..
        } => {
            state.id = id;
            state.parent = parent;
            state.cwd = cwd;
        }
        LogEvent::SessionTitle { title } => state.title = Some(title),
        LogEvent::UserInput { content, .. } => {
            state.messages.push(ModelMessage::User {
                content: content.into_iter().map(content_to_model).collect(),
            });
        }
        LogEvent::AssistantMessage { content, .. } => {
            state.messages.push(ModelMessage::Assistant {
                content: content.into_iter().map(content_to_model).collect(),
            });
        }
        LogEvent::ToolResult {
            call_id, output, ..
        } => {
            // output = {"content": <str>, "is_error": <bool>}(recorder 侧约定)
            let content_str = output
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let is_error = output
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            state.messages.push(ModelMessage::ToolResult {
                call_id,
                content: vec![ModelContent::Text(content_str)],
                is_error,
            });
        }
        LogEvent::PermissionGrant { tool, pattern } => {
            state.session_grants.insert((tool, pattern));
        }
        LogEvent::ModeChange { mode } => state.mode = mode,
        LogEvent::Compaction { .. } => {
            // v1 TODO(M2-5):截断 messages 到 covers_through_seq + push summary
        }
        // ToolCall/Approval/PermissionDecision/Checkpoint/TurnBoundary/Error:
        // 审计/边界事件,不影响 messages/grants/mode 投影。
        _ => {}
    }
}

fn content_to_model(c: Content) -> ModelContent {
    match c {
        Content::Text { text } => ModelContent::Text(text),
        Content::Thinking { text, signature } => ModelContent::Thinking { text, signature },
        Content::ToolUse {
            call_id,
            name,
            input,
        } => ModelContent::ToolUse {
            call_id,
            name,
            input,
        },
        Content::Image { mime, data_base64 } => ModelContent::Image { mime, data_base64 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::{JsonlRecorder, Recorder};
    use tao_protocol::content::Content;
    use tao_protocol::ids::{CallId, TurnId};
    use tempfile::TempDir;

    #[test]
    fn replay_reconstructs_state() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let (recorder, id) = JsonlRecorder::create_with_base(&cwd, dir.path()).unwrap();
        recorder.record(LogEvent::UserInput {
            content: vec![Content::text("hi")],
            turn_id: TurnId::new("t1"),
        });
        recorder.record(LogEvent::ModeChange {
            mode: PermissionMode::Plan,
        });
        recorder.record(LogEvent::PermissionGrant {
            tool: "Bash".into(),
            pattern: "cargo *".into(),
        });

        let state = replay(recorder.path()).unwrap();
        assert_eq!(state.id, id);
        assert_eq!(state.mode, PermissionMode::Plan);
        assert!(
            state
                .session_grants
                .contains(&("Bash".into(), "cargo *".into()))
        );
        assert_eq!(state.messages.len(), 1); // UserInput
    }

    #[test]
    fn replay_tool_result_and_assistant() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let (recorder, _id) = JsonlRecorder::create_with_base(&cwd, dir.path()).unwrap();
        recorder.record(LogEvent::AssistantMessage {
            content: vec![Content::ToolUse {
                call_id: CallId::new("c1"),
                name: "Bash".into(),
                input: serde_json::json!({"command": ["echo", "x"]}),
            }],
            usage: Default::default(),
            turn_id: TurnId::new("t1"),
        });
        recorder.record(LogEvent::ToolResult {
            call_id: CallId::new("c1"),
            output: serde_json::json!({"content": "done", "is_error": false}),
            duration_ms: 10,
        });

        let state = replay(recorder.path()).unwrap();
        assert_eq!(state.messages.len(), 2); // Assistant + ToolResult
        assert!(matches!(state.messages[0], ModelMessage::Assistant { .. }));
        if let ModelMessage::ToolResult {
            is_error, content, ..
        } = &state.messages[1]
        {
            assert!(!*is_error);
            assert!(
                content
                    .iter()
                    .any(|c| matches!(c, ModelContent::Text(t) if t == "done"))
            );
        } else {
            panic!("期望 ToolResult");
        }
    }
}
