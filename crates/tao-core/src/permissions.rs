//! 权限引擎:模式 × 规则 × 会话决策三层判定 + 逃逸分析 + 审批 trait。
//! 见 docs/design/permissions.md。
//!
//! 判定流(在 `Tool::call` 之前):
//! ```text
//!   verdict = engine.decide(tool, key)
//!   Allow → 直接执行
//!   Deny  → ToolOutput::error("被权限策略拒绝")
//!   Ask   → approver.request() → Approve/ApproveForSession → 执行
//!                                  Deny  → ToolOutput::error("用户拒绝")
//!                                  Abort → 中断整个 turn
//! ```
//!
//! **v1 逃逸分析是"减少打扰"而非安全边界**(见 permissions.md §3):
//! 不可解析 ⇒ 升级 Ask(宁可打扰不放过)。真正的安全边界是 M5 的 OS 沙箱。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use tao_protocol::event::{ApprovalDetail, ApprovalKind};
use tao_protocol::ids::CallId;
use tao_protocol::op::ReviewDecision;
use tao_protocol::permission::{
    Decision, PermissionMode, PermissionRule, RuleAction, Verdict, VerdictSource,
};

// ---- 权限维度 ----

/// 工具调用的权限维度,由 `Tool::permission_key` 提取。
/// engine 据此选择匹配语义:Bash 前缀 glob / Path glob / Domain 字符串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionKey {
    /// Bash:原始 argv。逃逸分析(bash -lc 拆分、危险包装)在 engine 内做。
    Bash { command: Vec<String> },
    /// 文件路径工具(Read/Write/Edit):已相对 cwd 规范化的路径。
    Path { path: PathBuf },
    /// 网络工具(WebFetch):域名 host。
    Domain { host: String },
}

impl PermissionKey {
    /// 归一化为模式串:会话授权比对 + pattern_suggestion 共用。
    /// Bash → argv 空格 join;Path → 路径串;Domain → host。
    pub fn pattern_string(&self) -> String {
        match self {
            PermissionKey::Bash { command } => command.join(" "),
            PermissionKey::Path { path } => path.to_string_lossy().into_owned(),
            PermissionKey::Domain { host } => host.clone(),
        }
    }
}

// ---- 引擎 ----

/// 权限引擎:不可变快照(rules)+ 可变状态(mode、会话授权)。
/// 内部用 `Mutex` 提供可变性,故 `decide`/`grant`/`set_mode` 只需 `&self`,
/// 便于在 `Arc<PermissionEngine>` 上跨 turn / 跨 task 共享。
pub struct PermissionEngine {
    mode: Mutex<PermissionMode>,
    rules: Vec<PermissionRule>,
    session_grants: Mutex<HashSet<(String, String)>>,
}

