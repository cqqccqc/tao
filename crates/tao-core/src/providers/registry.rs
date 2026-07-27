//! Provider 注册表:从 Config 构造 ModelClient。
//! 见 docs/design/config.md §2 / providers.md §6。

use std::sync::Arc;

use crate::config::{Config, ModelProviderConfig, WireApi};
use crate::model::ModelError;
use crate::providers::ModelClient;
use crate::providers::anthropic::AnthropicClient;
use crate::providers::openai_chat::OpenAiChatClient;
use crate::providers::openai_responses::OpenAiResponsesClient;

/// 从配置解析出 (client, model_id)。
///
/// provider 选择:Config.model_provider > model 前缀 > 报错。
/// API key:从 provider.env_key 指向的环境变量读取。
pub fn resolve(config: &Config) -> Result<(Arc<dyn ModelClient>, String), ModelError> {
    let provider_id = config.current_provider_id().ok_or_else(|| {
        ModelError::Build("未指定 model 或 model_provider(例:anthropic/claude-sonnet-4-6)".into())
    })?;

    let provider = config.model_providers.get(&provider_id).ok_or_else(|| {
        ModelError::Build(format!(
            "未知 provider: {provider_id}(在 model_providers 里定义它)"
        ))
    })?;

    let api_key = std::env::var(&provider.env_key).map_err(|_| {
        ModelError::Auth(format!(
            "环境变量 {} 未设置(provider: {provider_id})",
            provider.env_key
        ))
    })?;
    if api_key.is_empty() {
        return Err(ModelError::Auth(format!(
            "环境变量 {} 为空(provider: {provider_id})",
            provider.env_key
        )));
    }

    let model_id = config
        .current_model_id()
        .ok_or_else(|| ModelError::Build("未指定 model(例:anthropic/claude-sonnet-4-6)".into()))?;

    let client: Arc<dyn ModelClient> = match provider.wire_api {
        WireApi::Anthropic => Arc::new(AnthropicClient::with_api_key(&provider.base_url, &api_key)),
        WireApi::OpenaiResponses => {
            Arc::new(OpenAiResponsesClient::new(&provider.base_url, &api_key))
        }
        WireApi::OpenaiChat => Arc::new(OpenAiChatClient::new(&provider.base_url, &api_key)),
    };

    Ok((client, model_id))
}

/// 仅用于错误信息:列出所有已配置的 provider id。
pub fn list_provider_ids(config: &Config) -> Vec<String> {
    config.model_providers.keys().cloned().collect()
}

/// 从 provider 配置预检 API key 是否就绪(不构造 client)。
pub fn check_credential(provider: &ModelProviderConfig) -> Result<(), String> {
    match std::env::var(&provider.env_key) {
        Ok(v) if !v.is_empty() => Ok(()),
        Ok(_) => Err(format!("环境变量 {} 为空", provider.env_key)),
        Err(_) => Err(format!("环境变量 {} 未设置", provider.env_key)),
    }
}
