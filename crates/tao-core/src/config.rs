//! 分层配置:默认 < 用户(~/.tao/config.toml)< 项目(.tao/config.toml)
//! < 环境变量(TAO_*)< CLI(-c key=value / --profile)。
//! 见 docs/design/config.md。

use std::collections::HashMap;
use std::path::PathBuf;

use crate::hooks::HookConfig;
use serde::{Deserialize, Serialize};
use tao_protocol::permission::{PermissionMode, PermissionRule};

/// wire 协议三选一。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireApi {
    Anthropic,
    /// OpenAI Responses API(OpenAI 首选)。
    OpenaiResponses,
    /// OpenAI Chat Completions(覆盖 DeepSeek/Qwen/Kimi/OpenRouter 等兼容生态)。
    OpenaiChat,
}

/// Anthropic provider 的认证方式。默认 ApiKey(Anthropic 原生)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AnthropicAuth {
    /// `x-api-key: <key>`(Anthropic 原生)。
    #[default]
    ApiKey,
    /// `Authorization: Bearer <token>`(OAuth / 代理网关)。
    Bearer,
}

/// 单个 provider 的配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    #[serde(default)]
    pub name: String,
    pub base_url: String,
    pub wire_api: WireApi,
    /// 读 API key 的环境变量名。
    pub env_key: String,
    /// Anthropic provider 的认证方式(仅 wire_api=anthropic 时生效)。
    #[serde(default)]
    pub anthropic_auth: AnthropicAuth,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

/// 顶层配置。对应 ~/.tao/config.toml。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub permission_mode: PermissionMode,
    pub small_model: Option<String>,
    pub reasoning_effort: Option<tao_protocol::content::ReasoningEffort>,
    pub auto_compact_at: f32,
    pub max_turn_steps: u32,
    pub exec_timeout_ms: u64,
    pub editor: String,
    pub request_max_retries: u32,
    pub stream_idle_timeout_ms: u64,
    pub model_providers: HashMap<String, ModelProviderConfig>,
    pub profiles: HashMap<String, PartialConfig>,
    pub sessions: SessionsConfig,
    pub permissions: PermissionsConfig,
    pub hooks: HooksConfig,
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".into(),
            ModelProviderConfig {
                name: "Anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                wire_api: WireApi::Anthropic,
                env_key: "ANTHROPIC_API_KEY".into(),
                anthropic_auth: AnthropicAuth::ApiKey,
                headers: HashMap::new(),
            },
        );
        providers.insert(
            "openai".into(),
            ModelProviderConfig {
                name: "OpenAI".into(),
                base_url: "https://api.openai.com".into(),
                wire_api: WireApi::OpenaiResponses,
                env_key: "OPENAI_API_KEY".into(),
                anthropic_auth: AnthropicAuth::default(),
                headers: HashMap::new(),
            },
        );
        Self {
            model: None,
            model_provider: None,
            permission_mode: PermissionMode::Default,
            small_model: None,
            reasoning_effort: None,
            auto_compact_at: 0.92,
            max_turn_steps: 100,
            exec_timeout_ms: 120_000,
            editor: "vim".into(),
            request_max_retries: 4,
            stream_idle_timeout_ms: 60_000,
            model_providers: providers,
            sessions: SessionsConfig::default(),
            profiles: HashMap::new(),
            permissions: PermissionsConfig::default(),
            hooks: HooksConfig::default(),
            mcp_servers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionsConfig {
    pub keep_days: u32,
    pub max_session_mb: u64,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            keep_days: 30,
            max_session_mb: 50,
        }
    }
}

/// MCP server 配置(`[mcp_servers.<name>]`)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub startup_timeout_ms: u64,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            startup_timeout_ms: 10_000,
        }
    }
}

/// 权限规则配置(`[permissions]` 表)。
/// 只放 `rules`;权限模式用顶层 `permission_mode`(向后兼容,已有完整加载链)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub rules: Vec<PermissionRule>,
}

/// hooks 配置(`[hooks]` 表,按事件点)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct HooksConfig {
    pub pre_tool_use: Vec<HookConfig>,
    pub post_tool_use: Vec<HookConfig>,
    pub session_start: Vec<HookConfig>,
    pub session_end: Vec<HookConfig>,
    pub stop: Vec<HookConfig>,
}

impl HooksConfig {
    /// 合并(各事件点 Vec append,用户级 + 项目级都生效)。
    pub fn merge(&mut self, other: &HooksConfig) {
        self.pre_tool_use.extend(other.pre_tool_use.iter().cloned());
        self.post_tool_use
            .extend(other.post_tool_use.iter().cloned());
        self.session_start
            .extend(other.session_start.iter().cloned());
        self.session_end.extend(other.session_end.iter().cloned());
        self.stop.extend(other.stop.iter().cloned());
    }

    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.session_start.is_empty()
            && self.session_end.is_empty()
            && self.stop.is_empty()
    }
}

