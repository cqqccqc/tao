//! Bash 工具:执行 shell 命令。
//! 见 docs/design/tools.md §2/§4。
//!
//! 设计:
//! - **不经 shell 解析**:`command` 是 argv 数组,需要管道/重定向时模型须显式
//!   `["bash", "-lc", "..."]`。这是逃逸分析的前提(M2 实现权限引擎时)。
//! - 流式输出:stdout/stderr 分行产出(M2 起通过 EventSink 发 ExecCommandOutputDelta;
//!   M1 的 ToolCtx 暂无 sink,先合并返回)。
//! - 超时 + cancel-on-drop:进程组整体处理,不留孤儿。
//! - 输出截断:超阈值保留头尾,中间标记 `[... N bytes truncated ...]`。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::Instant;

use crate::model::ToolSpec;
use crate::permissions::PermissionKey;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// 输出截断阈值(字符数)。头 10k + 尾 20k。
const OUTPUT_HEAD: usize = 10_000;
const OUTPUT_TAIL: usize = 20_000;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Bash".into(),
            description: "执行 shell 命令。command 是 argv 数组(不经 shell 解析);\
                          需要管道/重定向/子shell 时用 [\"bash\",\"-lc\",\"...\"]。\
                          默认超时 120s,可通过 timeout_ms 覆盖(上限 600s)。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "命令与参数(argv 数组),如 [\"cargo\",\"test\"]",
                        "minItems": 1
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "超时毫秒数,默认 120000,上限 600000",
                        "minimum": 0
                    },
                    "cwd": {
                        "type": "string",
                        "description": "工作目录(相对 ToolCtx.cwd 解析);省略则用 ToolCtx.cwd"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn permission_key(&self, args: &Value, _cwd: &Path) -> Option<PermissionKey> {
        parse_command(args)
            .filter(|c| !c.is_empty())
            .map(|command| PermissionKey::Bash { command })
    }

    async fn call(&self, args: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let command = parse_command(args)
            .ok_or_else(|| ToolError::InvalidArgs("command 必须是字符串数组".into()))?;
        if command.is_empty() {
            return Err(ToolError::InvalidArgs("command 不能为空".into()));
        }

        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        let cwd = args
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(|p| ctx.cwd.join(p))
            .unwrap_or_else(|| ctx.cwd.clone());

        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..]);
        cmd.current_dir(&cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // 进程组:cancel 时整体 kill,不留孤儿。
        cmd.process_group(0);
        cmd.kill_on_drop(true);

        let start = Instant::now();
        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Failed(format!("启动 `{}` 失败: {e}", command[0])))?;

        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");

        // 在子任务里读 stdout/stderr,主任务 select 超时/取消/退出。
        let cancel = ctx.cancel.clone();
        let stdout_task = tokio::spawn(async move {
            let mut buf = String::new();
            let _ = stdout.read_to_string(&mut buf).await;
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf).await;
            buf
        });

        // 等待:子进程退出 / 超时 / 取消
        let exit_status = tokio::select! {
            s = child.wait() => s.map_err(ToolError::Io)?,
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                // 超时:child 因 kill_on_drop 会在 drop 时被 kill,但我们显式 kill 更快。
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(ToolError::Timeout(timeout_ms));
            }
            _ = cancel.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(ToolError::Cancelled);
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        let stdout_str = stdout_task.await.unwrap_or_default();
        let stderr_str = stderr_task.await.unwrap_or_default();

        let exit_code = exit_status.code().unwrap_or(-1);
        let (stdout_trunc, stdout_dropped) = truncate(&stdout_str);
        let (stderr_trunc, stderr_dropped) = truncate(&stderr_str);

        let mut content = String::new();
        content.push_str(&format!("Exit: {}\n", exit_code));
        content.push_str(&format!("Duration: {}ms\n\n", duration_ms));
        if !stdout_trunc.is_empty() {
            content.push_str("=== stdout ===\n");
            content.push_str(&stdout_trunc);
            if stdout_dropped > 0 {
                content.push_str(&format!("\n[... {} bytes truncated ...]", stdout_dropped));
            }
            content.push('\n');
        }
        if !stderr_trunc.is_empty() {
            content.push_str("\n=== stderr ===\n");
            content.push_str(&stderr_trunc);
            if stderr_dropped > 0 {
                content.push_str(&format!("\n[... {} bytes truncated ...]", stderr_dropped));
            }
            content.push('\n');
        }

        // 非零退出码也算正常输出(模型可见 exit code),只有基础设施错误才是 ToolError。
        Ok(ToolOutput {
            content,
            is_error: !exit_status.success(),
        })
    }
}

/// 从 args 解析 command argv(lenient:任一元素非字符串或缺失则返回 None)。
/// `permission_key` 与 `call` 共用,保证权限判定与实际执行看同一个 command。
fn parse_command(args: &Value) -> Option<Vec<String>> {
    let arr = args.get("command")?.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        out.push(v.as_str()?.to_string());
    }
    Some(out)
}

/// 截断输出:保留头 OUTPUT_HEAD + 尾 OUTPUT_TAIL,中间丢弃。
/// 返回 (保留的文本, 被丢弃的字节数)。
fn truncate(s: &str) -> (String, usize) {
    let total = s.len();
    if total <= OUTPUT_HEAD + OUTPUT_TAIL {
        return (s.to_owned(), 0);
    }
    let head = &s[..OUTPUT_HEAD];
    // 从尾部往回找字符边界(避免切到 UTF-8 中间)。
    let tail_start = find_char_boundary(s, total - OUTPUT_TAIL);
    let tail = &s[tail_start..];
    let dropped = total - OUTPUT_HEAD - (total - tail_start);
    let mut out = String::with_capacity(OUTPUT_HEAD + OUTPUT_TAIL + 64);
    out.push_str(head);
    out.push_str("\n[... truncated ...]\n");
    out.push_str(tail);
    (out, dropped)
}

/// 从 `from` 往后找第一个 UTF-8 字符边界。
fn find_char_boundary(s: &str, mut from: usize) -> usize {
    if from >= s.len() {
        return s.len();
    }
    while from > 0 && !s.is_char_boundary(from) {
        from -= 1;
    }
    from
}
