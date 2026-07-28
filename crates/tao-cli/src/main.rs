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

    /// resume 指定 session id(exec 支持;tui 留后续)。
    #[arg(long, global = true)]
    resume: Option<String>,

    /// 配合 --resume 分叉新会话(继承历史,parent 指向原 id)。
    #[arg(long, global = true)]
    fork: bool,

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
    /// 列出会话(fork 树 + title)。
    Ls,
    /// 预览会话内容(meta + 消息摘要)。
    Show { session_id: String },
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
                resume: cli.resume.clone(),
                fork: cli.fork,
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
        Command::Sessions { action } => {
            let cwd = std::env::current_dir()?;
            match action {
                SessionsAction::Ls => run_sessions_ls(&cwd),
                SessionsAction::Show { session_id } => run_sessions_show(&cwd, &session_id),
                SessionsAction::Audit { session_id } => run_sessions_audit(&cwd, &session_id),
                SessionsAction::Gc => {
                    let config = tao_core::config::Config::load(&load_opts)?;
                    run_sessions_gc(&cwd, config.sessions.keep_days)
                }
            }
        }
    }
}

// ---- sessions 子命令实现(v1:扫描 JSONL)----

fn run_sessions_ls(cwd: &std::path::Path) -> anyhow::Result<()> {
    let dir = tao_core::recorder::session_dir(cwd)
        .ok_or_else(|| anyhow::anyhow!("HOME 未设置,无法定位会话目录"))?;
    if !dir.exists() {
        println!("(无会话)");
        return Ok(());
    }

    // 加载所有会话(id → SessionState + size + mtime)
    let mut sessions: Vec<(tao_core::replay::SessionState, u64, u64)> = Vec::new();
    for e in std::fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
        let path = e.path();
        if path.extension().is_some_and(|x| x == "jsonl") {
            let meta = e.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if let Ok(state) = tao_core::replay::replay(&path) {
                sessions.push((state, size, mtime));
            }
        }
    }

    if sessions.is_empty() {
        println!("(无会话)");
        return Ok(());
    }

    // 按 mtime 倒序
    sessions.sort_by_key(|b| std::cmp::Reverse(b.2));

    // 构建父子映射(id → children)
    use std::collections::HashMap;
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, (s, _, _)) in sessions.iter().enumerate() {
        if let Some(p) = &s.parent {
            let pid = p.as_ref().to_string();
            children.entry(pid).or_default().push(i);
        } else {
            roots.push(i);
        }
    }

    // 递归打印树
    fn print_tree(
        sessions: &[(tao_core::replay::SessionState, u64, u64)],
        children: &HashMap<String, Vec<usize>>,
        idx: usize,
        depth: usize,
    ) {
        let (s, size, _mtime) = &sessions[idx];
        let indent = "  ".repeat(depth);
        let title = s.title.as_deref().unwrap_or_else(|| {
            s.messages
                .iter()
                .find_map(|m| match m {
                    tao_core::model::ModelMessage::User { content } => {
                        content.iter().find_map(|c| match c {
                            tao_core::model::ModelContent::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .unwrap_or("(无标题)")
        });
        let title_trunc: String = title.chars().take(40).collect();
        let id_short = &s.id.as_ref().to_string()[..8];
        println!(
            "{indent}{title_trunc}  [{id_short}]  {}msg  {size}B",
            s.messages.len()
        );

        if let Some(kids) = children.get(s.id.as_ref().to_string().as_str()) {
            for &k in kids {
                print_tree(sessions, children, k, depth + 1);
            }
        }
    }

    for &r in &roots {
        print_tree(&sessions, &children, r, 0);
    }
    // 无 parent 但 parent 不在列表中的(孤儿)
    for (i, (s, _, _)) in sessions.iter().enumerate() {
        if let Some(p) = &s.parent {
            let pid = p.as_ref().to_string();
            if !sessions.iter().any(|(ss, _, _)| ss.id.as_ref() == pid) {
                print_tree(&sessions, &children, i, 0);
            }
        }
    }

    Ok(())
}

fn run_sessions_show(cwd: &std::path::Path, id: &str) -> anyhow::Result<()> {
    let sid = tao_protocol::ids::SessionId::new(id.to_string());
    let path = tao_core::recorder::session_file_path(cwd, &sid)
        .ok_or_else(|| anyhow::anyhow!("HOME 未设置,无法定位会话日志"))?;
    let state =
        tao_core::replay::replay(&path).map_err(|e| anyhow::anyhow!("读取会话失败: {e}"))?;

    println!("═══ 会话 {} ═══", state.id.as_ref());
    println!(
        "parent: {}",
        state
            .parent
            .as_ref()
            .map(|p| p.as_ref().to_string())
            .unwrap_or_else(|| "(根)".into())
    );
    println!("cwd:    {}", state.cwd.display());
    println!("title:  {}", state.title.as_deref().unwrap_or("(无)"));
    println!("messages: {}", state.messages.len());
    println!();

    for (i, msg) in state.messages.iter().enumerate() {
        let (role, text) = match msg {
            tao_core::model::ModelMessage::User { content } => (
                "用户",
                content
                    .iter()
                    .filter_map(|c| {
                        if let tao_core::model::ModelContent::Text(t) = c {
                            Some(t.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            tao_core::model::ModelMessage::Assistant { content } => (
                "助手",
                content
                    .iter()
                    .filter_map(|c| {
                        if let tao_core::model::ModelContent::Text(t) = c {
                            Some(t.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            tao_core::model::ModelMessage::ToolResult {
                content, is_error, ..
            } => {
                let t = content
                    .iter()
                    .filter_map(|c| {
                        if let tao_core::model::ModelContent::Text(t) = c {
                            Some(t.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                (
                    "工具",
                    format!("[{}] {}", if *is_error { "✗" } else { "✓" }, t),
                )
            }
        };
        let trunc = if text.len() > 100 {
            &text[..100]
        } else {
            &text
        };
        println!("[{i}] {role}: {trunc}");
    }

    Ok(())
}

fn run_sessions_audit(cwd: &std::path::Path, id: &str) -> anyhow::Result<()> {
    let sid = tao_protocol::ids::SessionId::new(id.to_string());
    let path = tao_core::recorder::session_file_path(cwd, &sid)
        .ok_or_else(|| anyhow::anyhow!("HOME 未设置,无法定位会话日志"))?;
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", path.display()))?;
    for line in content.lines() {
        let ll: tao_protocol::log::LogLine = match serde_json::from_str(line) {
            Ok(l) => l,
            Err(_) => continue,
        };
        match ll.event {
            tao_protocol::log::LogEvent::PermissionDecision { tool, decision } => {
                println!(
                    "[seq {}] 判定 {tool}: {:?} 来源 {:?}",
                    ll.seq, decision.verdict, decision.source
                );
            }
            tao_protocol::log::LogEvent::Approval {
                call_id,
                verdict,
                rule_suggestion,
            } => {
                println!(
                    "[seq {}] 审批 {} → {:?} {}",
                    ll.seq,
                    call_id,
                    verdict,
                    rule_suggestion.unwrap_or_default()
                );
            }
            tao_protocol::log::LogEvent::PermissionGrant { tool, pattern } => {
                println!("[seq {}] 会话授权 {tool}: {pattern}", ll.seq);
            }
            _ => {}
        }
    }
    Ok(())
}

fn run_sessions_gc(cwd: &std::path::Path, keep_days: u32) -> anyhow::Result<()> {
    let dir = tao_core::recorder::session_dir(cwd)
        .ok_or_else(|| anyhow::anyhow!("HOME 未设置,无法定位会话目录"))?;
    if !dir.exists() {
        return Ok(());
    }
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(keep_days as u64 * 86400);
    let mut removed = 0;
    for e in std::fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
        let path = e.path();
        if path.extension().is_some_and(|x| x == "jsonl")
            && let Ok(meta) = e.metadata()
            && let Ok(mtime) = meta.modified()
            && mtime < cutoff
            && std::fs::remove_file(&path).is_ok()
        {
            println!("删除 {}", path.display());
            removed += 1;
        }
    }
    println!("清理完成,删除 {removed} 个会话");
    Ok(())
}
