//! Grep 工具:内容搜索(见 docs/design/tools.md §2)。
//!
//! - 优先调 `rg` 二进制(argv/超时/cancel-on-drop/输出截断);`rg` 不在 PATH 时
//!   fallback 到 `regex` + `std::fs` 递归遍历(跳过 .git/target/node_modules 等,
//!   无 .gitignore 语义——v1 简化,rg 优先时无此问题)。
//! - permission_key 返回 None(read 类,默认 Allow)。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::model::ToolSpec;
use crate::permissions::PermissionKey;
use crate::tools::{Tool, ToolCtx, ToolError, ToolOutput};

const GREP_TIMEOUT_MS: u64 = 30_000;
const OUTPUT_HEAD: usize = 10_000;
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".next",
    "dist",
    ".cache",
    "__pycache__",
    ".venv",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Content,
    Files,
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Grep".into(),
            description: "按正则在文件中搜索内容。优先用 ripgrep(rg);rg 不可用时用内置遍历。\
                          知道确切内容用此工具;按文件名找文件用 Glob。\
                          默认输出 file:line:content;output_mode=files_with_matches 只列文件。"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "正则表达式" },
                    "path": { "type": "string", "description": "搜索路径(相对 cwd,默认 cwd)" },
                    "glob": { "type": "string", "description": "文件名 glob 过滤,如 \"*.rs\"" },
                    "output_mode": { "type": "string", "enum": ["content", "files_with_matches"], "description": "默认 content" }
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
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| resolve_path(&ctx.cwd, p))
            .unwrap_or_else(|| ctx.cwd.clone());
        let glob = args.get("glob").and_then(|v| v.as_str());
        let mode = match args.get("output_mode").and_then(|v| v.as_str()) {
            Some("files_with_matches") => OutputMode::Files,
            _ => OutputMode::Content,
        };

        // 先校验正则(fallback 与 rg 共用语义)
        let re =
            Regex::new(pattern).map_err(|e| ToolError::InvalidArgs(format!("正则无效: {e}")))?;

        // 优先 rg
        if let Some(out) = run_rg(pattern, &path, glob, mode, &ctx.cancel).await? {
            return Ok(out);
        }
        // fallback:内置遍历
        let out = grep_fallback(&re, &path, glob, mode, &ctx.cancel).await?;
        Ok(out)
    }
}

/// 调 rg。返回 Ok(Some) 表示 rg 可用并已产出结果;Ok(None) 表示 rg 不可用(应 fallback)。
async fn run_rg(
    pattern: &str,
    path: &Path,
    glob: Option<&str>,
    mode: OutputMode,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<ToolOutput>, ToolError> {
    let mut cmd = Command::new("rg");
    cmd.arg(pattern);
    cmd.arg("-n");
    cmd.arg("--color=never");
    if mode == OutputMode::Files {
        cmd.arg("-l");
    }
    if let Some(g) = glob {
        cmd.arg("--glob").arg(g);
    }
    cmd.arg(path);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None); // rg 未安装 → fallback
        }
        Err(e) => return Err(ToolError::Failed(format!("启动 rg 失败: {e}"))),
    };

    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let cancel_clone = cancel.clone();
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

    let exit = tokio::select! {
        s = child.wait() => s.map_err(ToolError::Io)?,
        _ = tokio::time::sleep(Duration::from_millis(GREP_TIMEOUT_MS)) => {
            let _ = child.start_kill();
            return Err(ToolError::Timeout(GREP_TIMEOUT_MS));
        }
        _ = cancel_clone.cancelled() => {
            let _ = child.start_kill();
            return Err(ToolError::Cancelled);
        }
    };

    let stdout_str = stdout_task.await.unwrap_or_default();
    let stderr_str = stderr_task.await.unwrap_or_default();
    let code = exit.code().unwrap_or(-1);

    // rg:0=有匹配,1=无匹配,>1=错误
    if code > 1 {
        return Err(ToolError::Failed(format!(
            "rg 错误(退出码 {code}): {stderr_str}"
        )));
    }
    let (truncated, dropped) = truncate(&stdout_str);
    let mut content = truncated;
    if dropped > 0 {
        content.push_str(&format!("\n[... {dropped} bytes truncated ...]"));
    }
    Ok(Some(ToolOutput::ok(content)))
}

