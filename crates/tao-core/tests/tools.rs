//! 内置工具测试:Bash / Read / Write。
//! 见 docs/design/testing.md §3。

use serde_json::json;
use tao_core::tools::{Tool, ToolCtx, ToolError, ToolRegistry};
use tokio_util::sync::CancellationToken;

async fn run(tool: &dyn Tool, args: &serde_json::Value, cwd: &str) -> Result<String, ToolError> {
    let ctx = ToolCtx::new(cwd);
    tool.call(args, &ctx).await.map(|o| o.content)
}

// ---- Bash ----

#[tokio::test]
async fn bash_success() {
    let out = run(
        &tao_core::tools::bash::BashTool,
        &json!({"command": ["echo", "hello"]}),
        ".",
    )
    .await
    .unwrap();
    assert!(out.contains("Exit: 0"));
    assert!(out.contains("hello"));
}

#[tokio::test]
async fn bash_nonzero_exit() {
    let out = run(
        &tao_core::tools::bash::BashTool,
        &json!({"command": ["bash", "-c", "exit 3"]}),
        ".",
    )
    .await
    .unwrap();
    assert!(out.contains("Exit: 3"));
}

#[tokio::test]
async fn bash_stderr_captured() {
    let out = run(
        &tao_core::tools::bash::BashTool,
        &json!({"command": ["bash", "-c", "echo to-err >&2; echo to-out"]}),
        ".",
    )
    .await
    .unwrap();
    assert!(out.contains("to-out"));
    assert!(out.contains("to-err"));
}

#[tokio::test]
async fn bash_timeout() {
    let tool = &tao_core::tools::bash::BashTool;
    let ctx = ToolCtx::new(".");
    let args = json!({"command": ["bash", "-c", "sleep 30"], "timeout_ms": 100});
    let err = tool.call(&args, &ctx).await.unwrap_err();
    assert!(matches!(err, ToolError::Timeout(100)), "got: {err:?}");
}

#[tokio::test]
async fn bash_cancellation() {
    let tool = &tao_core::tools::bash::BashTool;
    let cancel = CancellationToken::new();
    let ctx = ToolCtx::with_cancel(".", cancel.clone());
    let args = json!({"command": ["bash", "-c", "sleep 30"], "timeout_ms": 60000});

    let handle = tokio::spawn(async move { tool.call(&args, &ctx).await });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    cancel.cancel();

    let err = handle.await.unwrap().unwrap_err();
    assert!(matches!(err, ToolError::Cancelled), "got: {err:?}");
}

#[tokio::test]
async fn bash_output_truncation() {
    // 生成 50k 行(每行 ~10 字符 = 500k+ 字符,远超 head+tail 阈值)
    let out = run(
        &tao_core::tools::bash::BashTool,
        &json!({"command": ["bash", "-c", "seq 1 50000"]}),
        ".",
    )
    .await
    .unwrap();
    assert!(out.contains("[... truncated ...]"), "应有截断标记");
    // 头部保留 seq 的前几行
    assert!(out.contains("1\n"));
    // 尾部保留最后一行
    assert!(out.contains("50000\n"));
}

#[tokio::test]
async fn bash_command_must_be_array() {
    let err = run(
        &tao_core::tools::bash::BashTool,
        &json!({"command": "echo hello"}),
        ".",
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs(_)));
}

// ---- Read ----

#[tokio::test]
async fn read_with_line_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    tokio::fs::write(&path, "line1\nline2\nline3\n")
        .await
        .unwrap();

    let tool = &tao_core::tools::fs::ReadTool;
    let ctx = ToolCtx::new(dir.path());
    let args = json!({"path": "a.txt"});
    let out = tool.call(&args, &ctx).await.unwrap().content;
    assert!(out.contains("     1\tline1"));
    assert!(out.contains("     2\tline2"));
    assert!(out.contains("[1-3 of 3 lines]"));
}

