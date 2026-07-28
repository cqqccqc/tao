//! `tao` 二进制:薄壳调度。所有能力在库 crate,这里只做 clap 解析与分发
//! (见 docs/design/architecture.md 的 CLI 子命令表)。

use clap::{Parser, Subcommand};
use tao_core::config::{CliOverride, LoadOpts};

#[derive(Parser)]
#[command(
    name = "tao",
    version,
    about = "tao: a Rust coding agent (harness core + TUI, ACP-embeddable)",
    long_about = None
)]
struct Cli {
    /// 覆盖配置项(key=value,可多次),如 -c model=openai/gpt-5.1
    #[arg(short = 'c', long = "config", value_name = "KEY=VALUE", global = true)]
    overrides: Vec<String>,

    /// 使用配置档案(profiles.<name>)
    #[arg(long, global = true)]
    profile: Option<String>,

    /// 指定模型(provider/model-id)
    #[arg(long, global = true)]
    model: Option<String>,

    /// 跳过所有权限审批(等价 yolo,permission_mode=bypass)。需显式声明。
    #[arg(long, global = true)]
    dangerously_bypass_permissions: bool,

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
        /// Ask 审批的处理策略:deny(默认)或 approve。
        #[arg(long, value_name = "deny|approve", default_value = "deny")]
        on_ask: String,
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

fn parse_overrides(raw: &[String]) -> anyhow::Result<Vec<CliOverride>> {
    raw.iter()
        .map(|s| {
            if let Some((k, v)) = s.split_once('=') {
                Ok(CliOverride {
                    key: k.to_owned(),
                    value: v.to_owned(),
                })
            } else {
                anyhow::bail!("配置覆盖格式错误: {s}(应为 key=value)")
            }
        })
        .collect()
}

fn build_load_opts(cli: &Cli) -> anyhow::Result<LoadOpts> {
    let mut overrides = parse_overrides(&cli.overrides)?;
    if cli.dangerously_bypass_permissions {
        overrides.push(CliOverride {
            key: "permission_mode".into(),
            value: "bypass".into(),
        });
    }
    Ok(LoadOpts {
        profile: cli.profile.clone(),
        overrides,
        ..Default::default()
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let load_opts = build_load_opts(&cli)?;
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => tao_tui::run_with_load_opts(load_opts).await,
        Command::Exec {
            prompt,
            json,
            on_ask,
        } => {
            let on_ask = match on_ask.as_str() {
                "deny" => tao_exec::OnAsk::Deny,
                "approve" => tao_exec::OnAsk::Approve,
                other => anyhow::bail!("--on-ask 无效: {other}(可选: deny/approve)"),
            };
            let opts = tao_exec::ExecOpts {
                prompt,
                cwd: std::env::current_dir()?,
                model: cli.model.clone(),
                json,
                on_ask,
                load_opts,
            };
            tao_exec::run(opts).await
        }
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
