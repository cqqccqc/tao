//! AGENTS.md 指令文件:层级发现 + 合并(见 docs/design/config.md §4)。
//!
//! v1 实现:
//! - 发现 `~/.tao/AGENTS.md`(全局)+ `<cwd>/AGENTS.md`(项目),兼容 `CLAUDE.md`/`TAO.md`
//!   (AGENTS.md 优先;老名仅 fallback,降低迁移成本)。
//! - 合并顺序:全局 → 项目(后写优先,模型天然更重视靠后指令)。
//! - TODO(留后续):向上遍历找 repo 根、子目录惰性追加、`@path` 引用展开、
//!   `config_fingerprint` hash 集(resume 漂移检测,依赖会话持久化)。

use std::path::{Path, PathBuf};

/// 指令文件候选名(按优先级):AGENTS.md(社区通用)> CLAUDE.md > TAO.md(老名)。
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", "TAO.md"];

/// 发现并加载指令文件,返回合并后的指令文本(全局 + 项目)。
///
/// 返回 `None` 表示无任何指令文件。调用方把它作为 system 的前置"指令区"。
pub fn load(cwd: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    // 全局 ~/.tao/AGENTS.md(兼容老名)
    if let Some(home) = std::env::var_os("HOME") {
        let global_dir = PathBuf::from(home).join(".tao");
        if let Some(text) = read_first(&global_dir) {
            parts.push(text);
        }
    }

    // 项目 <cwd>/AGENTS.md(兼容老名)
    if let Some(text) = read_first(cwd) {
        parts.push(text);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// 在 `dir` 中按优先级找第一个存在的指令文件,读其内容(带来源标注)。
fn read_first(dir: &Path) -> Option<String> {
    for name in INSTRUCTION_FILES {
        let path = dir.join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            return Some(format!("# 来自 {}\n{}", path.display(), text));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn load_project_agents_md() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "用 4 空格缩进").unwrap();
        let text = load(dir.path()).unwrap();
        assert!(text.contains("用 4 空格缩进"));
        assert!(text.contains("AGENTS.md"));
    }

    #[test]
    fn load_fallback_claude_md() {
        let dir = TempDir::new().unwrap();
        // 无 AGENTS.md,只有 CLAUDE.md → fallback 读取
        fs::write(dir.path().join("CLAUDE.md"), "项目约定").unwrap();
        let text = load(dir.path()).unwrap();
        assert!(text.contains("项目约定"));
        assert!(text.contains("CLAUDE.md"));
    }

    #[test]
    fn agents_md_preferred_over_claude_md() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "新指令").unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "老指令").unwrap();
        let text = load(dir.path()).unwrap();
        assert!(text.contains("新指令"));
        assert!(!text.contains("老指令"));
    }
}