impl PermissionEngine {
    pub fn new(mode: PermissionMode, rules: Vec<PermissionRule>) -> Self {
        Self {
            mode: Mutex::new(mode),
            rules,
            session_grants: Mutex::new(HashSet::new()),
        }
    }

    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock().unwrap()
    }

    /// 运行时切换模式(TUI shift+tab 用)。turn 之间生效。
    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().unwrap() = mode;
    }

    /// 记录一条会话级授权(`ApproveForSession`)。resume 时由持久化层重放恢复。
    pub fn grant(&self, tool: &str, pattern: &str) {
        self.session_grants
            .lock()
            .unwrap()
            .insert((tool.to_string(), pattern.to_string()));
    }

    /// 三层判定:`first_match(会话决策, 规则引擎, 模式默认值)`。
    pub fn decide(&self, tool: &str, key: Option<&PermissionKey>) -> Decision {
        let mode = *self.mode.lock().unwrap();

        // 1. 会话决策(ApproveForSession 累积)
        if let Some(k) = key {
            let pat = k.pattern_string();
            if self
                .session_grants
                .lock()
                .unwrap()
                .contains(&(tool.to_string(), pat))
            {
                return Decision {
                    verdict: Verdict::Allow,
                    source: VerdictSource::SessionGrant,
                };
            }
        }

        // 2. 规则引擎(含逃逸分析对 Bash 的拆分/升级)
        if let Some(d) = self.decide_by_rules(mode, tool, key) {
            return d;
        }

        // 3. 模式默认值
        Decision {
            verdict: mode_default(mode, tool),
            source: VerdictSource::ModeDefault { mode },
        }
    }

    /// 规则引擎判定。返回 `None` 表示无规则命中(回退模式默认值)。
    fn decide_by_rules(
        &self,
        mode: PermissionMode,
        tool: &str,
        key: Option<&PermissionKey>,
    ) -> Option<Decision> {
        match key {
            Some(PermissionKey::Bash { command }) => {
                let analysis = analyze_bash(command);
                // 危险包装 / 不可解析 → 强制 Ask(规则不覆盖)
                if analysis.dangerous_wrapper || analysis.unanalyzable {
                    return Some(Decision {
                        verdict: Verdict::Ask,
                        source: VerdictSource::ModeDefault { mode },
                    });
                }
                if analysis.segments.len() <= 1 {
                    return self.match_one(tool, key).map(|(action, rule)| Decision {
                        verdict: action_to_verdict(action),
                        source: VerdictSource::Rule { rule },
                    });
                }
                // 多段:取最严(全 allow 才 allow;任一 deny 则 deny;否则 ask)
                let mut agg: Option<Verdict> = None;
                let mut first_rule: Option<PermissionRule> = None;
                for seg in &analysis.segments {
                    let seg_argv: Vec<String> = seg.split_whitespace().map(String::from).collect();
                    let seg_key = PermissionKey::Bash { command: seg_argv };
                    let v = match self.match_one(tool, Some(&seg_key)) {
                        Some((action, rule)) => {
                            if first_rule.is_none() {
                                first_rule = Some(rule);
                            }
                            action_to_verdict(action)
                        }
                        None => mode_default(mode, tool),
                    };
                    agg = Some(aggregate_verdict(agg, v));
                }
                let verdict = agg.unwrap_or(Verdict::Ask);
                let source = first_rule
                    .map(|rule| VerdictSource::Rule { rule })
                    .unwrap_or(VerdictSource::ModeDefault { mode });
                Some(Decision { verdict, source })
            }
            _ => self.match_one(tool, key).map(|(action, rule)| Decision {
                verdict: action_to_verdict(action),
                source: VerdictSource::Rule { rule },
            }),
        }
    }

    /// 在所有规则中找命中最强者(deny > ask > allow;同 action 则 pattern 更长/更具体者优先)。
    fn match_one(
        &self,
        tool: &str,
        key: Option<&PermissionKey>,
    ) -> Option<(RuleAction, PermissionRule)> {
        let mut best: Option<(RuleAction, PermissionRule, usize)> = None;
        for rule in &self.rules {
            if !tool_matches(&rule.tool, tool) || !pattern_matches(&rule.pattern, key) {
                continue;
            }
            let plen = rule.pattern.len();
            let candidate = (rule.action, rule.clone(), plen);
            best = Some(match best {
                None => candidate,
                Some(b) => {
                    // action 大优先;同 action 则 pattern 更长(更具体)优先
                    if candidate.0 > b.0 || (candidate.0 == b.0 && candidate.2 > b.2) {
                        candidate
                    } else {
                        b
                    }
                }
            });
        }
        best.map(|(a, r, _)| (a, r))
    }
}

fn action_to_verdict(a: RuleAction) -> Verdict {
    match a {
        RuleAction::Allow => Verdict::Allow,
        RuleAction::Ask => Verdict::Ask,
        RuleAction::Deny => Verdict::Deny,
    }
}

/// 多段 Bash 聚合:全 Allow → Allow;任一 Deny → Deny;否则 Ask。
fn aggregate_verdict(agg: Option<Verdict>, v: Verdict) -> Verdict {
    match (agg, v) {
        (None, v) => v,
        (Some(Verdict::Deny), _) | (_, Verdict::Deny) => Verdict::Deny,
        (Some(Verdict::Allow), Verdict::Allow) => Verdict::Allow,
        _ => Verdict::Ask,
    }
}

// ---- 模式默认值 ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolClass {
    Read,
    Write,
    Exec,
    Net,
    /// 未知工具(MCP / 子 agent 等)。
    Other,
}

fn tool_class(tool: &str) -> ToolClass {
    match tool {
        "Bash" => ToolClass::Exec,
        "Read" => ToolClass::Read,
        "Write" | "Edit" | "Patch" => ToolClass::Write,
        "WebFetch" => ToolClass::Net,
        _ => ToolClass::Other,
    }
}

