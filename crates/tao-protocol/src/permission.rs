//! 权限模型类型:模式 / 规则 / 判定(见 docs/design/permissions.md)。

use serde::{Deserialize, Serialize};

/// 权限模式。plan 模式即一个权限 profile,不是特殊循环。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// 写/执行/网络需审批(除非规则允许)。
    Default,
    /// 只读:一切 mutation deny。
    Plan,
    /// 文件编辑自动允许,执行/网络仍需审批。
    AcceptEdits,
    /// 全部允许(需显式 flag 启动)。
    Bypass,
}

/// 规则动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Ask,
    Deny,
}

/// 一条权限规则:`(tool, pattern) → action`。
/// Bash pattern 对规范化命令做前缀 glob;Edit/Patch 用 globset 路径模式;
/// WebFetch 用域名匹配。具体匹配语义在 tao-core 的规则引擎中实现。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    /// 工具名,支持 `|` 多选(如 "Edit|Patch")。
    pub tool: String,
    /// 匹配模式(命令前缀 / 路径 glob / 域名)。
    pub pattern: String,
    pub action: RuleAction,
}

/// 规则写入范围(AddPermissionRule Op 用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    /// 仅本会话(内存,等价 ApproveForSession 的显式形式)。
    Session,
    /// 写入项目 .tao/config.toml。
    Project,
    /// 写入用户 ~/.tao/config.toml。
    User,
}

/// 权限判定结果(engine.decide 的返回)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Allow,
    Ask,
    Deny,
}

/// 判定来源(审计用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictSource {
    SessionGrant,
    Rule { rule: PermissionRule },
    ModeDefault { mode: PermissionMode },
}

/// 一次权限判定(协议 ApprovalDetail 与日志 PermissionDecision 共用)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub verdict: Verdict,
    pub source: VerdictSource,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_ordering_deny_highest() {
        // deny 优先于 ask 优先于 allow(规则引擎靠这个序取 max)
        assert!(RuleAction::Deny > RuleAction::Ask);
        assert!(RuleAction::Ask > RuleAction::Allow);
    }

    #[test]
    fn mode_kebab_serde() {
        assert_eq!(
            serde_json::to_string(&PermissionMode::AcceptEdits).unwrap(),
            "\"accept-edits\""
        );
    }
}
