//! 协议线格式稳定性测试。
//!
//! 这些 JSON 字面量是**对外契约**:任何修改(改字段名、改 tag、改 rename)
//! 都会让这些测试失败——这正是目的。要改协议,必须显式 bump PROTOCOL_VERSION
//! 并在这里留下新版本的字面量,而不是悄悄破坏既有前端。

use tao_protocol::op::{ReviewDecision, UserInput};
use tao_protocol::permission::{PermissionMode, RuleAction};
use tao_protocol::{
    ApprovalDetail, ApprovalKind, CallId, Content, Event, EventMsg, LogEvent, LogLine, Op,
    PermissionRule, SessionId, Submission, TokenUsage, TurnId,
};

fn roundtrip<T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
    json: &str,
    expected: &T,
) {
    let parsed: T = serde_json::from_str(json).expect("字面量必须能解析");
    assert_eq!(&parsed, expected, "解析结果与期望值不一致");
    let serialized = serde_json::to_string(expected).unwrap();
    let reparsed: T = serde_json::from_str(&serialized).unwrap();
    assert_eq!(&reparsed, expected, "roundtrip 失败");
}

#[test]
fn submission_user_turn_wire() {
    roundtrip::<Submission>(
        r#"{"id":"req-1","op":{"type":"user_turn","turn_id":"t-1","input":[{"type":"text","text":"hi"}]}}"#,
        &Submission {
            id: "req-1".into(),
            op: Op::UserTurn {
                turn_id: "t-1".into(),
                input: vec![UserInput::Text { text: "hi".into() }],
            },
        },
    );
}

#[test]
fn submission_approval_response_wire() {
    roundtrip::<Submission>(
        r#"{"id":"req-2","op":{"type":"approval_response","call_id":"c-1","decision":"approve_for_session"}}"#,
        &Submission {
            id: "req-2".into(),
            op: Op::ApprovalResponse {
                call_id: "c-1".into(),
                decision: ReviewDecision::ApproveForSession,
            },
        },
    );
}

#[test]
fn event_turn_complete_wire() {
    roundtrip::<Event>(
        r#"{"id":"req-1","seq":42,"turn":"t-1","msg":{"type":"turn_complete","turn_id":"t-1","usage":{"input":100,"cached_input":20,"output":30,"reasoning":5},"stop_reason":"end_turn"}}"#,
        &Event {
            id: "req-1".into(),
            seq: 42,
            turn: Some(TurnId::new("t-1")),
            msg: EventMsg::TurnComplete {
                turn_id: TurnId::new("t-1"),
                usage: TokenUsage {
                    input: 100,
                    cached_input: 20,
                    output: 30,
                    reasoning: 5,
                },
                stop_reason: tao_protocol::StopReason::EndTurn,
            },
        },
    );
}

#[test]
fn event_approval_request_wire() {
    roundtrip::<Event>(
        r#"{"id":"req-3","seq":7,"msg":{"type":"approval_request","call_id":"c-2","kind":"exec","detail":{"command":["cargo","test"],"pattern_suggestion":"Bash(cargo test *)"}}}"#,
        &Event {
            id: "req-3".into(),
            seq: 7,
            turn: None,
            msg: EventMsg::ApprovalRequest {
                call_id: CallId::new("c-2"),
                kind: ApprovalKind::Exec,
                detail: ApprovalDetail {
                    rule_matched: None,
                    command: Some(vec!["cargo".into(), "test".into()]),
                    files: None,
                    tool: None,
                    args_summary: None,
                    pattern_suggestion: Some("Bash(cargo test *)".into()),
                },
            },
        },
    );
}

#[test]
fn logline_session_meta_wire() {
    roundtrip::<LogLine>(
        r#"{"seq":0,"ts":1720000000000,"type":"session_meta","id":"s-1","cwd":"/repo","config_fingerprint":"abc","created_at_ms":1720000000000}"#,
        &LogLine {
            seq: 0,
            ts: 1_720_000_000_000,
            event: LogEvent::SessionMeta {
                id: SessionId::new("s-1"),
                parent: None,
                cwd: "/repo".into(),
                git_head: None,
                config_fingerprint: "abc".into(),
                created_at_ms: 1_720_000_000_000,
            },
        },
    );
}

#[test]
fn logline_assistant_message_wire() {
    roundtrip::<LogLine>(
        r#"{"seq":5,"ts":1720000001000,"type":"assistant_message","content":[{"type":"text","text":"done"}],"usage":{"input":1,"cached_input":0,"output":2,"reasoning":0},"turn_id":"t-1"}"#,
        &LogLine {
            seq: 5,
            ts: 1_720_000_001_000,
            event: LogEvent::AssistantMessage {
                content: vec![Content::text("done")],
                usage: TokenUsage {
                    input: 1,
                    cached_input: 0,
                    output: 2,
                    reasoning: 0,
                },
                turn_id: TurnId::new("t-1"),
            },
        },
    );
}

#[test]
fn permission_rule_wire() {
    roundtrip::<PermissionRule>(
        r#"{"tool":"Bash","pattern":"cargo *","action":"allow"}"#,
        &PermissionRule {
            tool: "Bash".into(),
            pattern: "cargo *".into(),
            action: RuleAction::Allow,
        },
    );
}

#[test]
fn set_permission_mode_op_wire() {
    roundtrip::<Op>(
        r#"{"type":"set_permission_mode","mode":"plan"}"#,
        &Op::SetPermissionMode {
            mode: PermissionMode::Plan,
        },
    );
}