/// 模式默认值表(见 permissions.md §1)。
fn mode_default(mode: PermissionMode, tool: &str) -> Verdict {
    let class = tool_class(tool);
    match (mode, class) {
        (PermissionMode::Bypass, _) => Verdict::Allow,
        (PermissionMode::Plan, ToolClass::Read) => Verdict::Allow,
        (PermissionMode::Plan, _) => Verdict::Deny,
        (PermissionMode::AcceptEdits, ToolClass::Read)
        | (PermissionMode::AcceptEdits, ToolClass::Write) => Verdict::Allow,
        (PermissionMode::AcceptEdits, _) => Verdict::Ask,
        (PermissionMode::Default, ToolClass::Read) => Verdict::Allow,
        (PermissionMode::Default, _) => Verdict::Ask,
    }
}

// ---- 规则匹配 ----

/// rule.tool 支持 `|` 多选(如 "Edit|Patch")。
fn tool_matches(rule_tool: &str, tool: &str) -> bool {
    rule_tool.split('|').any(|t| t.trim() == tool)
}

/// pattern 按权限维度匹配:None(未知工具)永远不命中(回退模式默认)。
fn pattern_matches(pattern: &str, key: Option<&PermissionKey>) -> bool {
    let key = match key {
        Some(k) => k,
        None => return false,
    };
    match key {
        PermissionKey::Bash { command } => glob_match(pattern, &command.join(" ")),
        PermissionKey::Path { path } => glob_match(pattern, &path.to_string_lossy()),
        PermissionKey::Domain { host } => host == pattern || host.ends_with(&format!(".{pattern}")),
    }
}

fn glob_match(pattern: &str, s: &str) -> bool {
    match globset::Glob::new(pattern) {
        Ok(g) => g.compile_matcher().is_match(s),
        Err(_) => false,
    }
}

// ---- 逃逸分析(v1) ----

/// Bash 命令的静态分析结果。
#[derive(Debug, Clone)]
struct BashAnalysis {
    /// 规范化命令段;非 bash -lc 时为单段。
    segments: Vec<String>,
    /// 含 `$(...)`/反引号/引号内分隔符等不可静态解析结构 → 整条升级 Ask。
    unanalyzable: bool,
    /// sudo/env/xargs/find -exec/git -c 等危险包装 → 升级 Ask。
    dangerous_wrapper: bool,
}

fn analyze_bash(command: &[String]) -> BashAnalysis {
    let dangerous = is_dangerous_wrapper(command);

    if let Some(script) = extract_shell_script(command) {
        // 不可解析结构:$(...) / 反引号 / ${...}
        if script.contains("$(") || script.contains('`') || script.contains("${") {
            return BashAnalysis {
                segments: vec![command.join(" ")],
                unanalyzable: true,
                dangerous_wrapper: dangerous,
            };
        }
        let (segs, unanalyzable) = split_segments(&script);
        let segments: Vec<String> = segs
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        return BashAnalysis {
            segments,
            unanalyzable,
            dangerous_wrapper: dangerous,
        };
    }

    // 普通 argv(不经 shell):单段
    BashAnalysis {
        segments: vec![command.join(" ")],
        unanalyzable: false,
        dangerous_wrapper: dangerous,
    }
}

/// 检测 `bash -lc "<script>"` / `sh -c`,返回脚本串。
fn extract_shell_script(command: &[String]) -> Option<String> {
    let first = command.first()?;
    if !matches!(
        first.as_str(),
        "bash" | "sh" | "zsh" | "dash" | "ash" | "/bin/bash" | "/bin/sh"
    ) {
        return None;
    }
    for (i, a) in command.iter().enumerate() {
        if a == "-c" || a == "-lc" {
            return command.get(i + 1).cloned();
        }
    }
    None
}

/// 已知危险包装(见 permissions.md §3)。
fn is_dangerous_wrapper(command: &[String]) -> bool {
    let first = command.first().map(|s| s.as_str()).unwrap_or("");
    match first {
        "sudo" | "env" | "xargs" => true,
        "find" => command
            .iter()
            .any(|a| a == "-exec" || a.starts_with("-exec")),
        "git" => command.iter().any(|a| a == "-c"),
        _ => false,
    }
}

