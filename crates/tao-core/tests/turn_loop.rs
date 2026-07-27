//! turn loop 测试:用 MockModel 脚本化 ModelStreamEvent,验证循环逻辑。
//! 见 docs/design/testing.md §2(MockModel 是 agent 测试的基石)。

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::json;
use tao_core::model::{ModelError, ModelRequest, ModelStreamEvent};
use tao_core::providers::ModelClient;
use tao_core::session::{TurnConfig, TurnEvent, run_turn};
use tao_core::tools::ToolRegistry;
use tao_protocol::content::StopReason;
use tao_protocol::ids::CallId;
use tokio_util::sync::CancellationToken;

/// 脚本化的 MockModel:按顺序返回预设的"轮次",每轮是一个事件序列。
struct MockModel {
    turns: Mutex<Vec<Vec<ModelStreamEvent>>>,
}

impl MockModel {
    fn new(turns: Vec<Vec<ModelStreamEvent>>) -> Self {
        Self {
            turns: Mutex::new(turns),
        }
    }
}

#[async_trait]
impl ModelClient for MockModel {
    async fn stream(
        &self,
        _req: &ModelRequest,
        _cancel: &CancellationToken,
    ) -> Result<BoxStream<'static, Result<ModelStreamEvent, ModelError>>, ModelError> {
        let mut turns = self.turns.lock().unwrap();
        if turns.is_empty() {
            return Err(ModelError::Fatal("MockModel: 无更多轮次".into()));
        }
        let events = turns.remove(0);
        drop(turns);
        let stream = futures::stream::iter(events.into_iter().map(Ok));
        Ok(Box::pin(stream))
    }
}

fn text_turn(text: &str, stop: StopReason) -> Vec<ModelStreamEvent> {
    vec![
        ModelStreamEvent::TextDelta(text.into()),
        ModelStreamEvent::MessageEnd {
            stop_reason: stop,
            usage: Default::default(),
        },
    ]
}

fn tool_turn(call_id: &str, name: &str, input: serde_json::Value) -> Vec<ModelStreamEvent> {
    let input_str = serde_json::to_string(&input).unwrap();
    vec![
        ModelStreamEvent::ToolUseBegin {
            call_id: CallId::new(call_id),
            name: name.into(),
        },
        ModelStreamEvent::ToolUseInputDelta {
            call_id: CallId::new(call_id),
            json_fragment: input_str,
        },
        ModelStreamEvent::ToolUseEnd {
            call_id: CallId::new(call_id),
        },
        ModelStreamEvent::MessageEnd {
            stop_reason: StopReason::ToolUse,
            usage: Default::default(),
        },
    ]
}

fn collect_events(events: &[TurnEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            TurnEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

async fn run_with(
    model: MockModel,
    messages: &mut Vec<tao_core::model::ModelMessage>,
    cancel: &CancellationToken,
) -> (
    Vec<TurnEvent>,
    Result<tao_core::session::TurnResult, ModelError>,
) {
    let tools = ToolRegistry::builtin();
    let req = ModelRequest {
        model: "mock".into(),
        system: vec![],
        messages: vec![],
        tools: vec![],
        reasoning: None,
        max_output_tokens: 1024,
        temperature: None,
        metadata: Default::default(),
    };
    let config = TurnConfig { max_steps: 10 };
    let collected: Arc<Mutex<Vec<TurnEvent>>> = Arc::new(Mutex::new(vec![]));
    let collected2 = collected.clone();
    let result = run_turn(
        &model,
        &tools,
        &req,
        messages,
        &config,
        Path::new("."),
        cancel,
        move |ev| collected2.lock().unwrap().push(ev),
    )
    .await;
    let events = collected.lock().unwrap().clone();
    (events, result)
}

#[tokio::test]
async fn text_only_turn() {
    let model = MockModel::new(vec![text_turn("hello", StopReason::EndTurn)]);
    let mut messages = vec![];
    let cancel = CancellationToken::new();
    let (events, result) = run_with(model, &mut messages, &cancel).await;
    let result = result.unwrap();
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.steps, 1);
    assert_eq!(collect_events(&events), vec!["hello"]);
    assert_eq!(messages.len(), 1);
}

#[tokio::test]
async fn tool_call_then_text() {
    let model = MockModel::new(vec![
        tool_turn("c1", "Bash", json!({"command": ["echo", "done"]})),
        text_turn("all good", StopReason::EndTurn),
    ]);
    let mut messages = vec![];
    let cancel = CancellationToken::new();
    let (events, result) = run_with(model, &mut messages, &cancel).await;
    let result = result.unwrap();
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.steps, 2);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::ToolCallBegin { tool, .. } if tool == "Bash"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::ToolExecEnd { ok: true, .. }))
    );
    assert_eq!(collect_events(&events), vec!["all good"]);
    assert_eq!(messages.len(), 3);
}

#[tokio::test]
async fn unknown_tool_produces_error_result() {
    let model = MockModel::new(vec![
        tool_turn("c1", "Nonexistent", json!({})),
        text_turn("recovered", StopReason::EndTurn),
    ]);
    let mut messages = vec![];
    let cancel = CancellationToken::new();
    let (events, result) = run_with(model, &mut messages, &cancel).await;
    let result = result.unwrap();
    assert_eq!(result.steps, 2);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::ToolExecEnd { ok: false, .. }))
    );
    if let tao_core::model::ModelMessage::ToolResult { is_error, .. } = &messages[1] {
        assert!(*is_error);
    } else {
        panic!("期望 ToolResult");
    }
}

#[tokio::test]
async fn max_steps_terminates() {
    let turns: Vec<Vec<ModelStreamEvent>> = (0..20)
        .map(|i| tool_turn(&format!("c{i}"), "Bash", json!({"command": ["echo", "x"]})))
        .collect();
    let model = MockModel::new(turns);
    let mut messages = vec![];
    let cancel = CancellationToken::new();
    let (events, result) = run_with(model, &mut messages, &cancel).await;
    let result = result.unwrap();
    assert_eq!(result.stop_reason, StopReason::MaxSteps);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TurnEvent::Error(msg) if msg.contains("max_steps")))
    );
}

#[tokio::test]
async fn cancellation_during_turn() {
    let model = MockModel::new(vec![tool_turn(
        "c1",
        "Bash",
        json!({"command": ["sleep", "30"]}),
    )]);
    let mut messages = vec![];
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (_events, result) = run_with(model, &mut messages, &cancel).await;
    let result = result.unwrap();
    assert_eq!(result.stop_reason, StopReason::Interrupted);
}
