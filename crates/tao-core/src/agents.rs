//! 子 agent 定义:markdown frontmatter + 正文(见 extensibility.md §2)。
//!
//! `~/.tao/agents/<name>.md` 或 `<cwd>/.tao/agents/<name>.md`。
//! 通过 `Task { subagent, prompt }` 调用,独立会话(只读权限),返回报告。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 子 agent 定义(frontmatter + system_prompt)。
#[derive(Debug, Clone)]
pub struct SubagentDef {
    pub name: String,
    pub description: String,
    /// 允许的工具名(省略 = 默认只读 Read/Grep/Glob)。
    pub tools: Vec<String>,
    /// 覆盖模型(省略 = 继承父级)。
    pub model: Option<String>,
    pub system_prompt: String,
}

/// 发现并加载子 agent:全局 `~/.tao/agents/` + 项目 `<cwd>/.tao/agents/`(项目优先)。
pub fn load_agents(cwd: &Path) -> Vec<SubagentDef> {
    let mut agents: HashMap<String, SubagentDef> = HashMap::new();
    if let Some(home) = std::env::var_os("HOME") {
        let dir = PathBuf::from(home).join(".tao").join("agents");
        load_dir(&dir, &mut agents);
    }
    let proj_dir = cwd.join(".tao").join("agents");
    load_dir(&proj_dir, &mut agents);
    agents.into_values().collect()
}

fn load_dir(dir: &Path, agents: &mut HashMap<String, SubagentDef>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_some_and(|x| x == "md")
            && let Some(def) = parse_agent(&path)
        {
            agents.insert(def.name.clone(), def);
        }
    }
}

fn parse_agent(path: &Path) -> Option<SubagentDef> {
    let mut name = path.file_stem()?.to_string_lossy().to_string();
    let content = std::fs::read_to_string(path).ok()?;
    let (fm, body) = split_frontmatter(&content);
    let mut description = String::new();
    let mut tools = Vec::new();
    let mut model = None;
    for line in fm.lines() {
        if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("tools:") {
            // [Read, Grep, Glob] 或 Read, Grep, Glob
            let v = v.trim().trim_start_matches('[').trim_end_matches(']');
            tools = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if let Some(v) = line.strip_prefix("model:") {
            model = Some(v.trim().trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("name:") {
            // frontmatter name 覆盖文件名
            let n = v.trim().trim_matches('"').to_string();
            if !n.is_empty() {
                name = n;
            }
        }
    }
    let system_prompt = body.trim().to_string();
    if description.is_empty() {
        description = system_prompt.lines().next().unwrap_or("").to_string();
    }
    if tools.is_empty() {
        tools = vec!["Read".into(), "Grep".into(), "Glob".into()];
    }
    Some(SubagentDef {
        name,
        description,
        tools,
        model,
        system_prompt,
    })
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
    fn load_agents_from_dir() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join(".tao").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("explorer.md"),
            "---\nname: explorer\ndescription: 只读探索\ntools: [Read, Grep, Glob]\nmodel: haiku\n---\n你是探索专家。\n",
        )
        .unwrap();
        let agents = load_agents(dir.path());
        let exp = agents.iter().find(|a| a.name == "explorer");
        assert!(exp.is_some(), "应加载 explorer");
        let exp = exp.unwrap();
        assert_eq!(exp.description, "只读探索");
        assert_eq!(exp.tools, vec!["Read", "Grep", "Glob"]);
        assert_eq!(exp.model.as_deref(), Some("haiku"));
        assert!(exp.system_prompt.contains("探索专家"));
    }

    #[test]
    fn default_tools_when_empty() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join(".tao").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("basic.md"),
            "---\ndescription: basic\n---\n系统提示\n",
        )
        .unwrap();
        let agents = load_agents(dir.path());
        let basic = agents.iter().find(|a| a.name == "basic").unwrap();
        assert_eq!(basic.tools, vec!["Read", "Grep", "Glob"]);
    }
}