/// profile 覆盖:任意顶层字段的可选版本。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PartialConfig {
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub small_model: Option<String>,
    pub reasoning_effort: Option<tao_protocol::content::ReasoningEffort>,
    pub auto_compact_at: Option<f32>,
    pub max_turn_steps: Option<u32>,
    pub exec_timeout_ms: Option<u64>,
    pub editor: Option<String>,
    pub request_max_retries: Option<u32>,
    pub stream_idle_timeout_ms: Option<u64>,
}

/// CLI `-c key=value` 覆盖。
#[derive(Debug, Clone)]
pub struct CliOverride {
    pub key: String,
    pub value: String,
}

/// 配置加载选项。
#[derive(Debug, Clone, Default)]
pub struct LoadOpts {
    pub project_config: Option<PathBuf>,
    pub user_config: Option<PathBuf>,
    pub profile: Option<String>,
    pub overrides: Vec<CliOverride>,
}

impl Config {
    /// 按分层加载。
    pub fn load(opts: &LoadOpts) -> anyhow::Result<Self> {
        let mut config = Config::default();

        // 用户级
        let user_path = opts.user_config.clone().or_else(default_user_config);
        if let Some(p) = user_path.as_deref()
            && p.exists()
        {
            let text = std::fs::read_to_string(p)
                .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", p.display()))?;
            let partial: PartialFileConfig = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("解析 {} 失败: {e}", p.display()))?;
            config.merge_partial_file(&partial);
        }

