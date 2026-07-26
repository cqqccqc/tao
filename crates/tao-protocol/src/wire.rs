//! JSONL 线格式编解码(见 docs/design/protocol.md §5)。
//! 纯函数实现,无 IO 依赖:tokio adapter 归 tao-server,同步 reader 归 tao-cli。

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::ProtocolError;
use crate::op::Op;
use crate::{Event, Submission};

/// 协议版本。新增变体 = 次版本;删除/改义 = 主版本。
pub const PROTOCOL_VERSION: u32 = 1;

/// 把一个可序列化消息编码为一行 JSON(不含换行)。
pub fn encode_line<T: Serialize>(msg: &T) -> Result<String, ProtocolError> {
    serde_json::to_string(msg).map_err(|e| ProtocolError::Codec(e.to_string()))
}

/// 解析一行 JSON(容忍首尾空白与末尾换行)。
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T, ProtocolError> {
    let trimmed = line.trim();
    serde_json::from_str(trimmed).map_err(|e| ProtocolError::Codec(e.to_string()))
}

/// Submission 编解码便捷函数。
pub fn encode_submission(sub: &Submission) -> Result<String, ProtocolError> {
    encode_line(sub)
}
pub fn decode_submission(line: &str) -> Result<Submission, ProtocolError> {
    decode_line(line)
}

/// Event 编解码便捷函数。
pub fn encode_event(ev: &Event) -> Result<String, ProtocolError> {
    encode_line(ev)
}
pub fn decode_event(line: &str) -> Result<Event, ProtocolError> {
    decode_line(line)
}

/// wire 首条消息构造:握手。
pub fn hello() -> Submission {
    Submission {
        id: "hello".into(),
        op: Op::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventMsg;

    #[test]
    fn submission_wire_roundtrip() {
        let sub = Submission {
            id: "r-1".into(),
            op: Op::UserTurn {
                turn_id: "t-1".into(),
                input: vec![crate::op::UserInput::Text {
                    text: "你好".into(),
                }],
            },
        };
        let line = encode_submission(&sub).unwrap();
        assert!(!line.contains('\n'));
        let back = decode_submission(&line).unwrap();
        assert_eq!(back, sub);
    }

    #[test]
    fn event_wire_roundtrip_with_trailing_newline() {
        let ev = Event::new(
            "r-1",
            EventMsg::StreamError {
                message: "boom".into(),
            },
        );
        let line = encode_event(&ev).unwrap() + "\n";
        let back = decode_event(&line).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn hello_uses_current_version() {
        let h = hello();
        match h.op {
            Op::Hello { protocol_version } => assert_eq!(protocol_version, PROTOCOL_VERSION),
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_submission("{not json").is_err());
        assert!(decode_event("{\"id\":\"x\"}").is_err());
    }
}
