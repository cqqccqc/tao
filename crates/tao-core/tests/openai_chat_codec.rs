//! OpenAI Chat Completions codec 测试:wiremock 回放 SSE。
//! 见 docs/design/providers.md §3.3、testing.md §3。

use std::time::Duration;

use futures::StreamExt;
use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::providers::common::HttpSseClient;
use tao_core::providers::openai_chat::OpenAiChatClient;
use tao_core::{ModelClient, ModelStreamEvent};
use tao_protocol::content::StopReason;
use tao_protocol::ids::CallId;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body(chunks: &[&str]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str("data: ");
        out.push_str(c);
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn req() -> ModelRequest {
    ModelRequest {
        model: "gpt-4o".into(),
        system: vec![SystemBlock {
            text: "你是助手".into(),
            cache_breakpoint: None,
        }],
        messages: vec![ModelMessage::User {
            content: vec![ModelContent::text("hi")],
        }],
        tools: vec![],
        reasoning: None,
        max_output_tokens: 1024,
        temperature: None,
        metadata: RequestMeta::default(),
    }
}

async fn collect(
    client: &OpenAiChatClient,
    req: &ModelRequest,
    cancel: &CancellationToken,
) -> Vec<ModelStreamEvent> {
    let mut s = client.stream(req, cancel).await.expect("stream ok");
    let mut out = vec![];
    while let Some(ev) = s.next().await {
        match ev {
            Ok(e) => out.push(e),
            Err(e) => panic!("stream error: {e}"),
        }
    }
    out
}

#[tokio::test]
async fn text_streaming_and_usage() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"Hel"},"index":0}]}"#,
        r#"{"choices":[{"delta":{"content":"lo"},"index":0}]}"#,
        r#"{"choices":[{"delta":{},"index":0,"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":0}}}"#,
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = OpenAiChatClient::new(server.uri(), "test-key")
        .with_http(HttpSseClient::for_test(0, Duration::from_secs(5)));
    let cancel = CancellationToken::new();

    let events = collect(&client, &req(), &cancel).await;
    let texts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ModelStreamEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["Hel", "lo"]);

    let end = events.iter().find_map(|e| match e {
        ModelStreamEvent::MessageEnd { stop_reason, .. } => Some(*stop_reason),
        _ => None,
    });
    assert_eq!(end, Some(StopReason::EndTurn));
}

#[tokio::test]
async fn tool_calls_index_accumulation() {
    let server = MockServer::start().await;
    // OpenAI chat 协议:首块带 id+name,后续只带 arguments 片段,按 index 路由。
    let body = sse_body(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"Bash","arguments":""}}]},"index":0}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"comm"}}]},"index":0}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":[\"ls\"]}"}}]},"index":0,"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":3}}"#,
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = OpenAiChatClient::new(server.uri(), "test-key")
        .with_http(HttpSseClient::for_test(0, Duration::from_secs(5)));
    let cancel = CancellationToken::new();

    let events = collect(&client, &req(), &cancel).await;
    let begin = events.iter().find_map(|e| match e {
        ModelStreamEvent::ToolUseBegin { call_id, name } => Some((call_id.clone(), name.clone())),
        _ => None,
    });
    assert_eq!(begin, Some((CallId::new("call_1"), "Bash".to_owned())));

    let frags: String = events
        .iter()
        .filter_map(|e| match e {
            ModelStreamEvent::ToolUseInputDelta { json_fragment, .. } => {
                Some(json_fragment.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(frags, r#"{"command":["ls"]}"#);

    // usage 到达时补发 ToolUseEnd(chat 协议无显式 end)
    assert!(events.iter().any(|e| matches!(e, ModelStreamEvent::ToolUseEnd { call_id } if call_id == &CallId::new("call_1"))));

    let end_stop = events.iter().find_map(|e| match e {
        ModelStreamEvent::MessageEnd { stop_reason, .. } => Some(*stop_reason),
        _ => None,
    });
    assert_eq!(end_stop, Some(StopReason::ToolUse));
}

#[tokio::test]
async fn reasoning_content_mapped_to_thinking_delta() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"choices":[{"delta":{"reasoning_content":"思考"},"index":0}]}"#,
        r#"{"choices":[{"delta":{"content":"答案"},"index":0,"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = OpenAiChatClient::new(server.uri(), "test-key")
        .with_http(HttpSseClient::for_test(0, Duration::from_secs(5)));
    let cancel = CancellationToken::new();

    let events = collect(&client, &req(), &cancel).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelStreamEvent::ThinkingDelta(t) if t == "思考"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelStreamEvent::TextDelta(t) if t == "答案"))
    );
}

#[tokio::test]
async fn retry_on_500_then_succeed() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"choices":[{"delta":{"content":"ok"},"index":0,"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = OpenAiChatClient::new(server.uri(), "test-key")
        .with_http(HttpSseClient::for_test(3, Duration::from_secs(5)));
    let cancel = CancellationToken::new();
    let events = collect(&client, &req(), &cancel).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelStreamEvent::TextDelta(t) if t == "ok"))
    );
}