/// fallback:regex + std::fs 递归遍历。
async fn grep_fallback(
    re: &Regex,
    path: &Path,
    glob: Option<&str>,
    mode: OutputMode,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let glob_matcher = glob
        .map(|g| globset::Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| ToolError::InvalidArgs(format!("glob 无效: {e}")))?;

    let path_buf = path.to_path_buf();
    let re_clone = re.clone();
    let cancel_clone = cancel.clone();
    let out = tokio::task::spawn_blocking(move || {
        let mut buf = String::new();
        walk(
            &path_buf,
            &re_clone,
            glob_matcher.as_ref(),
            mode,
            &cancel_clone,
            &mut buf,
        );
        buf
    })
    .await
    .map_err(|e| ToolError::Failed(format!("搜索任务失败: {e}")))?;

    let (truncated, dropped) = truncate(&out);
    let mut content = truncated;
    if dropped > 0 {
        content.push_str(&format!("\n[... {dropped} bytes truncated ...]"));
    }
    Ok(ToolOutput::ok(content))
}

fn walk(
    dir: &Path,
    re: &Regex,
    glob: Option<&globset::GlobMatcher>,
    mode: OutputMode,
    cancel: &tokio_util::sync::CancellationToken,
    out: &mut String,
) {
    if cancel.is_cancelled() || out.len() > OUTPUT_HEAD * 2 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        if cancel.is_cancelled() {
            return;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if path.is_dir() {
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            walk(&path, re, glob, mode, cancel, out);
        } else if path.is_file() {
            if let Some(g) = glob
                && !g.is_match(&path)
            {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        if mode == OutputMode::Files {
                            out.push_str(&format!("{}\n", path.display()));
                            break;
                        }
                        out.push_str(&format!("{}:{}:{}\n", path.display(), i + 1, line));
                    }
                }
            }
        }
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

fn truncate(s: &str) -> (String, usize) {
    if s.len() <= OUTPUT_HEAD {
        return (s.to_owned(), 0);
    }
    let end = find_char_boundary(s, OUTPUT_HEAD.min(s.len()));
    let head = &s[..end];
    (head.to_owned(), s.len() - head.len())
}

/// 从 `from` 往前找第一个 UTF-8 字符边界(`floor_char_boundary` 的 MSRV 兼容版)。
fn find_char_boundary(s: &str, mut from: usize) -> usize {
    if from >= s.len() {
        return s.len();
    }
    while from > 0 && !s.is_char_boundary(from) {
        from -= 1;
    }
    from
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
    async fn grep_fallback_finds_matches() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "fn main() {}\nfn other() {}\n")
            .await
            .unwrap();
        fs::write(dir.path().join("b.txt"), "nope\n").await.unwrap();
        // 强制 fallback:用一个不存在的 rg 路径?简单起见测 fallback 函数本身
        let re = Regex::new("fn main").unwrap();
        let out = grep_fallback(
            &re,
            dir.path(),
            None,
            OutputMode::Content,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.content.contains("a.rs:1:fn main() {}"));
        assert!(!out.content.contains("b.txt"));
    }

    #[tokio::test]
    async fn grep_fallback_files_mode() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "fn main() {}\n")
            .await
            .unwrap();
        let re = Regex::new("main").unwrap();
        let out = grep_fallback(
            &re,
            dir.path(),
            None,
            OutputMode::Files,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.content.contains("a.rs"));
        assert!(!out.content.contains(":1:")); // files 模式无行号
    }

    #[tokio::test]
    async fn grep_tool_runs_or_fallback() {
        // 不假设 rg 存在;只验证工具不 panic 且返回 ok(rg 或 fallback 之一)
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "fn main() {}\n")
            .await
            .unwrap();
        let out = GrepTool
            .call(
                &json!({"pattern": "fn main", "path": "."}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("fn main"));
    }

    #[tokio::test]
    async fn grep_invalid_regex_error() {
        let dir = TempDir::new().unwrap();
        let res = GrepTool
            .call(&json!({"pattern": "(unclosed"}), &ctx(dir.path()))
            .await;
        assert!(res.is_err());
    }
}
