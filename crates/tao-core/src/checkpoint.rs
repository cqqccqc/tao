//! shadow-git checkpoint:影子 git 仓库(bare)+ 文件快照 + 回滚(见 sessions.md §3)。
//!
//! v1:Edit/Write 前自动快照(touched files);/rollback 回滚文件。
//! 不碰用户 .git;纯本地永不 push;不 fork 对话(TODO)。

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// 影子 git 仓库(bare,~/.tao/projects/<slug>/shadow.git)。
pub struct ShadowRepo {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

impl ShadowRepo {
    pub fn init(cwd: &Path) -> Result<Self> {
        let slug = slugify(cwd);
        let git_dir = std::env::var_os("HOME")
            .map(|h| {
                PathBuf::from(h)
                    .join(".tao")
                    .join("projects")
                    .join(slug)
                    .join("shadow.git")
            })
            .context("HOME 未设置")?;

        std::fs::create_dir_all(&git_dir)?;
        // git init --bare 直接指定路径(不用 --git-dir/--work-tree,init 不支持)
        let output = Command::new("git")
            .args(["init", "--bare"])
            .arg(&git_dir)
            .output()
            .context("git init --bare 失败")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git init --bare 失败: {}", stderr.trim());
        }
        let exclude = git_dir.join("info").join("exclude");
        if let Some(parent) = exclude.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&exclude, ".env\n*.key\nsecrets.*\n*.pem\n")?;

        Ok(Self {
            git_dir,
            work_tree: cwd.to_path_buf(),
        })
    }

    pub fn snapshot(&self, files: &[PathBuf]) -> Result<Option<String>> {
        if files.is_empty() {
            return Ok(None);
        }
        let mut args = vec!["add", "-f"];
        for f in files {
            args.push(f.to_str().unwrap_or(""));
        }
        run_git(&self.git_dir, &self.work_tree, &args)?;

        let output = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(["commit", "-m", "tao checkpoint", "--allow-empty"])
            .output()
            .context("git commit 失败")?;
        if !output.status.success() {
            return Ok(None);
        }
        let hash = run_git(&self.git_dir, &self.work_tree, &["rev-parse", "HEAD"])?;
        Ok(Some(hash.trim().to_string()))
    }

    pub fn rollback(&self, commit: &str) -> Result<()> {
        run_git(
            &self.git_dir,
            &self.work_tree,
            &["checkout", commit, "--", "."],
        )?;
        Ok(())
    }
}

fn run_git(git_dir: &Path, work_tree: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("--work-tree")
        .arg(work_tree)
        .args(args)
        .output()
        .context(format!("git {} 失败", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} 失败: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn slugify(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .replace(['/', ' ', ':', '\\'], "-")
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    // shadow-git 测试需设 HOME(与并行测试冲突),通过 dogfood 手动验证:
    // exec Edit 文件 → sessions audit 有 Checkpoint → rollback 恢复
}
