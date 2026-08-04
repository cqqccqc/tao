//! 上下文压缩:token 估算超阈值时,用 model 生成结构化摘要(见 sessions.md §4)。
//!
//! v1:token 近似(字符数/4);window 硬编码 200k;自动触发;keep_last 4;
//! covers_through_seq 用消息数近似(TODO 对齐日志 seq)。

use std::sync::OnceLock;

use anyhow::{Context, Result};
use futures::StreamExt;
use tao_protocol::content::Content;
use tao_protocol::log::LogEvent;
use tiktoken_rs::{CoreBPE, cl100k_base};
use tokio_util::sync::CancellationToken;

use crate::model::{
    ModelContent, ModelMessage, ModelRequest, ModelStreamEvent, RequestMeta, SystemBlock,
};
use crate::providers::ModelClient;
use crate::recorder::Recorder;

/// tiktoken cl100k_base tokenizer 单例(初始化失败返回 None,fallback chars/4)。
static TOKENIZER: OnceLock<Option<CoreBPE>> = OnceLock::new();

fn get_tokenizer() -> Option<&'static CoreBPE> {
    TOKENIZER.get_or_init(|| cl100k_base().ok()).as_ref()
}

/// 默认上下文窗口(v1 硬编码;TODO:registry 暴露 context_window)。
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
/// 压缩时保留最近 N 条消息(2 轮)。
pub const DEFAULT_KEEP_LAST: usize = 4;

/// 近似 token 估算:优先用 tiktoken cl100k_base 编码求和;初始化失败 fallback
/// chars/4(英文近似;中文偏高,安全侧早压缩)。
pub fn approx_tokens(messages: &[ModelMessage]) -> u64 {
    if let Some(tokenizer) = get_tokenizer() {
        let total: usize = messages.iter().map(|m| message_tokens(m, tokenizer)).sum();
        total as u64
    } else {
        let chars: usize = messages.iter().map(message_chars).sum();
        (chars as u64) / 4
    }
}

/// tiktoken 路径:各 message 的 Text content encode 求和。
fn message_tokens(m: &ModelMessage, tokenizer: &CoreBPE) -> usize {
    match m {
        ModelMessage::User { content } | ModelMessage::Assistant { content } => {
            content.iter().map(|c| content_tokens(c, tokenizer)).sum()
        }
        ModelMessage::ToolResult { content, .. } => {
            content.iter().map(|c| content_tokens(c, tokenizer)).sum()
        }
    }
}

fn content_tokens(c: &ModelContent, tokenizer: &CoreBPE) -> usize {
    match c {
        ModelContent::Text(t) => tokenizer.encode_ordinary(t).len(),
        _ => 0,
    }
}

// ---- fallback: chars / 4 ----

fn message_chars(m: &ModelMessage) -> usize {
    match m {
        ModelMessage::User { content } | ModelMessage::Assistant { content } => {
            content.iter().map(content_chars).sum()
        }
        ModelMessage::ToolResult { content, .. } => content.iter().map(content_chars).sum(),
    }
}

fn content_chars(c: &ModelContent) -> usize {
    match c {
        ModelContent::Text(t) => t.len(),
        ModelContent::Thinking { text, .. } => text.len(),
        ModelContent::ToolUse { input, .. } => input.to_string().len(),
        ModelContent::Image { data_base64, .. } => data_base64.len(),
    }
}

/// 压缩:摘要 `messages[..len-keep]` → summary,返回 `[Assistant(summary)] + messages[len-keep..]`。
/// 记 `Compaction` 事件(`covers_through_seq` = 被摘要的消息数,近似)。
pub async fn compact(
    client: &dyn ModelClient,
    model: &str,
    messages: &[ModelMessage],
    keep_last: usize,
    recorder: &dyn Recorder,
) -> Result<Vec<ModelMessage>> {
    let keep = keep_last.min(messages.len());
    if messages.len() <= keep {
        return Ok(messages.to_vec());
    }
    let to_summarize = &messages[..messages.len() - keep];
    let summary = summarize(client, model, to_summarize).await?;
    // 对齐日志 seq:被摘要的消息对应到当前已记录的最大 seq
    let covers = recorder.current_seq();
    recorder.record(LogEvent::Compaction {
        summary: vec![Content::text(&summary)],
        covers_through_seq: covers,
    });
    let mut new_messages = vec![ModelMessage::Assistant {
        content: vec![ModelContent::Text(summary)],
    }];
    new_messages.extend(messages[messages.len() - keep..].iter().cloned());
    Ok(new_messages)
}

