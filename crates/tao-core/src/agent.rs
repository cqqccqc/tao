//! 会话 actor 的占位骨架。真实 turn loop 在 M1 实现。

use std::path::PathBuf;

use tao_protocol::SessionId;

/// 会话配置(M1 起大幅扩展:模型、权限模式、指令文件等)。
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub model: String,
}

/// 会话 actor(M1 实现)。
pub struct Agent;

/// 克隆即新句柄,共享同一会话 actor(M1 实现 submit/next_event)。
#[derive(Debug, Clone)]
pub struct AgentHandle {
    _private: (),
}

impl AgentHandle {
    pub async fn spawn(_config: SessionConfig) -> anyhow::Result<(Self, SessionId)> {
        Err(anyhow::anyhow!(
            "tao-core 尚未实现:Agent::spawn 将在 M1(模型接入)落地,见 docs/design/roadmap.md"
        ))
    }
}