        // 项目级
        let project_path = opts.project_config.clone().or_else(default_project_config);
        if let Some(p) = project_path.as_deref()
            && p.exists()
        {
            let text = std::fs::read_to_string(p)
                .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", p.display()))?;
            let partial: PartialFileConfig = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("解析 {} 失败: {e}", p.display()))?;
            config.merge_partial_file(&partial);
        }

        // 环境变量
        if let Ok(v) = std::env::var("TAO_MODEL")
            && !v.is_empty()
        {
            config.model = Some(v);
        }
        if let Ok(v) = std::env::var("TAO_PERMISSION_MODE")
            && !v.is_empty()
        {
            config.permission_mode = parse_permission_mode(&v)?;
        }
        if let Ok(v) = std::env::var("TAO_SMALL_MODEL")
            && !v.is_empty()
        {
            config.small_model = Some(v);
        }

        // profile 覆盖
        let profile_name = opts
            .profile
            .clone()
            .or_else(|| std::env::var("TAO_PROFILE").ok());
        if let Some(name) = &profile_name
            && let Some(partial) = config.profiles.get(name).cloned()
        {
            config.apply_partial(&partial);
        }

        // CLI 覆盖
        for ov in &opts.overrides {
            config.apply_override(ov)?;
        }

        Ok(config)
    }

    /// 解析当前 provider id:显式 > model 前缀。
    pub fn current_provider_id(&self) -> Option<String> {
        if let Some(p) = &self.model_provider {
            return Some(p.clone());
        }
        if let Some(m) = &self.model
            && let Some((provider, _)) = m.split_once('/')
        {
            return Some(provider.to_owned());
        }
        None
    }

    /// 解析当前模型 id(去掉 provider/ 前缀)。
    pub fn current_model_id(&self) -> Option<String> {
        let m = self.model.as_ref()?;
        if let Some((_, id)) = m.split_once('/') {
            Some(id.to_owned())
        } else {
            Some(m.clone())
        }
    }

    fn merge_partial_file(&mut self, p: &PartialFileConfig) {
        if let Some(v) = &p.model {
            self.model = Some(v.clone());
        }
        if let Some(v) = &p.model_provider {
            self.model_provider = Some(v.clone());
        }
        if let Some(v) = p.permission_mode {
            self.permission_mode = v;
        }
        if let Some(v) = &p.small_model {
            self.small_model = Some(v.clone());
        }
        if let Some(v) = p.reasoning_effort {
            self.reasoning_effort = Some(v);
        }
        if let Some(v) = p.auto_compact_at {
            self.auto_compact_at = v;
        }
        if let Some(v) = p.max_turn_steps {
            self.max_turn_steps = v;
        }
        if let Some(v) = p.exec_timeout_ms {
            self.exec_timeout_ms = v;
        }
        if let Some(v) = &p.editor {
            self.editor = v.clone();
        }
        if let Some(v) = p.request_max_retries {
            self.request_max_retries = v;
        }
        if let Some(v) = p.stream_idle_timeout_ms {
            self.stream_idle_timeout_ms = v;
        }
        for (k, v) in &p.model_providers {
            self.model_providers.insert(k.clone(), v.clone());
        }
        for (k, v) in &p.profiles {
            self.profiles.insert(k.clone(), v.clone());
        }
        if let Some(s) = &p.sessions {
            if let Some(v) = s.keep_days {
                self.sessions.keep_days = v;
            }
            if let Some(v) = s.max_session_mb {
                self.sessions.max_session_mb = v;
            }
        }
        if let Some(perm) = &p.permissions {
            self.permissions.rules.extend(perm.rules.iter().cloned());
        }
        if let Some(h) = &p.hooks {
            self.hooks.merge(h);
        }
        for (k, v) in &p.mcp_servers {
            self.mcp_servers.insert(k.clone(), v.clone());
        }
    }

    fn apply_partial(&mut self, p: &PartialConfig) {
        if let Some(v) = &p.model {
            self.model = Some(v.clone());
        }
        if let Some(v) = &p.model_provider {
            self.model_provider = Some(v.clone());
        }
        if let Some(v) = p.permission_mode {
            self.permission_mode = v;
        }
        if let Some(v) = &p.small_model {
            self.small_model = Some(v.clone());
        }
        if let Some(v) = p.reasoning_effort {
            self.reasoning_effort = Some(v);
        }
        if let Some(v) = p.auto_compact_at {
            self.auto_compact_at = v;
        }
        if let Some(v) = p.max_turn_steps {
            self.max_turn_steps = v;
        }
        if let Some(v) = p.exec_timeout_ms {
            self.exec_timeout_ms = v;
        }
        if let Some(v) = &p.editor {
            self.editor = v.clone();
        }
        if let Some(v) = p.request_max_retries {
            self.request_max_retries = v;
        }
        if let Some(v) = p.stream_idle_timeout_ms {
            self.stream_idle_timeout_ms = v;
        }
    }

    fn apply_override(&mut self, ov: &CliOverride) -> anyhow::Result<()> {
        match ov.key.as_str() {
            "model" => self.model = Some(ov.value.clone()),
            "model_provider" => self.model_provider = Some(ov.value.clone()),
            "permission_mode" => {
                self.permission_mode = parse_permission_mode(&ov.value)?;
            }
            "small_model" => self.small_model = Some(ov.value.clone()),
            "editor" => self.editor = ov.value.clone(),
            "max_turn_steps" => {
                self.max_turn_steps = ov
                    .value
                    .parse()
                    .map_err(|e| anyhow::anyhow!("max_turn_steps 无效: {e}"))?;
            }
            "exec_timeout_ms" => {
                self.exec_timeout_ms = ov
                    .value
                    .parse()
                    .map_err(|e| anyhow::anyhow!("exec_timeout_ms 无效: {e}"))?;
            }
            "auto_compact_at" => {
                self.auto_compact_at = ov
                    .value
                    .parse()
                    .map_err(|e| anyhow::anyhow!("auto_compact_at 无效: {e}"))?;
            }
            _ => {
                if let Some(rest) = ov.key.strip_prefix("model_providers.")
                    && let Some((id, field)) = rest.split_once('.')
                {
                    let entry = self
                        .model_providers
                        .entry(id.to_owned())
                        .or_insert_with(|| ModelProviderConfig {
                            name: id.to_owned(),
                            base_url: String::new(),
                            wire_api: WireApi::OpenaiChat,
                            env_key: String::new(),
                            anthropic_auth: AnthropicAuth::default(),
                            headers: HashMap::new(),
                        });
                    match field {
                        "base_url" => entry.base_url = ov.value.clone(),
                        "wire_api" => {
                            entry.wire_api = parse_wire_api(&ov.value)?;
                        }
                        "env_key" => entry.env_key = ov.value.clone(),
                        "name" => entry.name = ov.value.clone(),
                        _ => anyhow::bail!("不支持的 provider 字段: {field}"),
                    }
                } else {
                    anyhow::bail!("不支持的配置键: {}", ov.key);
                }
            }
        }
        Ok(())
    }
}