#[tokio::test]
async fn read_offset_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.txt");
    let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
    tokio::fs::write(&path, content).await.unwrap();

    let tool = &tao_core::tools::fs::ReadTool;
    let ctx = ToolCtx::new(dir.path());
    let args = json!({"path": "b.txt", "offset": 3, "limit": 2});
    let out = tool.call(&args, &ctx).await.unwrap().content;
    assert!(out.contains("     3\tline3"));
    assert!(out.contains("     4\tline4"));
    assert!(!out.contains("line5"));
    assert!(out.contains("[3-4 of 10 lines]"));
}

#[tokio::test]
async fn read_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let tool = &tao_core::tools::fs::ReadTool;
    let ctx = ToolCtx::new(dir.path());
    let args = json!({"path": "nope.txt"});
    let err = tool.call(&args, &ctx).await.unwrap_err();
    match err {
        ToolError::Failed(msg) => assert!(msg.contains("文件不存在")),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn read_directory_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    tokio::fs::create_dir(&sub).await.unwrap();

    let tool = &tao_core::tools::fs::ReadTool;
    let ctx = ToolCtx::new(dir.path());
    let args = json!({"path": "sub"});
    let out = tool.call(&args, &ctx).await.unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("目录"));
}

#[tokio::test]
async fn read_binary_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bin.dat");
    tokio::fs::write(&path, b"\x00\x01\x02 binary \xff")
        .await
        .unwrap();

    let tool = &tao_core::tools::fs::ReadTool;
    let ctx = ToolCtx::new(dir.path());
    let args = json!({"path": "bin.dat"});
    let out = tool.call(&args, &ctx).await.unwrap().content;
    assert!(out.contains("二进制文件"));
}

// ---- Write ----

#[tokio::test]
async fn write_creates_file() {
    let dir = tempfile::tempdir().unwrap();
    let tool = &tao_core::tools::fs::WriteTool;
    let ctx = ToolCtx::new(dir.path());
    let args = json!({"path": "new.txt", "content": "hello\n"});
    let out = tool.call(&args, &ctx).await.unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("created"));

    let written = tokio::fs::read_to_string(dir.path().join("new.txt"))
        .await
        .unwrap();
    assert_eq!(written, "hello\n");
}

#[tokio::test]
async fn write_overwrites_existing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("exist.txt");
    tokio::fs::write(&path, "old").await.unwrap();

    let tool = &tao_core::tools::fs::WriteTool;
    let ctx = ToolCtx::new(dir.path());
    let args = json!({"path": "exist.txt", "content": "new"});
    let out = tool.call(&args, &ctx).await.unwrap();
    assert!(out.content.contains("overwrote"));

    let written = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(written, "new");
}

#[tokio::test]
async fn write_creates_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let tool = &tao_core::tools::fs::WriteTool;
    let ctx = ToolCtx::new(dir.path());
    let args = json!({"path": "a/b/c.txt", "content": "nested"});
    tool.call(&args, &ctx).await.unwrap();

    let written = tokio::fs::read_to_string(dir.path().join("a/b/c.txt"))
        .await
        .unwrap();
    assert_eq!(written, "nested");
}

// ---- Registry ----

#[tokio::test]
async fn registry_builtin_tools() {
    let reg = ToolRegistry::builtin();
    let names = reg.names();
    assert!(names.contains(&"Bash"));
    assert!(names.contains(&"Read"));
    assert!(names.contains(&"Write"));

    let specs = reg.specs();
    assert_eq!(specs.len(), 3);
    assert!(specs.iter().all(|s| !s.schema.is_null()));
}

#[tokio::test]
async fn registry_dispatch_by_name() {
    let reg = ToolRegistry::builtin();
    let tool = reg.get("Bash").expect("Bash registered");
    let ctx = ToolCtx::new(".");
    let args = json!({"command": ["echo", "dispatched"]});
    let out = tool.call(&args, &ctx).await.unwrap();
    assert!(out.content.contains("dispatched"));
}
