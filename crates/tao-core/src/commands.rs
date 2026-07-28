//! slash 命令:markdown 模板 + 内置命令(见 extensibility.md §3)。
//!
//! markdown 模板(`~/.tao/commands/`、`<cwd>/.tao/commands/`):frontmatter + body,
//! body 中 `` !`cmd` `` 执行注入、`$ARGUMENTS` 替换。内置命令 core 实现。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use tao_protocol::permission::PermissionMode;

/// markdown 命令定义(frontmatter + body)。
#[derive(Debug, Clone)]
pub struct CommandDef {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub body: String,
}

/// 发现并加载命令:全局 `~/.tao/commands/` + 项目 `<cwd>/.tao/commands/`(项目优先,同名覆盖)。
pub fn load_commands(cwd: &Path) -> Vec<CommandDef> {
    let mut cmds: HashMap<String, CommandDef> = HashMap::new();
    if let Some(home) = std::env::var_os("HOME") {
        let dir = PathBuf::from(home).join(".tao").join("commands");
        load_dir(&dir, &mut cmds);
    }
    let proj_dir = cwd.join(".tao").join("commands");
    load_dir(&proj_dir, &mut cmds);
    cmds.into_values().collect()
}

fn load_dir(dir: &Path, cmds: &mut HashMap<String, CommandDef>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_some_and(|x| x == "md")
            && let Some(cmd) = parse_command(&path)
        {
            cmds.insert(cmd.name.clone(), cmd);
        }
    }
}

fn parse_command(path: &Path) -> Option<CommandDef> {
    let name = path.file_stem()?.to_string_lossy().to_string();
    let content = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = split_frontmatter(&content);
    let mut description = String::new();
    let mut argument_hint = None;
    for line in frontmatter.lines() {
        if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("argument_hint:") {
            argument_hint = Some(v.trim().trim_matches('"').to_string());
        }
    }
    if description.is_empty() {
        description = body.lines().next().unwrap_or("").to_string();
    }
    Some(CommandDef {
        name,
        description,
        argument_hint,
        body,
    })
}

/// 分离 frontmatter(`---` 块)与 body。无 frontmatter 则返回空 fm + 全文。
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

/// 展开 body:`` !`cmd` `` 执行注入 stdout + `$ARGUMENTS` 替换。
pub fn expand(body: &str, args: &str, cwd: &Path) -> String {
    let with_args = body.replace("$ARGUMENTS", args);
    let re = Regex::new(r"!`([^`]+)`").unwrap();
    re.replace_all(&with_args, |caps: &regex::Captures| {
        exec_command(&caps[1], cwd)
    })
    .to_string()
}

fn exec_command(cmd: &str, cwd: &Path) -> String {
    let out = std::process::Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        Ok(o) => format!(
            "[命令失败({}): {}]",
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(e) => format!("[命令执行失败: {e}]"),
    }
}

/// 内置命令。
#[derive(Debug, Clone)]
pub enum Builtin {
    Help,
    Clear,
    Mode(PermissionMode),
    /// `/mode` 无参 = 循环(等价 shift+tab)。
    ModeCycle,
    Compact,
    Sessions,
}

/// 解析内置命令。返回 `None` = 非内置(可能是 markdown 模板)。
pub fn parse_builtin(input: &str) -> Option<Builtin> {
    let input = input.trim();
    let cmd = input.split(' ').next().unwrap_or(input);
    match cmd {
        "/help" => Some(Builtin::Help),
        "/clear" => Some(Builtin::Clear),
        "/compact" => Some(Builtin::Compact),
        "/sessions" => Some(Builtin::Sessions),
        "/mode" => match input.split_once(' ') {
            Some((_, mode)) => match mode.trim() {
                "default" => Some(Builtin::Mode(PermissionMode::Default)),
                "plan" => Some(Builtin::Mode(PermissionMode::Plan)),
                "accept-edits" => Some(Builtin::Mode(PermissionMode::AcceptEdits)),
                _ => None,
            },
            None => Some(Builtin::ModeCycle),
        },
        _ => None,
    }
}

/// 从输入提取命令名与参数:`/commit --amend` → ("commit", "--amend")。
pub fn split_name_args(input: &str) -> (String, String) {
    let input = input.trim();
    let input = input.strip_prefix('/').unwrap_or(input);
    match input.split_once(' ') {
        Some((n, a)) => (n.to_string(), a.to_string()),
        None => (input.to_string(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_builtin_basic() {
        assert!(matches!(parse_builtin("/help"), Some(Builtin::Help)));
        assert!(matches!(parse_builtin("/clear"), Some(Builtin::Clear)));
        assert!(matches!(parse_builtin("/compact"), Some(Builtin::Compact)));
        assert!(matches!(
            parse_builtin("/sessions"),
            Some(Builtin::Sessions)
        ));
        assert!(matches!(
            parse_builtin("/mode plan"),
            Some(Builtin::Mode(PermissionMode::Plan))
        ));
        assert!(matches!(parse_builtin("/mode"), Some(Builtin::ModeCycle)));
        assert!(parse_builtin("/unknown").is_none());
        assert!(parse_builtin("/mode bad").is_none());
    }

    #[test]
    fn split_name_args_basic() {
        assert_eq!(
            split_name_args("/commit --amend"),
            ("commit".to_string(), "--amend".to_string())
        );
        assert_eq!(
            split_name_args("/help"),
            ("help".to_string(), String::new())
        );
    }

    #[test]
    fn expand_arguments() {
        let dir = TempDir::new().unwrap();
        let out = expand("args: $ARGUMENTS", "hello world", dir.path());
        assert!(out.contains("args: hello world"));
    }

    #[test]
    fn expand_command_injection() {
        let dir = TempDir::new().unwrap();
        let out = expand("result: !`echo hi`", "", dir.path());
        assert!(out.contains("result: hi"));
    }

    #[test]
    fn load_commands_from_dir() {
        let dir = TempDir::new().unwrap();
        let cmds_dir = dir.path().join(".tao").join("commands");
        std::fs::create_dir_all(&cmds_dir).unwrap();
        std::fs::write(
            cmds_dir.join("test.md"),
            "---\ndescription: test cmd\nargument_hint: \"[x]\"\n---\nbody $ARGUMENTS",
        )
        .unwrap();
        let cmds = load_commands(dir.path());
        let test = cmds.iter().find(|c| c.name == "test");
        assert!(test.is_some(), "应加载 test 命令");
        let test = test.unwrap();
        assert_eq!(test.description, "test cmd");
        assert_eq!(test.argument_hint.as_deref(), Some("[x]"));
        assert!(test.body.contains("body"));
    }

    #[test]
    fn split_frontmatter_no_fm() {
        let (fm, body) = split_frontmatter("just body\n");
        assert!(fm.is_empty());
        assert!(body.contains("just body"));
    }
}
