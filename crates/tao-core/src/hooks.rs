//! hooks:事件点 + 守门(见 extensibility.md §1)。
//!
//! core spawn 命令,stdin 喂 JSON,退出码语义:0 放行 / 2 阻断(stderr 反馈) /
//! 其他非阻断错误(警告 + 放行)。最小环境(剥离 provider 凭证)。
//!
//! v1 事件点:SessionStart/SessionEnd/PreToolUse/PostToolUse/Stop。
//! UserPromptSubmit/Notification/SubagentStop 留后续。

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

/// hook 事件点。
#[derive(Debug, Clone)]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    PreToolUse { tool: String },
    PostToolUse { tool: String },
    UserPromptSubmit { text: String },
    Notification { message: String },
    SubagentStop { name: String },
    Stop,
}

impl HookEvent {
    /// 事件名(JSON / 环境变量用)。
    pub fn name(&self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::PreToolUse { .. } => "PreToolUse",
            HookEvent::PostToolUse { .. } => "PostToolUse",
            HookEvent::UserPromptSubmit { .. } => "UserPromptSubmit",
            HookEvent::Notification { .. } => "Notification",
            HookEvent::SubagentStop { .. } => "SubagentStop",
            HookEvent::Stop => "Stop",
        }
    }

    /// 关联的工具名(非工具事件为 None)。
    pub fn tool(&self) -> Option<&str> {
        match self {
            HookEvent::PreToolUse { tool } | HookEvent::PostToolUse { tool } => Some(tool),
            _ => None,
        }
    }
}

/// 单条 hook 配置(`[hooks.<event>]` 数组项)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookConfig {
    /// 工具名匹配(`Bash|Edit` 多选,`*` 全匹配;非工具事件忽略)。
    #[serde(default = "default_matcher")]
    pub matcher: String,
    /// shell 命令(`sh -c` 执行)。
    pub command: String,
    /// 超时毫秒(默认 5000)。
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_matcher() -> String {
    "*".into()
}
fn default_timeout_ms() -> u64 {
    5000
}

/// hook 执行上下文(spawn 时 stdin JSON + 环境变量)。
#[derive(Debug, Clone)]
pub struct HookCtx {
    pub session_id: String,
    pub cwd: PathBuf,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
}

/// hook 执行结果。
#[derive(Debug, Clone)]
pub enum HookOutcome {
    /// 放行(所有 hook Pass 或非阻断错误)。
    Pass,
    /// 阻断(某 hook exit 2,reason = stderr)。
    Block(String),
}

/// 运行一组 hook(并行,任一 Block 则 Block)。`hooks` 应已按事件点过滤。
/// v1:并行不短路(全部跑完再聚合 Block);hook 数量少,影响小。
pub async fn run_hooks(event: &HookEvent, hooks: &[HookConfig], ctx: &HookCtx) -> HookOutcome {
    use futures::future::join_all;
    let futs: Vec<_> = hooks
        .iter()
        .filter(|h| matches_event(event, &h.matcher))
        .map(|h| exec_hook(&h.command, event, ctx, h.timeout_ms))
        .collect();
    let results = join_all(futs).await;
    let mut block: Option<String> = None;
    for r in results {
        match r {
            HookExecResult::Pass => {}
            HookExecResult::Block(reason) => {
                if block.is_none() {
                    block = Some(reason);
                }
            }
            HookExecResult::Error(e) => {
                tracing::warn!("hook 非阻断错误: {e}");
            }
        }
    }
    match block {
        Some(reason) => HookOutcome::Block(reason),
        None => HookOutcome::Pass,
    }
}

fn matches_event(event: &HookEvent, matcher: &str) -> bool {
    if matcher == "*" {
        return true;
    }
    match event.tool() {
        Some(tool) => matcher.split('|').any(|m| m.trim() == tool),
        None => true, // 非工具事件,任何 matcher 匹配
    }
}

enum HookExecResult {
    Pass,
    Block(String),
    Error(String),
}