/// 按 `&&` / `||` / `;` / `|` / 换行拆分脚本。v1 不做完整 shell 词法:
/// 含引号且含分隔符时判定不可解析(保守升级)。
fn split_segments(script: &str) -> (Vec<String>, bool) {
    let has_quote = script.contains('"') || script.contains('\'');
    let has_sep = ['&', '|', ';', '\n'].iter().any(|c| script.contains(*c));
    if has_quote && has_sep {
        return (vec![], true);
    }

    let chars: Vec<char> = script.chars().collect();
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // 双字符分隔符优先
        if c == '&' && i + 1 < chars.len() && chars[i + 1] == '&' {
            segs.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        if c == '|' && i + 1 < chars.len() && chars[i + 1] == '|' {
            segs.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        // 单字符分隔符:管道 / 分号 / 换行
        if c == '|' || c == ';' || c == '\n' {
            segs.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    segs.push(cur);
    (segs, false)
}

// ---- 审批 ----

/// core 发给前端的一次审批请求(复用协议类型)。
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub call_id: CallId,
    pub tool: String,
    pub kind: ApprovalKind,
    pub detail: ApprovalDetail,
}

/// 审批器:Ask 判定时由 `run_turn` 调用,前端返回用户决定。
/// Allow/Deny 判定不走这里(直接执行 / 直接拒绝)。
#[async_trait]
pub trait Approver: Send + Sync {
    async fn request(&self, req: ApprovalRequest) -> ReviewDecision;
}

/// 构造审批请求。`rule_matched` 取自 `Decision::source`(命中的 ask 规则原文)。
pub fn approval_request(
    call_id: CallId,
    tool: &str,
    key: Option<&PermissionKey>,
    rule_matched: Option<&str>,
) -> ApprovalRequest {
    let detail = ApprovalDetail {
        rule_matched: rule_matched.map(String::from),
        command: match key {
            Some(PermissionKey::Bash { command }) => Some(command.clone()),
            _ => None,
        },
        files: match key {
            Some(PermissionKey::Path { path }) => Some(vec![path.clone()]),
            _ => None,
        },
        tool: Some(tool.to_string()),
        args_summary: None,
        pattern_suggestion: pattern_suggestion(tool, key),
    };
    ApprovalRequest {
        call_id,
        tool: tool.to_string(),
        kind: approval_kind(tool),
        detail,
    }
}

pub fn approval_kind(tool: &str) -> ApprovalKind {
    match tool {
        "Bash" => ApprovalKind::Exec,
        "Write" | "Edit" | "Patch" => ApprovalKind::Patch,
        _ => ApprovalKind::Tool,
    }
}

/// 建议的 allow 规则,审批 UI 可一键固化(本任务仅生成,固化写入留后续)。
pub fn pattern_suggestion(tool: &str, key: Option<&PermissionKey>) -> Option<String> {
    match key? {
        PermissionKey::Bash { command } => Some(format!("Bash({} *)", command.join(" "))),
        PermissionKey::Path { path } => Some(format!("{}({})", tool, path.display())),
        PermissionKey::Domain { host } => Some(format!("WebFetch({host})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tao_protocol::permission::PermissionRule;

    fn rule(tool: &str, pattern: &str, action: RuleAction) -> PermissionRule {
        PermissionRule {
            tool: tool.into(),
            pattern: pattern.into(),
            action,
        }
    }

    // ---- 模式默认值表 ----

    #[test]
    fn mode_default_matrix() {
        // Default:read=allow, 其余=ask
        assert_eq!(
            mode_default(PermissionMode::Default, "Read"),
            Verdict::Allow
        );
        assert_eq!(mode_default(PermissionMode::Default, "Bash"), Verdict::Ask);
        assert_eq!(mode_default(PermissionMode::Default, "Write"), Verdict::Ask);
        assert_eq!(
            mode_default(PermissionMode::Default, "WebFetch"),
            Verdict::Ask
        );
        // Plan:read=allow, 其余=deny(只读)
        assert_eq!(mode_default(PermissionMode::Plan, "Read"), Verdict::Allow);
        assert_eq!(mode_default(PermissionMode::Plan, "Bash"), Verdict::Deny);
        assert_eq!(mode_default(PermissionMode::Plan, "Write"), Verdict::Deny);
        // AcceptEdits:read/write=allow, exec/net=ask
        assert_eq!(
            mode_default(PermissionMode::AcceptEdits, "Write"),
            Verdict::Allow
        );
        assert_eq!(
            mode_default(PermissionMode::AcceptEdits, "Bash"),
            Verdict::Ask
        );
        // Bypass:全 allow
        assert_eq!(mode_default(PermissionMode::Bypass, "Bash"), Verdict::Allow);
        assert_eq!(
            mode_default(PermissionMode::Bypass, "Write"),
            Verdict::Allow
        );
        // 未知工具:Default=ask, Plan=deny
        assert_eq!(
            mode_default(PermissionMode::Default, "McpFoo"),
            Verdict::Ask
        );
        assert_eq!(mode_default(PermissionMode::Plan, "McpFoo"), Verdict::Deny);
    }

    // ---- 规则:deny > ask > allow ----

    #[test]
    fn deny_beats_allow() {
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![
                rule("Bash", "cargo *", RuleAction::Allow),
                rule("Bash", "cargo *", RuleAction::Deny),
            ],
        );
        let key = PermissionKey::Bash {
            command: vec!["cargo".into(), "test".into()],
        };
        let d = engine.decide("Bash", Some(&key));
        assert_eq!(d.verdict, Verdict::Deny);
    }

    #[test]
    fn ask_beats_allow() {
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![
                rule("Bash", "cargo *", RuleAction::Allow),
                rule("Bash", "rm *", RuleAction::Ask),
            ],
        );
        let key = PermissionKey::Bash {
            command: vec!["rm".into(), "-rf".into(), "x".into()],
        };
        assert_eq!(engine.decide("Bash", Some(&key)).verdict, Verdict::Ask);
    }

    // ---- 具体度优先 ----

    #[test]
    fn more_specific_pattern_wins() {
        // 同为 ask:更具体的 pattern("cargo test *")优先于("cargo *")
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![
                rule("Bash", "cargo *", RuleAction::Allow),
                rule("Bash", "cargo test *", RuleAction::Ask),
            ],
        );
        let key = PermissionKey::Bash {
            command: vec!["cargo".into(), "test".into(), "--release".into()],
        };
        // "cargo test --release" 同时命中 allow(cargo *)和 ask(cargo test *);
        // ask 更具体且 action 更高 → Ask
        assert_eq!(engine.decide("Bash", Some(&key)).verdict, Verdict::Ask);
    }

    #[test]
    fn tool_pipe_alternatives() {
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![rule("Edit|Patch", "src/**", RuleAction::Allow)],
        );
        let key = PermissionKey::Path {
            path: PathBuf::from("src/main.rs"),
        };
        assert_eq!(engine.decide("Edit", Some(&key)).verdict, Verdict::Allow);
        assert_eq!(engine.decide("Patch", Some(&key)).verdict, Verdict::Allow);
    }

    // ---- 路径 glob ----

    #[test]
    fn path_glob_deny() {
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![rule("Write", "src/generated/**", RuleAction::Deny)],
        );
        let key = PermissionKey::Path {
            path: PathBuf::from("src/generated/gen.rs"),
        };
        assert_eq!(engine.decide("Write", Some(&key)).verdict, Verdict::Deny);
        // 未命中 → 回退模式默认(Write in Default = Ask)
        let other = PermissionKey::Path {
            path: PathBuf::from("src/main.rs"),
        };
        assert_eq!(engine.decide("Write", Some(&other)).verdict, Verdict::Ask);
    }

    // ---- 域名 ----

    #[test]
    fn domain_match() {
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![rule("WebFetch", "docs.rs", RuleAction::Allow)],
        );
        let exact = PermissionKey::Domain {
            host: "docs.rs".into(),
        };
        assert_eq!(
            engine.decide("WebFetch", Some(&exact)).verdict,
            Verdict::Allow
        );
        let sub = PermissionKey::Domain {
            host: "sub.docs.rs".into(),
        };
        assert_eq!(
            engine.decide("WebFetch", Some(&sub)).verdict,
            Verdict::Allow
        );
    }

    // ---- 会话授权优先 ----

    #[test]
    fn session_grant_overrides_mode() {
        let engine = PermissionEngine::new(PermissionMode::Default, vec![]);
        let key = PermissionKey::Bash {
            command: vec!["cargo".into(), "test".into()],
        };
        // 未 grant → Default Bash = Ask
        assert_eq!(engine.decide("Bash", Some(&key)).verdict, Verdict::Ask);
        engine.grant("Bash", "cargo test");
        // grant 后 → Allow
        assert_eq!(engine.decide("Bash", Some(&key)).verdict, Verdict::Allow);
    }

    // ---- 逃逸分析 ----

    #[test]
    fn bash_plain_argv_matches_prefix() {
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![rule("Bash", "cargo *", RuleAction::Allow)],
        );
        let key = PermissionKey::Bash {
            command: vec!["cargo".into(), "test".into()],
        };
        assert_eq!(engine.decide("Bash", Some(&key)).verdict, Verdict::Allow);
    }

    #[test]
    fn bash_lc_splits_segments() {
        // cargo test && rm x:cargo 段 allow,rm 段无规则(回退 Default Bash=Ask)→ 聚合 Ask
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![rule("Bash", "cargo *", RuleAction::Allow)],
        );
        let key = PermissionKey::Bash {
            command: vec!["bash".into(), "-lc".into(), "cargo test && rm -f x".into()],
        };
        assert_eq!(engine.decide("Bash", Some(&key)).verdict, Verdict::Ask);
    }

    #[test]
    fn bash_lc_all_segments_allow_passes() {
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![
                rule("Bash", "cargo *", RuleAction::Allow),
                rule("Bash", "echo *", RuleAction::Allow),
            ],
        );
        let key = PermissionKey::Bash {
            command: vec![
                "bash".into(),
                "-lc".into(),
                "cargo test && echo done".into(),
            ],
        };
        assert_eq!(engine.decide("Bash", Some(&key)).verdict, Verdict::Allow);
    }

    #[test]
    fn bash_lc_unanalyzable_upgrades_to_ask() {
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![rule("Bash", "cargo *", RuleAction::Allow)],
        );
        let key = PermissionKey::Bash {
            command: vec![
                "bash".into(),
                "-lc".into(),
                "cargo test && $(rm -f x)".into(),
            ],
        };
        assert_eq!(engine.decide("Bash", Some(&key)).verdict, Verdict::Ask);
    }

    #[test]
    fn dangerous_wrapper_upgrades_to_ask() {
        let engine = PermissionEngine::new(
            PermissionMode::Default,
            vec![rule("Bash", "cargo *", RuleAction::Allow)],
        );
        // sudo 包装:即便 cargo 规则 allow,仍 Ask
        let key = PermissionKey::Bash {
            command: vec!["sudo".into(), "cargo".into(), "test".into()],
        };
        assert_eq!(engine.decide("Bash", Some(&key)).verdict, Verdict::Ask);
        // git -c
        let git_key = PermissionKey::Bash {
            command: vec!["git".into(), "-c".into(), "x=y".into(), "status".into()],
        };
        assert_eq!(engine.decide("Bash", Some(&git_key)).verdict, Verdict::Ask);
    }

    #[test]
    fn plan_mode_denies_write_even_if_rule_absent() {
        let engine = PermissionEngine::new(PermissionMode::Plan, vec![]);
        let key = PermissionKey::Path {
            path: PathBuf::from("src/main.rs"),
        };
        assert_eq!(engine.decide("Write", Some(&key)).verdict, Verdict::Deny);
    }

    // ---- pattern_suggestion ----

    #[test]
    fn suggestion_format() {
        let bash = PermissionKey::Bash {
            command: vec!["cargo".into(), "test".into()],
        };
        assert_eq!(
            pattern_suggestion("Bash", Some(&bash)).as_deref(),
            Some("Bash(cargo test *)")
        );
        let path = PermissionKey::Path {
            path: PathBuf::from("src/main.rs"),
        };
        assert_eq!(
            pattern_suggestion("Write", Some(&path)).as_deref(),
            Some("Write(src/main.rs)")
        );
        assert_eq!(pattern_suggestion("Bash", None), None);
    }

    #[test]
    fn set_mode_at_runtime() {
        let engine = PermissionEngine::new(PermissionMode::Default, vec![]);
        assert_eq!(engine.mode(), PermissionMode::Default);
        engine.set_mode(PermissionMode::Plan);
        assert_eq!(engine.mode(), PermissionMode::Plan);
        let key = PermissionKey::Bash {
            command: vec!["echo".into()],
        };
        assert_eq!(engine.decide("Bash", Some(&key)).verdict, Verdict::Deny);
    }
}
