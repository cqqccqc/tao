//! xtask:本地与 CI 共用的任务入口。`cargo ci`(alias)即 `cargo xtask ci`。

use std::process::Command;

fn main() -> anyhow::Result<()> {
    let task = std::env::args().nth(1).unwrap_or_else(|| "ci".into());
    match task.as_str() {
        "ci" => ci(),
        other => {
            eprintln!("未知任务: {other}\n可用: ci");
            std::process::exit(2);
        }
    }
}

fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    println!("$ {program} {}", args.join(" "));
    let status = Command::new(program).args(args).status()?;
    anyhow::ensure!(status.success(), "`{program} {}` 失败", args.join(" "));
    Ok(())
}

fn ci() -> anyhow::Result<()> {
    run("cargo", &["fmt", "--all", "--check"])?;
    run(
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run("cargo", &["test", "--workspace"])?;
    println!("ci: 全部通过");
    Ok(())
}
