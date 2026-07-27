//! OpenAI Responses API codec 测试:wiremock 回放 SSE。
//! 见 docs/design/providers.md §3.2、testing.md §3。

use std::time::Duration;

use futures::StreamExt;
use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::providers::common::HttpSseClient;
use tao_core::providers::openai_responses::OpenAiResponsesClient;
use tao_core::{ModelClient, ModelStreamEvent};
use tao_protocol::content::StopReason;
use tao_protocol::ids::CallId;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body(events: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (event, data) in events {
        out.push_str("event: ");
        out.push_str(event);
        out.push('\n');
        out.push_str("data: ");
        out.push_str(data);
        out.push_str("\n\n");
    }
    out
}

fn req() -> ModelRequest {
    ModelRequest {
        model: "gpt-5.1".into(),
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
    client: &OpenAiResponsesClient,
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
async fn text_streaming_and_completion() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "response.created",
            r#"{"response":{"id":"resp_1","status":"in_progress"}}"#,
        ),
        (
            "response.output_item.added",
            r#"{"output_index":0,"item":{"type":"message","role":"assistant","id":"msg_1"}}"#,
        ),
        (
            "response.content_part.added",
            r#"{"output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}"#,
        ),
        (
            "response.output_text.delta",
            r#"{"output_index":0,"content_index":0,"delta":"Hel"}"#,
        ),
        (
            "response.output_text.delta",
            r#"{"output_index":0,"content_index":0,"delta":"lo"}"#,
        ),
        (
            "response.output_text.done",
            r#"{"output_index":0,"content_index":0,"text":"Hello"}"#,
        ),
        (
            "response.content_part.done",
            r#"{"output_index":0,"content_index":0,"part":{"type":"output_text","text":"Hello"}}"#,
        ),
        (
            "response.output_item.done",
            r#"{"output_index":0,"item":{"type":"message","role":"assistant","id":"msg_1"}}"#,
        ),
        (
            "response.completed",
            r#"{"response":{"id":"resp_1","status":"completed","usage":{"input_tokens":5,"output_tokens":2,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}}}"#,
        ),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = OpenAiResponsesClient::new(server.uri(), "test-key")
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
async fn function_call_accumulation() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "response.created",
            r#"{"response":{"id":"resp_1","status":"in_progress"}}"#,
        ),
        (
            "response.output_item.added",
            r#"{"output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"Bash","arguments":""}}"#,
        ),
        (
            "response.function_call_arguments.delta",
            r#"{"output_index":0,"delta":"{\"comm"}"#,
        ),
        (
            "response.function_call_arguments.delta",
            r#"{"output_index":0,"delta":"and\":[\"ls\"]}"}"#,
        ),
        (
            "response.output_item.done",
            r#"{"output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"Bash","arguments":"{\"command\":[\"ls\"]}"}}"#,
        ),
        (
            "response.completed",
            r#"{"response":{"id":"resp_1","status":"completed","usage":{"input_tokens":5,"output_tokens":3,"output_tokens_details":{"reasoning_tokens":0}}}}"#,
        ),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = OpenAiResponsesClient::new(server.uri(), "test-key")
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

    assert!(events.iter().any(|e| matches!(e, ModelStreamEvent::ToolUseEnd { call_id } if call_id == &CallId::new("call_1"))));
}

#[tokio::test]
async fn reasoning_summary_mapped_to_thinking_delta() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "response.created",
            r#"{"response":{"id":"resp_1","status":"in_progress"}}"#,
        ),
        (
            "response.reasoning_summary_text.delta",
            r#"{"output_index":0,"summary_index":0,"delta":"思考"}"#,
        ),
        (
            "response.output_item.added",
            r#"{"output_index":1,"item":{"type":"message","role":"assistant","id":"msg_1"}}"#,
        ),
        (
            "response.output_text.delta",
            r#"{"output_index":1,"content_index":0,"delta":"答案"}"#,
        ),
        (
            "response.output_item.done",
            r#"{"output_index":1,"item":{"type":"message","role":"assistant","id":"msg_1"}}"#,
        ),
        (
            "response.completed",
            r#"{"response":{"id":"resp_1","status":"completed","usage":{"input_tokens":1,"output_tokens":1,"output_tokens_details":{"reasoning_tokens":2}}}}"#,
        ),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = OpenAiResponsesClient::new(server.uri(), "test-key")
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

    // 验证 reasoning_tokens 被解析
    let usage = events.iter().find_map(|e| match e {
        ModelStreamEvent::MessageEnd { usage, .. } => Some(*usage),
        _ => None,
    });
    assert_eq!(usage.map(|u| u.reasoning), Some(2));
}

#[tokio::test]
async fn store_false_in_request_body() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "response.created",
            r#"{"response":{"id":"resp_1","status":"in_progress"}}"#,
        ),
        (
            "response.completed",
            r#"{"response":{"id":"resp_1","status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(wiremock::matchers::body_string_contains("\"store\":false"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let client = OpenAiResponsesClient::new(server.uri(), "test-key")
        .with_http(HttpSseClient::for_test(0, Duration::from_secs(5)));
    let cancel = CancellationToken::new();
    let events = collect(&client, &req(), &cancel).await;
    assert!(!events.is_empty());
}