/// TOML 文件解析出的部分配置(所有字段可选,用于分层合并)。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct PartialFileConfig {
    model: Option<String>,
    model_provider: Option<String>,
    permission_mode: Option<PermissionMode>,
    small_model: Option<String>,
    reasoning_effort: Option<tao_protocol::content::ReasoningEffort>,
    auto_compact_at: Option<f32>,
    max_turn_steps: Option<u32>,
    exec_timeout_ms: Option<u64>,
    editor: Option<String>,
    request_max_retries: Option<u32>,
    stream_idle_timeout_ms: Option<u64>,
    model_providers: HashMap<String, ModelProviderConfig>,
    profiles: HashMap<String, PartialConfig>,
    sessions: Option<PartialSessionsConfig>,
    permissions: Option<PartialPermissionsConfig>,
    hooks: Option<HooksConfig>,
    mcp_servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct PartialPermissionsConfig {
    rules: Vec<PermissionRule>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct PartialSessionsConfig {
    keep_days: Option<u32>,
    max_session_mb: Option<u64>,
}

fn parse_permission_mode(s: &str) -> anyhow::Result<PermissionMode> {
    match s {
        "default" => Ok(PermissionMode::Default),
        "plan" => Ok(PermissionMode::Plan),
        "accept-edits" => Ok(PermissionMode::AcceptEdits),
        "bypass" => Ok(PermissionMode::Bypass),
        _ => anyhow::bail!("permission_mode 无效: {s}(可选: default/plan/accept-edits/bypass)"),
    }
}

fn parse_wire_api(s: &str) -> anyhow::Result<WireApi> {
    match s {
        "anthropic" => Ok(WireApi::Anthropic),
        "openai-responses" => Ok(WireApi::OpenaiResponses),
        "openai-chat" => Ok(WireApi::OpenaiChat),
        _ => anyhow::bail!("wire_api 无效: {s}(可选: anthropic/openai-responses/openai-chat)"),
    }
}

fn default_user_config() -> Option<PathBuf> {
    // 对齐 docs/design/config.md:~/.tao/config.toml(不走 XDG,与 codex/claude-code 风格一致)
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".tao").join("config.toml"))
}

fn default_project_config() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .map(|d| d.join(".tao").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_builtin_providers() {
        let c = Config::default();
        assert!(c.model_providers.contains_key("anthropic"));
        assert!(c.model_providers.contains_key("openai"));
        assert_eq!(
            c.model_providers["openai"].wire_api,
            WireApi::OpenaiResponses
        );
    }

    #[test]
    fn model_prefix_resolves_provider() {
        let c = Config {
            model: Some("anthropic/claude-sonnet-4-6".into()),
            ..Config::default()
        };
        assert_eq!(c.current_provider_id().as_deref(), Some("anthropic"));
        assert_eq!(c.current_model_id().as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn explicit_provider_overrides_prefix() {
        let c = Config {
            model: Some("anthropic/claude-sonnet-4-6".into()),
            model_provider: Some("openai".into()),
            ..Config::default()
        };
        assert_eq!(c.current_provider_id().as_deref(), Some("openai"));
    }

    #[test]
    fn partial_override_applies() {
        let mut c = Config {
            model: Some("anthropic/x".into()),
            ..Config::default()
        };
        let p = PartialConfig {
            model: Some("openai/gpt-4o".into()),
            permission_mode: Some(PermissionMode::Plan),
            ..Default::default()
        };
        c.apply_partial(&p);
        assert_eq!(c.model.as_deref(), Some("openai/gpt-4o"));
        assert_eq!(c.permission_mode, PermissionMode::Plan);
    }

    #[test]
    fn cli_override_model() {
        let mut c = Config::default();
        c.apply_override(&CliOverride {
            key: "model".into(),
            value: "openai/gpt-4o".into(),
        })
        .unwrap();
        assert_eq!(c.model.as_deref(), Some("openai/gpt-4o"));
    }

    #[test]
    fn cli_override_provider_field() {
        let mut c = Config::default();
        c.apply_override(&CliOverride {
            key: "model_providers.deepseek.base_url".into(),
            value: "https://api.deepseek.com".into(),
        })
        .unwrap();
        assert_eq!(
            c.model_providers["deepseek"].base_url,
            "https://api.deepseek.com"
        );
        assert_eq!(c.model_providers["deepseek"].wire_api, WireApi::OpenaiChat);
    }

    #[test]
    fn permissions_rules_from_toml_and_merge() {
        let user = r#"
[[permissions.rules]]
tool = "Bash"
pattern = "cargo *"
action = "allow"
"#;
        let project = r#"
[[permissions.rules]]
tool = "Edit|Patch"
pattern = "src/generated/**"
action = "deny"
"#;
        let mut c = Config::default();
        c.merge_partial_file(&toml::from_str::<PartialFileConfig>(user).unwrap());
        c.merge_partial_file(&toml::from_str::<PartialFileConfig>(project).unwrap());
        assert_eq!(c.permissions.rules.len(), 2);
        assert_eq!(c.permissions.rules[0].pattern, "cargo *");
        assert_eq!(c.permissions.rules[1].tool, "Edit|Patch");
    }
}
