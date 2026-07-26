//! # tao-exec
//!
//! headless 单次执行:`tao exec "fix the tests"`。M1 实现。
//! Ask 审批默认 deny(`--on-ask approve` 可改),保证脚本可预期。

/// `tao exec` 入口(M1 实现)。
pub async fn run(_prompt: &str, _json: bool) -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "tao exec 尚未实现(M1),见 docs/design/architecture.md"
    ))
}
