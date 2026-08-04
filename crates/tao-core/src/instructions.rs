//! AGENTS.md 指令文件:层级发现 + 合并 + `@path` 引用展开(见 docs/design/config.md §4)。
//!
//! - 发现 `~/.tao/AGENTS.md`(全局)+ 从 repo 根到 `<cwd>` 沿途每一级的 `AGENTS.md`
//!   (兼容 `CLAUDE.md`/`TAO.md`,AGENTS.md 优先)。合并顺序:全局 → repo 根 → … → cwd
//!   (后写优先,模型天然更重视靠后指令)。
//! - `@path/to/file` 引用就地展开为文件内容(递归,深度限制 8,防循环)。
//!   `@` 前必须是空白或行首;路径相对该指令文件所在目录。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 指令文件候选名(按优先级):AGENTS.md(社区通用)> CLAUDE.md > TAO.md(老名)。
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", "TAO.md"];
/// `@path` 递归展开深度上限,防循环引用。
const MAX_AT_DEPTH: u32 = 8;

/// 发现并加载指令文件,返回合并 + `@path` 展开后的指令文本。
///
/// 返回 `None` 表示无任何指令文件。调用方把它作为 system 的前置"指令区"。
pub fn load(cwd: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    // 全局 ~/.tao/AGENTS.md(兼容老名)
    if let Some(home) = std::env::var_os("HOME") {
        let global_dir = PathBuf::from(home).join(".tao");
        if let Some(text) = read_first(&global_dir) {
            parts.push(expand_at_refs(&text, &global_dir, 0));
        }
    }

    // 从 repo 根到 cwd 沿途每一级 AGENTS.md(根在前)
    for dir in dirs_from_repo_root_to(cwd) {
        if let Some(text) = read_first(&dir) {
            parts.push(expand_at_refs(&text, &dir, 0));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// 从最近的 `.git` 祖先(含 cwd)到 cwd 的目录链(根在前)。
/// 无 `.git` 时只返回 `[cwd]`。
fn dirs_from_repo_root_to(cwd: &Path) -> Vec<PathBuf> {
    // 找最近的 .git 祖先作为 repo 根
    let mut root = cwd;
    let mut cur = Some(cwd);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            root = d;
            break;
        }
        cur = d.parent();
    }
    // 从 cwd 回溯到 root,收集,反转成 根→cwd
    let mut up: Vec<PathBuf> = Vec::new();
    let mut c = cwd;
    loop {
        up.push(c.to_path_buf());
        if c == root {
            break;
        }
        match c.parent() {
            Some(p) => c = p,
            None => break,
        }
    }
    up.reverse();
    up
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

/// 展开 `@path` 引用为文件内容(递归)。`@` 前须是空白或行首。
/// 路径相对 `base_dir`(指令文件所在目录)。失败(不存在/读错)则原样保留 token。
fn expand_at_refs(text: &str, base_dir: &Path, depth: u32) -> String {
    if depth > MAX_AT_DEPTH {
        return text.to_string();
    }
    let mut out = String::new();
    // 已展开文件集,防循环引用(A→B→A)
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for line in text.lines() {
        for token in line.split_whitespace() {
            let Some(path_str) = token.strip_prefix('@').filter(|s| !s.is_empty()) else {
                out.push_str(token);
                out.push(' ');
                continue;
            };
            let p = base_dir.join(path_str);
            let Ok(canon) = std::fs::canonicalize(&p) else {
                out.push_str(token);
                out.push(' ');
                continue;
            };
            // let-chain:文件存在 + 未循环 + 读取成功 → 展开为内容
            if p.is_file()
                && seen.insert(canon)
                && let Ok(content) = std::fs::read_to_string(&p)
            {
                let parent = p.parent().unwrap_or(base_dir);
                out.push_str(&expand_at_refs(&content, parent, depth + 1));
                out.push('\n');
            } else {
                out.push_str(token);
                out.push(' ');
            }
        }
        out.push('\n');
    }
    out
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

    #[test]
    fn at_ref_expands_file_content() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("notes.md"), "重要约定").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "参见 @notes.md 结束").unwrap();
        let text = load(dir.path()).unwrap();
        assert!(text.contains("重要约定"));
        assert!(!text.contains("@notes.md"));
    }

    #[test]
    fn at_ref_recursive() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("b.md"), "B内容").unwrap();
        fs::write(dir.path().join("a.md"), "A开头 @b.md A尾").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "@a.md").unwrap();
        let text = load(dir.path()).unwrap();
        assert!(text.contains("A开头"));
        assert!(text.contains("B内容"));
        assert!(text.contains("A尾"));
    }

    #[test]
    fn at_ref_cycle_breaks() {
        let dir = TempDir::new().unwrap();
        // a 引用 b,b 引用 a —— canonicalize seen 防循环
        fs::write(dir.path().join("a.md"), "A @b.md").unwrap();
        fs::write(dir.path().join("b.md"), "B @a.md").unwrap();
        fs::write(dir.path().join("AGENTS.md"), "@a.md").unwrap();
        let text = load(dir.path()).unwrap();
        // 应终止,不无限递归
        assert!(text.contains("A"));
        assert!(text.contains("B"));
    }

    #[test]
    fn at_ref_missing_kept_literal() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "参见 @nonexistent.md").unwrap();
        let text = load(dir.path()).unwrap();
        // 不存在的 @path 原样保留
        assert!(text.contains("@nonexistent.md"));
    }

    #[test]
    fn dirs_from_repo_root_walks_up() {
        let dir = TempDir::new().unwrap();
        // repo 根
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join("AGENTS.md"), "root").unwrap();
        // 子目录
        let sub = dir.path().join("crates").join("tao-core");
        fs::create_dir_all(&sub).unwrap();
        let chain = dirs_from_repo_root_to(&sub);
        assert_eq!(chain[0], dir.path());
        assert_eq!(*chain.last().unwrap(), sub);
        // load 在子目录应合并 root + 子级(若有)
        fs::write(sub.join("AGENTS.md"), "sub").unwrap();
        let text = load(&sub).unwrap();
        assert!(text.contains("root"));
        assert!(text.contains("sub"));
    }
}
