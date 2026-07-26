//! 新类型标识符,避免 String 混用。M1 起内部值将换成 uuid。

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

id_type!(SessionId, "会话 ID。");
id_type!(TurnId, "turn ID(由发起 UserTurn 的客户端生成)。");
id_type!(CallId, "单次工具调用 / 审批请求 ID。");
id_type!(CheckpointId, "文件快照 checkpoint ID(影子 git commit)。");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_serializes_transparently() {
        let id = SessionId::new("s-1");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"s-1\"");
        let back: SessionId = serde_json::from_str("\"s-1\"").unwrap();
        assert_eq!(back, id);
    }
}