async fn exec_hook(
    command: &str,
    event: &HookEvent,
    ctx: &HookCtx,
    timeout_ms: u64,
) -> HookExecResult {
    let stdin_json = serde_json::json!({
        "event": event.name(),
        "session_id": ctx.session_id,
        "cwd": ctx.cwd.display().to_string(),
        "tool_name": ctx.tool_name,
        "tool_input": ctx.tool_input,
    })
    .to_string();

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd.current_dir(&ctx.cwd);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // 最小环境:剥离 provider 凭证,只保留 PATH/HOME
    cmd.env_clear();
    if let Ok(p) = std::env::var("PATH") {
        cmd.env("PATH", p);
    }
    if let Ok(h) = std::env::var("HOME") {
        cmd.env("HOME", h);
    }
    cmd.env("TAO_HOOK_EVENT", event.name());
    if let Some(t) = &ctx.tool_name {
        cmd.env("TAO_TOOL_NAME", t);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return HookExecResult::Error(format!("启动失败: {e}")),
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(stdin_json.as_bytes()).await;
    }
    drop(child.stdin.take());

    let result =
        tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output()).await;
    match result {
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if output.status.success() {
                HookExecResult::Pass
            } else if output.status.code() == Some(2) {
                HookExecResult::Block(stderr.trim().to_string())
            } else {
                HookExecResult::Error(format!(
                    "退出码 {:?}: {}",
                    output.status.code(),
                    stderr.trim()
                ))
            }
        }
        Ok(Err(e)) => HookExecResult::Error(format!("wait 失败: {e}")),
        Err(_) => HookExecResult::Error(format!("超时({timeout_ms}ms)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(cwd: &std::path::Path) -> HookCtx {
        HookCtx {
            session_id: "test".into(),
            cwd: cwd.to_path_buf(),
            tool_name: Some("Bash".into()),
            tool_input: None,
        }
    }

    #[tokio::test]
    async fn pass_on_exit_0() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = vec![HookConfig {
            matcher: "*".into(),
            command: "exit 0".into(),
            timeout_ms: 5000,
        }];
        let outcome = run_hooks(
            &HookEvent::PreToolUse {
                tool: "Bash".into(),
            },
            &hooks,
            &ctx(dir.path()),
        )
        .await;
        assert!(matches!(outcome, HookOutcome::Pass));
    }

    #[tokio::test]
    async fn block_on_exit_2() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = vec![HookConfig {
            matcher: "*".into(),
            command: "echo '危险操作' >&2; exit 2".into(),
            timeout_ms: 5000,
        }];
        let outcome = run_hooks(
            &HookEvent::PreToolUse {
                tool: "Bash".into(),
            },
            &hooks,
            &ctx(dir.path()),
        )
        .await;
        match outcome {
            HookOutcome::Block(reason) => assert!(reason.contains("危险操作")),
            _ => panic!("期望 Block"),
        }
    }

    #[tokio::test]
    async fn timeout_is_non_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = vec![HookConfig {
            matcher: "*".into(),
            command: "sleep 10".into(),
            timeout_ms: 100,
        }];
        let outcome = run_hooks(
            &HookEvent::PreToolUse {
                tool: "Bash".into(),
            },
            &hooks,
            &ctx(dir.path()),
        )
        .await;
        assert!(matches!(outcome, HookOutcome::Pass)); // 超时非阻断
    }

    #[tokio::test]
    async fn matcher_filters_tool() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = vec![HookConfig {
            matcher: "Edit".into(), // 只匹配 Edit,不匹配 Bash
            command: "exit 2".into(),
            timeout_ms: 5000,
        }];
        let outcome = run_hooks(
            &HookEvent::PreToolUse {
                tool: "Bash".into(),
            },
            &hooks,
            &ctx(dir.path()),
        )
        .await;
        assert!(matches!(outcome, HookOutcome::Pass)); // 不匹配,跳过
    }

    #[tokio::test]
    async fn block_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = vec![
            HookConfig {
                matcher: "*".into(),
                command: "echo first; exit 2".into(),
                timeout_ms: 5000,
            },
            HookConfig {
                matcher: "*".into(),
                command: "echo second; exit 0".into(),
                timeout_ms: 5000,
            },
        ];
        let outcome = run_hooks(
            &HookEvent::PreToolUse {
                tool: "Bash".into(),
            },
            &hooks,
            &ctx(dir.path()),
        )
        .await;
        assert!(matches!(outcome, HookOutcome::Block(_))); // 第一个 Block 短路
    }
}