async fn summarize(
    client: &dyn ModelClient,
    model: &str,
    messages: &[ModelMessage],
) -> Result<String> {
    let req = ModelRequest {
        model: model.to_string(),
        system: vec![SystemBlock {
            text: "你是 tao 的上下文压缩器。把以下对话压缩为结构化摘要:\
                   1) 目标 2) 关键决策 3) 已做改动 4) 待办。保持简洁,保留关键事实。"
                .into(),
            cache_breakpoint: None,
        }],
        messages: vec![ModelMessage::User {
            content: vec![ModelContent::text(messages_to_text(messages))],
        }],
        tools: vec![],
        reasoning: None,
        max_output_tokens: 1024,
        temperature: None,
        metadata: RequestMeta::default(),
    };
    let cancel = CancellationToken::new();
    let mut stream = client.stream(&req, &cancel).await.context("压缩请求失败")?;
    let mut summary = String::new();
    while let Some(ev) = stream.next().await {
        match ev? {
            ModelStreamEvent::TextDelta(t) => summary.push_str(&t),
            ModelStreamEvent::MessageEnd { .. } => break,
            _ => {}
        }
    }
    Ok(summary)
}

fn messages_to_text(messages: &[ModelMessage]) -> String {
    let mut s = String::new();
    for m in messages {
        match m {
            ModelMessage::User { content } => {
                s.push_str("用户: ");
                s.push_str(&content_text(content));
            }
            ModelMessage::Assistant { content } => {
                s.push_str("助手: ");
                s.push_str(&content_text(content));
            }
            ModelMessage::ToolResult { content, .. } => {
                s.push_str("工具结果: ");
                s.push_str(&content_text(content));
            }
        }
        s.push('\n');
    }
    s
}

fn content_text(content: &[ModelContent]) -> String {
    let mut s = String::new();
    for c in content {
        match c {
            ModelContent::Text(t) => s.push_str(t),
            ModelContent::Thinking { text, .. } => {
                s.push_str("[思考] ");
                s.push_str(text);
            }
            ModelContent::ToolUse { name, input, .. } => {
                s.push_str(&format!("[调用 {name} {input}]"));
            }
            ModelContent::Image { .. } => s.push_str("[图片]"),
        }
        s.push(' ');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::JsonlRecorder;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use std::path::Path;
    use tao_protocol::content::StopReason;
    use tempfile::TempDir;

    struct MockModel {
        text: String,
    }

    #[async_trait]
    impl ModelClient for MockModel {
        async fn stream(
            &self,
            _req: &ModelRequest,
            _cancel: &CancellationToken,
        ) -> Result<
            BoxStream<'static, Result<ModelStreamEvent, crate::model::ModelError>>,
            crate::model::ModelError,
        > {
            let text = self.text.clone();
            let events = vec![
                ModelStreamEvent::TextDelta(text),
                ModelStreamEvent::MessageEnd {
                    stop_reason: StopReason::EndTurn,
                    usage: Default::default(),
                },
            ];
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    #[test]
    fn approx_tokens_counts_chars() {
        let msgs = vec![ModelMessage::User {
            content: vec![ModelContent::text("hello world")],
        }]; // 11 chars
        assert_eq!(approx_tokens(&msgs), 2); // 11/4 = 2
    }

    #[tokio::test]
    async fn compact_summarizes_and_keeps_last() {
        let dir = TempDir::new().unwrap();
        let (recorder, _id) =
            JsonlRecorder::create_with_base(Path::new("/tmp/test"), dir.path(), String::new())
                .unwrap();
        let model = MockModel {
            text: "摘要内容".into(),
        };
        let messages = vec![
            ModelMessage::User {
                content: vec![ModelContent::text("msg1")],
            },
            ModelMessage::Assistant {
                content: vec![ModelContent::text("a1")],
            },
            ModelMessage::User {
                content: vec![ModelContent::text("msg2")],
            },
            ModelMessage::Assistant {
                content: vec![ModelContent::text("a2")],
            },
            ModelMessage::User {
                content: vec![ModelContent::text("msg3")],
            }, // keep
            ModelMessage::Assistant {
                content: vec![ModelContent::text("a3")],
            }, // keep
        ];
        let new = compact(&model, "mock", &messages, 2, &recorder)
            .await
            .unwrap();
        assert_eq!(new.len(), 3); // [Assistant(summary)] + last 2
        if let ModelMessage::Assistant { content } = &new[0] {
            assert!(
                content
                    .iter()
                    .any(|c| matches!(c, ModelContent::Text(t) if t == "摘要内容"))
            );
        } else {
            panic!("期望 Assistant(summary)");
        }
        // 保留的最后一条
        if let ModelMessage::Assistant { content } = &new[2] {
            assert!(
                content
                    .iter()
                    .any(|c| matches!(c, ModelContent::Text(t) if t == "a3"))
            );
        }
        // Compaction 事件落盘
        let log = std::fs::read_to_string(recorder.path()).unwrap();
        assert!(log.contains("compaction"));
    }

    #[tokio::test]
    async fn compact_noop_when_too_few() {
        let dir = TempDir::new().unwrap();
        let (recorder, _id) =
            JsonlRecorder::create_with_base(Path::new("/tmp/test"), dir.path(), String::new())
                .unwrap();
        let model = MockModel { text: "x".into() };
        let messages = vec![ModelMessage::User {
            content: vec![ModelContent::text("only")],
        }];
        let new = compact(&model, "mock", &messages, 4, &recorder)
            .await
            .unwrap();
        assert_eq!(new.len(), 1); // 不压缩
    }
}
