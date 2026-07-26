//! `tao` 二进制:薄壳调度。所有能力在库 crate,这里只做 clap 解析与分发
//! (见 docs/design/architecture.md 的 CLI 子命令表)。

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "tao",
    version,
    about = "tao: a Rust coding agent (harness core + TUI, ACP-embeddable)",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 启动终端界面(默认)。
    Tui,
    /// headless 单次执行。
    Exec {
        /// 任务提示词。
        prompt: String,
        /// 以 JSONL 输出事件流。
        #[arg(long)]
        json: bool,
    },
    /// 协议模式:stdin/stdout 走 JSONL Op/Event。
    Proto,
    /// 常驻服务:TCP/WS 多客户端(M4)。
    Serve {
        #[arg(long, default_value_t = 7777)]
        port: u16,
    },
    /// ACP 模式:被 Zed 等编辑器以 stdio 拉起(M4)。
    Acp,
    /// 把 tao 暴露为 MCP server(M4)。
    McpServe,
    /// 登录 provider(OAuth 或 API key)。
    Login {
        /// provider id(anthropic / openai / ...)。
        provider: Option<String>,
        /// 以 API key 方式登录(存 OS keychain)。
        #[arg(long)]
        api_key: bool,
    },
    /// 登出 provider。
    Logout { provider: String },
    /// 查看各 provider 凭证状态。
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// 会话管理。
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// 列出每个 provider 的凭证来源与有效期。
    Status,
}

#[derive(Subcommand)]
enum SessionsAction {
    /// 列出会话(fork 树)。
    Ls,
    /// 输出会话的权限审计轨迹。
    Audit { session_id: String },
    /// 按保留策略清理旧会话。
    Gc,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => tao_tui::run().await,
        Command::Exec { prompt, json } => tao_exec::run(&prompt, json).await,
        Command::Proto => tao_server::run_proto().await,
        Command::Serve { port } => tao_server::run_serve(port).await,
        Command::Acp => tao_acp::run().await,
        Command::McpServe => tao_mcp::run_server().await,
        Command::Login { .. } | Command::Logout { .. } | Command::Auth { .. } => Err(
            anyhow::anyhow!("auth 子命令将在 M3 实现,见 docs/design/config.md §3"),
        ),
        Command::Sessions { .. } => Err(anyhow::anyhow!(
            "sessions 子命令将在 M2/M4 实现,见 docs/design/sessions.md"
        )),
    }
}
