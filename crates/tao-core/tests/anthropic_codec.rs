//! Anthropic codec 集成测试:用 wiremock 回放 SSE fixture,验证规范事件序列。
//! 见 docs/design/testing.md §3。

use std::time::Duration;

use futures::StreamExt;
use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::providers::anthropic::AnthropicClient;
use tao_core::providers::common::HttpSseClient;
use tao_core::{ModelClient, ModelStreamEvent};
use tao_protocol::content::StopReason;
use tao_protocol::ids::CallId;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body(events: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (event, data) in events {
        if !event.is_empty() {
            out.push_str("event: ");
            out.push_str(event);
            out.push('\n');
        }
        out.push_str("data: ");
        out.push_str(data);
        out.push_str("\n\n");
    }
    out
}

fn req() -> ModelRequest {
    ModelRequest {
        model: "claude-sonnet-4-6".into(),
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
    client: &AnthropicClient,
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
async fn text_streaming_produces_text_delta_and_end() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"output_tokens":0}}}"#,
        ),
        (
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
        ),
        ("content_block_stop", r#"{"index":0}"#),
        (
            "message_delta",
            r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpSseClient::for_test(0, Duration::from_secs(5));
    let client = AnthropicClient::with_api_key(server.uri(), "test-key").with_http(http);
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
async fn tool_use_partial_json_accumulated_into_deltas() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":5,"output_tokens":0}}}"#,
        ),
        (
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"Bash"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"[\"ls\"]}"}}"#,
        ),
        ("content_block_stop", r#"{"index":0}"#),
        (
            "message_delta",
            r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpSseClient::for_test(0, Duration::from_secs(5));
    let client = AnthropicClient::with_api_key(server.uri(), "test-key").with_http(http);
    let cancel = CancellationToken::new();

    let events = collect(&client, &req(), &cancel).await;
    let begin = events.iter().find_map(|e| match e {
        ModelStreamEvent::ToolUseBegin { call_id, name } => Some((call_id.clone(), name.clone())),
        _ => None,
    });
    assert_eq!(begin, Some((CallId::new("toolu_01"), "Bash".to_owned())));

    let frags: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ModelStreamEvent::ToolUseInputDelta { json_fragment, .. } => {
                Some(json_fragment.clone())
            }
            _ => None,
        })
        .collect();
    let combined: String = frags.concat();
    assert_eq!(combined, r#"{"command":["ls"]}"#);

    let end_stop = events.iter().find_map(|e| match e {
        ModelStreamEvent::MessageEnd { stop_reason, .. } => Some(*stop_reason),
        _ => None,
    });
    assert_eq!(end_stop, Some(StopReason::ToolUse));
}

#[tokio::test]
async fn retry_on_429_then_succeed() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":5,"output_tokens":0}}}"#,
        ),
        (
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
        ),
        ("content_block_stop", r#"{"index":0}"#),
        (
            "message_delta",
            r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);

    // 首次 429,第二次 200。
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_string(
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let http = HttpSseClient::for_test(3, Duration::from_secs(5));
    let client = AnthropicClient::with_api_key(server.uri(), "test-key").with_http(http);
    let cancel = CancellationToken::new();

    let events = collect(&client, &req(), &cancel).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ModelStreamEvent::TextDelta(t) if t == "ok"))
    );
}

#[tokio::test]
async fn auth_error_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"type":"error","error":{"type":"authentication_error","message":"invalid api key"}}"#))
        .mount(&server)
        .await;

    let http = HttpSseClient::for_test(5, Duration::from_secs(5));
    let client = AnthropicClient::with_api_key(server.uri(), "bad").with_http(http);
    let cancel = CancellationToken::new();

    match client.stream(&req(), &cancel).await {
        Ok(_) => panic!("应上抛 Auth 错误"),
        Err(tao_core::model::ModelError::Auth(_)) => {}
        Err(other) => panic!("应为 Auth 错误,got: {other:?}"),
    }
}

#[tokio::test]
async fn cancellation_aborts_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(60))
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let http = HttpSseClient::default();
    let client = AnthropicClient::with_api_key(server.uri(), "k").with_http(http);
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();

    // 50ms 后取消,请求应被中止。
    let handle = tokio::spawn(async move { client.stream(&req(), &cancel2).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let res = handle.await.unwrap();
    assert!(res.is_err(), "取消应上抛错误");
}
