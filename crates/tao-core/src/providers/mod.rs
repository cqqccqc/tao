//! provider 抽象与 codec 注册。见 docs/design/providers.md。

pub mod anthropic;
pub mod common;
pub mod openai_chat;
pub mod openai_responses;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::model::{ModelError, ModelRequest, ModelStreamEvent};

/// 模型客户端:agent loop 唯一依赖的 provider 接口。
#[async_trait]
pub trait ModelClient: Send + Sync {
    /// 发起流式请求,返回规范增量事件流。
    /// 重试/超时/取消由实现负责(见 common::HttpSseClient)。
    async fn stream(
        &self,
        req: &ModelRequest,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<BoxStream<'static, Result<ModelStreamEvent, ModelError>>, ModelError>;
}
