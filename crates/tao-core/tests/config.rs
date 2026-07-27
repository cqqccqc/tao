//! 配置加载与 provider 解析测试。
//! 见 docs/design/config.md。

use std::io::Write;

use tao_core::config::{CliOverride, Config, LoadOpts, WireApi};
use tao_core::providers::registry::resolve;

fn write_config(dir: &std::path::Path, content: &str) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    path
}

#[test]
fn defaults_load_without_files() {
    // 无任何配置文件,默认值应有效
    let config = Config::load(&LoadOpts {
        user_config: Some("/nonexistent/user.toml".into()),
        project_config: Some("/nonexistent/project.toml".into()),
        ..Default::default()
    })
    .unwrap();
    assert!(config.model_providers.contains_key("anthropic"));
    assert!(config.model_providers.contains_key("openai"));
}

#[test]
fn user_config_overrides_defaults() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
model = "anthropic/claude-sonnet-4-6"
permission_mode = "plan"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com"
wire_api = "openai-chat"
env_key = "DEEPSEEK_API_KEY"
"#,
    );

    let config = Config::load(&LoadOpts {
        user_config: Some(dir.path().join("config.toml")),
        ..Default::default()
    })
    .unwrap();

    assert_eq!(config.model.as_deref(), Some("anthropic/claude-sonnet-4-6"));
    assert_eq!(
        config.permission_mode,
        tao_protocol::permission::PermissionMode::Plan
    );
    assert!(config.model_providers.contains_key("deepseek"));
    assert_eq!(
        config.model_providers["deepseek"].wire_api,
        WireApi::OpenaiChat
    );
}

#[test]
fn project_config_overrides_user() {
    let user_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();
    write_config(user_dir.path(), r#"model = "anthropic/a""#);
    write_config(proj_dir.path(), r#"model = "openai/b""#);

    let config = Config::load(&LoadOpts {
        user_config: Some(user_dir.path().join("config.toml")),
        project_config: Some(proj_dir.path().join("config.toml")),
        ..Default::default()
    })
    .unwrap();

    assert_eq!(config.model.as_deref(), Some("openai/b"));
}

#[test]
fn profile_overrides_base() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
model = "anthropic/claude-sonnet-4-6"
permission_mode = "default"

[profiles.work]
model = "openai/gpt-5.1"
permission_mode = "accept-edits"
"#,
    );

    let config = Config::load(&LoadOpts {
        user_config: Some(dir.path().join("config.toml")),
        profile: Some("work".into()),
        ..Default::default()
    })
    .unwrap();

    assert_eq!(config.model.as_deref(), Some("openai/gpt-5.1"));
    assert_eq!(
        config.permission_mode,
        tao_protocol::permission::PermissionMode::AcceptEdits
    );
}

#[test]
fn cli_override_beats_file() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), r#"model = "anthropic/from-file""#);

    let config = Config::load(&LoadOpts {
        user_config: Some(dir.path().join("config.toml")),
        overrides: vec![CliOverride {
            key: "model".into(),
            value: "openai/from-cli".into(),
        }],
        ..Default::default()
    })
    .unwrap();

    assert_eq!(config.model.as_deref(), Some("openai/from-cli"));
}

#[test]
fn cli_override_provider_field() {
    let config = Config::load(&LoadOpts {
        overrides: vec![CliOverride {
            key: "model_providers.myproxy.base_url".into(),
            value: "https://proxy.internal/v1".into(),
        }],
        ..Default::default()
    })
    .unwrap();

    assert_eq!(
        config.model_providers["myproxy"].base_url,
        "https://proxy.internal/v1"
    );
}

#[test]
fn resolve_anthropic_provider() {
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-test-key");
    }
    let config = Config::load(&LoadOpts {
        overrides: vec![CliOverride {
            key: "model".into(),
            value: "anthropic/claude-sonnet-4-6".into(),
        }],
        ..Default::default()
    })
    .unwrap();

    match resolve(&config) {
        Ok((_client, model)) => assert_eq!(model, "claude-sonnet-4-6"),
        Err(e) => panic!("resolve 失败: {e}"),
    }
    unsafe {
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
}

#[test]
fn resolve_custom_chat_provider() {
    unsafe {
        std::env::set_var("DEEPSEEK_API_KEY", "sk-deepseek");
    }
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
model = "deepseek/deepseek-chat"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com"
wire_api = "openai-chat"
env_key = "DEEPSEEK_API_KEY"
"#,
    );

    let config = Config::load(&LoadOpts {
        user_config: Some(dir.path().join("config.toml")),
        ..Default::default()
    })
    .unwrap();

    match resolve(&config) {
        Ok((_client, model)) => assert_eq!(model, "deepseek-chat"),
        Err(e) => panic!("resolve 失败: {e}"),
    }
    unsafe {
        std::env::remove_var("DEEPSEEK_API_KEY");
    }
}

#[test]
fn resolve_missing_api_key_errors() {
    unsafe {
        std::env::remove_var("NONEXISTENT_KEY_FOR_TEST");
    }
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
model = "custom/model-x"

[model_providers.custom]
name = "Custom"
base_url = "https://example.com"
wire_api = "openai-chat"
env_key = "NONEXISTENT_KEY_FOR_TEST"
"#,
    );

    let config = Config::load(&LoadOpts {
        user_config: Some(dir.path().join("config.toml")),
        ..Default::default()
    })
    .unwrap();

    match resolve(&config) {
        Ok(_) => panic!("应报 Auth 错误"),
        Err(tao_core::model::ModelError::Auth(_)) => {}
        Err(other) => panic!("应为 Auth 错误,got: {other}"),
    }
}

#[test]
fn resolve_unknown_provider_errors() {
    let config = Config::load(&LoadOpts {
        overrides: vec![CliOverride {
            key: "model".into(),
            value: "unknown-provider/some-model".into(),
        }],
        ..Default::default()
    })
    .unwrap();

    match resolve(&config) {
        Ok(_) => panic!("应报 Build 错误"),
        Err(tao_core::model::ModelError::Build(_)) => {}
        Err(other) => panic!("应为 Build 错误,got: {other}"),
    }
}
