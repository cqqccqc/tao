//! 技能(SKILL.md 渐进披露,见 extensibility.md §5)。
//!
//! `~/.tao/skills/<name>/SKILL.md` 或 `<cwd>/.tao/skills/<name>/SKILL.md`。
//! 系统提示只注入 name+description;模型触发后自行 Read 正文(上下文按需加载)。
//! 技能 = 知识包(与子 agent 隔离执行区别)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 技能定义(frontmatter + SKILL.md 路径)。
#[derive(Debug, Clone)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// 发现并加载技能:全局 `~/.tao/skills/*/SKILL.md` + 项目 `<cwd>/.tao/skills/*/SKILL.md`(项目优先)。
pub fn load_skills(cwd: &Path) -> Vec<SkillDef> {
    let mut skills: HashMap<String, SkillDef> = HashMap::new();
    if let Some(home) = std::env::var_os("HOME") {
        let dir = PathBuf::from(home).join(".tao").join("skills");
        load_dir(&dir, &mut skills);
    }
    let proj_dir = cwd.join(".tao").join("skills");
    load_dir(&proj_dir, &mut skills);
    skills.into_values().collect()
}

fn load_dir(dir: &Path, skills: &mut HashMap<String, SkillDef>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let skill_dir = e.path();
        if skill_dir.is_dir() {
            let skill_md = skill_dir.join("SKILL.md");
            if skill_md.exists()
                && let Some(skill) = parse_skill(&skill_md)
            {
                skills.insert(skill.name.clone(), skill);
            }
        }
    }
}

fn parse_skill(path: &Path) -> Option<SkillDef> {
    let content = std::fs::read_to_string(path).ok()?;
    let (fm, _body) = split_frontmatter(&content);
    let mut name = path.parent()?.file_name()?.to_string_lossy().to_string();
    let mut description = String::new();
    for line in fm.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            let n = v.trim().trim_matches('"').to_string();
            if !n.is_empty() {
                name = n;
            }
        } else if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().trim_matches('"').to_string();
        }
    }
    if description.is_empty() {
        description = "（无描述）".into();
    }
    Some(SkillDef {
        name,
        description,
        path: path.to_path_buf(),
    })
}

/// 格式化技能列表为系统提示文本(None = 无技能)。
pub fn skills_prompt(skills: &[SkillDef]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut s = String::from("可用技能（触发后用 Read 工具读 SKILL.md 正文获取方法论):\n");
    for skill in skills {
        s.push_str(&format!(
            "- {}: {}\n  Read {}\n",
            skill.name,
            skill.description,
            skill.path.display()
        ));
    }
    Some(s)
}

/// 分离 frontmatter(`---` 块)与 body。
fn split_frontmatter(content: &str) -> (String, String) {
    let mut lines = content.lines();
    if lines.next().map(|l| l.trim()) != Some("---") {
        return (String::new(), content.to_string());
    }
    let mut fm = String::new();
    let mut body = String::new();
    let mut in_fm = true;
    for line in content.lines().skip(1) {
        if in_fm {
            if line.trim() == "---" {
                in_fm = false;
            } else {
                fm.push_str(line);
                fm.push('\n');
            }
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    (fm, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_skills_from_dir() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join(".tao").join("skills").join("rust-test");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: rust-test\ndescription: Rust 测试最佳实践\n---\n# 方法论\n",
        )
        .unwrap();
        let skills = load_skills(dir.path());
        let s = skills.iter().find(|s| s.name == "rust-test");
        assert!(s.is_some(), "应加载 rust-test");
        let s = s.unwrap();
        assert_eq!(s.description, "Rust 测试最佳实践");
        assert!(s.path.ends_with("SKILL.md"));
    }

    #[test]
    fn skills_prompt_format() {
        let skills = vec![SkillDef {
            name: "rust-test".into(),
            description: "Rust 测试".into(),
            path: PathBuf::from("/tmp/skills/rust-test/SKILL.md"),
        }];
        let p = skills_prompt(&skills).unwrap();
        assert!(p.contains("rust-test"));
        assert!(p.contains("Rust 测试"));
        assert!(p.contains("Read /tmp/skills/rust-test/SKILL.md"));
    }

    #[test]
    fn skills_prompt_empty() {
        assert!(skills_prompt(&[]).is_none());
    }
}
