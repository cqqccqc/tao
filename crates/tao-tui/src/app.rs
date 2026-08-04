//! TUI 主循环:tokio select! crossterm 事件 + TurnEvent 流 + tick。
//! 见 docs/design/tui.md §2。
//!
//! 支持 `--resume <id>` / `--fork`(replay 历史 → messages 注入 state;
//! mode/grants 恢复留 TODO)。

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tao_core::HooksConfig;
use tao_core::config::{Config, LoadOpts};
use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::permissions::{ApprovalRequest, Approver, PermissionEngine};
use tao_core::providers::ModelClient;
use tao_core::providers::registry::resolve;
use tao_core::recorder::{JsonlRecorder, Recorder, session_file_path};
use tao_core::replay::replay;
use tao_core::session::{TurnConfig, TurnEvent, run_turn};
use tao_core::tools::ToolRegistry;
use tao_protocol::content::{Content, TokenUsage};
use tao_protocol::ids::{SessionId, TurnId};
use tao_protocol::log::LogEvent;
use tao_protocol::op::ReviewDecision;
use tao_protocol::permission::PermissionMode;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::render::{HistoryCell, PendingApproval, RenderState};

type Term = Terminal<CrosstermBackend<Stdout>>;

pub struct TuiOpts {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub resume: Option<String>,
    pub fork: bool,
    pub load_opts: LoadOpts,
}

/// TUI 审批器:通过 oneshot + HashMap 与主循环按 call_id 配对。
/// `request()` 注册 oneshot 并 await;主循环按键后调 `respond()` 送回决定。
struct TuiApprover {
    pending: Mutex<HashMap<String, oneshot::Sender<ReviewDecision>>>,
}

impl TuiApprover {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// 主循环按键后调用:把决定送回对应 `request()` 的 await。
    fn respond(&self, call_id: &str, decision: ReviewDecision) {
        if let Some(tx) = self.pending.lock().unwrap().remove(call_id) {
            let _ = tx.send(decision);
        }
    }
}

#[async_trait]
impl Approver for TuiApprover {
    async fn request(&self, req: ApprovalRequest) -> ReviewDecision {
        let (tx, rx) = oneshot::channel();
        let key = req.call_id.to_string();
        self.pending.lock().unwrap().insert(key, tx);
        // 通道关闭(如 turn 被中断)→ 默认 Deny,安全侧。
        rx.await.unwrap_or(ReviewDecision::Deny)
    }
}

pub async fn run() -> anyhow::Result<()> {
    run_with_load_opts(LoadOpts::default(), None, false).await
}

pub async fn run_with_load_opts(
    load_opts: LoadOpts,
    resume: Option<String>,
    fork: bool,
) -> anyhow::Result<()> {
    let opts = TuiOpts {
        cwd: std::env::current_dir()?,
        model: None,
        resume,
        fork,
        load_opts,
    };
    run_with_opts(opts).await
}

async fn run_with_opts(opts: TuiOpts) -> anyhow::Result<()> {
    let config = Config::load(&opts.load_opts)?;
    let (client, default_model) = resolve(&config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let model = opts.model.unwrap_or(default_model);
    let engine = Arc::new(PermissionEngine::new(
        config.permission_mode,
        config.permissions.rules.clone(),
    ));
    // 新会话 / resume(--resume + --fork)
    let (recorder, session_id, messages) = match &opts.resume {
        Some(id_str) => {
            let parent = SessionId::new(id_str.clone());
            let path = session_file_path(&opts.cwd, &parent)
                .ok_or_else(|| anyhow::anyhow!("HOME 未设置,无法定位会话日志"))?;
            let state = replay(&path).map_err(|e| anyhow::anyhow!("重放会话失败: {e}"))?;
            // TODO: 恢复 state.mode + session_grants 到 engine(v1 简化用 config.permission_mode)
            let fp = config.config_fingerprint(&model);
            let (r, sid) = if opts.fork {
                JsonlRecorder::create_fork(&opts.cwd, &parent, fp)
                    .map_err(|e| anyhow::anyhow!("创建 fork 会话失败: {e}"))?
            } else {
                (
                    JsonlRecorder::open_existing(&parent, &opts.cwd, fp)
                        .map_err(|e| anyhow::anyhow!("打开会话失败: {e}"))?,
                    parent,
                )
            };
            (r, sid, state.messages)
        }
        None => {
            let fp = config.config_fingerprint(&model);
            let (r, sid) = JsonlRecorder::create(&opts.cwd, fp)
                .map_err(|e| anyhow::anyhow!("创建会话日志失败: {e}"))?;
            (r, sid, Vec::new())
        }
    };

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut tools = ToolRegistry::builtin();
    tao_mcp::load_mcp_tools(
        &mut tools,
        &config.mcp_servers,
        config.mcp_tool_budget,
        config.mcp_lazy,
    )
    .await;
    let tools = Arc::new(tools);
    let recorder = Arc::new(recorder);
    let result = app_loop(
        &mut terminal,
        client,
        model,
        &opts.cwd,
        engine,
        recorder,
        config.hooks,
        tools,
        session_id,
        messages,
    )
    .await;

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    result
}

struct UiState {
    input: String,
    history: Vec<HistoryCell>,
    live_text: String,
    tool_status: Option<(String, bool)>,
    messages: Vec<ModelMessage>,
    running: bool,
    error: Option<String>,
    mode: PermissionMode,
    pending_approval: Option<PendingApproval>,
    current_cancel: Option<CancellationToken>,
    /// 累计 token 用量(由 TurnEvent::Usage 更新,/cost 展示)。
    usage: TokenUsage,
    /// L4b viewport 滚动偏移:N = 从底部往上 N 行(0 = 跟随底部/最新)。
    /// 新内容到达(history.push / live_text 更新)时 reset 为 0(跟底);
    /// PgUp 增大(看旧),PgDn 减小(回底部)。见 `push_history` / `push_live`。
    scroll_offset: usize,
}

impl UiState {
    fn new(mode: PermissionMode) -> Self {
        Self {
            input: String::new(),
            history: Vec::new(),
            live_text: String::new(),
            tool_status: None,
            messages: Vec::new(),
            running: false,
            error: None,
            mode,
            pending_approval: None,
            current_cancel: None,
            usage: TokenUsage::default(),
            scroll_offset: 0,
        }
    }

    /// 推入历史格子 + 重置 viewport 跟随底部(auto-follow)。
    fn push_history(&mut self, cell: HistoryCell) {
        self.history.push(cell);
        self.scroll_offset = 0;
    }

    /// 追加 live 流式文本 + 重置 viewport 跟随底部(auto-follow)。
    fn push_live(&mut self, t: &str) {
        self.live_text.push_str(t);
        self.scroll_offset = 0;
    }
}

#[allow(clippy::too_many_arguments)]
async fn app_loop(
    terminal: &mut Term,
    client: Arc<dyn ModelClient>,
    model: String,
    cwd: &std::path::Path,
    engine: Arc<PermissionEngine>,
    recorder: Arc<JsonlRecorder>,
    hooks: HooksConfig,
    tools: Arc<ToolRegistry>,
    session_id: SessionId,
    messages: Vec<ModelMessage>,
) -> anyhow::Result<()> {
    let mut state = UiState::new(engine.mode());
    state.messages = messages;
    let approver = Arc::new(TuiApprover::new());
    let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEvent>();

    // 后台读 crossterm 事件(避免阻塞 tokio)。
    let (key_tx, mut key_rx) = mpsc::channel::<KeyEvent>(64);
    tokio::spawn(async move {
        loop {
            if let Ok(true) = event::poll(std::time::Duration::from_millis(50))
                && let Ok(Event::Key(k)) = event::read()
                && key_tx.send(k).await.is_err()
            {
                break;
            }
        }
    });

    let mut system: Vec<SystemBlock> = Vec::new();
    if let Some(instr) = tao_core::instructions::load(cwd) {
        system.push(SystemBlock {
            text: instr,
            cache_breakpoint: None,
        });
    }
    if let Some(p) = tao_core::skills::skills_prompt(&tao_core::skills::load_skills(cwd)) {
        system.push(SystemBlock {
            text: p,
            cache_breakpoint: None,
        });
    }
    system.push(SystemBlock {
        text: "你是 tao,一个 Rust 编写的 coding agent。\
               你可以通过 Bash / Read / Write / Edit / Patch / Grep / Glob 工具帮助用户完成任务。\
               优先用最少的步骤解决问题。"
            .into(),
        cache_breakpoint: None,
    });

    loop {
        let render_state = RenderState {
            input: &state.input,
            history: &state.history,
            live_text: &state.live_text,
            tool_status: state.tool_status.as_ref(),
            running: state.running,
            error: state.error.as_deref(),
            model: &model,
            mode: state.mode,
            pending_approval: state.pending_approval.as_ref(),
            scroll_offset: state.scroll_offset,
        };
        terminal.draw(|f| crate::render::draw(f, &render_state))?;

        tokio::select! {
            Some(key) = key_rx.recv() => {
                if handle_key(key, &mut state, &client, &tools, &system, &model, cwd, &turn_tx, &engine, &approver, &recorder, &hooks, &session_id).await? {
                    return Ok(());
                }
            }
            Some(ev) = turn_rx.recv() => {
                handle_turn_event(ev, &mut state);
            }
        }
    }
}

/// 处理按键。返回 true 表示退出。
#[allow(clippy::too_many_arguments)]
async fn handle_key(
    key: KeyEvent,
    state: &mut UiState,
    client: &Arc<dyn ModelClient>,
    tools: &Arc<ToolRegistry>,
    system: &[SystemBlock],
    model: &str,
    cwd: &std::path::Path,
    turn_tx: &mpsc::UnboundedSender<TurnEvent>,
    engine: &Arc<PermissionEngine>,
    approver: &Arc<TuiApprover>,
    recorder: &Arc<JsonlRecorder>,
    hooks: &HooksConfig,
    session_id: &SessionId,
) -> anyhow::Result<bool> {
    // 审批弹窗优先处理按键(y/s/n/a)
    if let Some(pending) = state.pending_approval.clone() {
        let decision = match key.code {
            KeyCode::Char('y') => Some(ReviewDecision::Approve),
            KeyCode::Char('s') => Some(ReviewDecision::ApproveForSession),
            KeyCode::Char('n') => Some(ReviewDecision::Deny),
            KeyCode::Char('a') => Some(ReviewDecision::Abort),
            _ => None,
        };
        if let Some(d) = decision {
            approver.respond(pending.call_id.as_ref(), d);
            state.pending_approval = None;
        }
        return Ok(false);
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
        | (KeyCode::Char('d'), KeyModifiers::CONTROL) => Ok(true),
        // shift+tab 循环模式 default → plan → accept-edits → default(不含 bypass)
        (KeyCode::BackTab, _) => {
            state.mode = match state.mode {
                PermissionMode::Default => PermissionMode::Plan,
                PermissionMode::Plan => PermissionMode::AcceptEdits,
                PermissionMode::AcceptEdits => PermissionMode::Default,
                PermissionMode::Bypass => PermissionMode::Bypass,
            };
            engine.set_mode(state.mode);
            Ok(false)
        }
        // L4b viewport 滚动:scroll_offset = 从底部往上 N 行(0 = 跟随底部/最新)。
        // PgUp 增大(看旧),PgDn 减小(回底部);新内容到达自动 reset 为 0。
        (KeyCode::PageUp, _) => {
            let page = page_height();
            state.scroll_offset = state.scroll_offset.saturating_add(page);
            Ok(false)
        }
        (KeyCode::PageDown, _) => {
            let page = page_height();
            state.scroll_offset = state.scroll_offset.saturating_sub(page);
            Ok(false)
        }
        (KeyCode::Enter, _) if !state.running => {
            let prompt = state.input.clone();
            if !prompt.is_empty() {
                state.input.clear();

                // slash 命令(内置)
                if prompt.starts_with('/')
                    && let Some(builtin) = tao_core::commands::parse_builtin(&prompt)
                {
                    match builtin {
                        tao_core::commands::Builtin::Help => {
                            let cmds = tao_core::commands::load_commands(cwd);
                            let mut msg = String::from(
                                "内置:/help /clear /mode [default|plan|accept-edits] /compact /sessions /cost /rewind /rollback /diff\n",
                            );
                            if !cmds.is_empty() {
                                msg.push_str("自定义:");
                                for c in &cmds {
                                    msg.push_str(&format!("\n  /{}: {}", c.name, c.description));
                                }
                            }
                            state.push_history(HistoryCell::tool(&msg));
                        }
                        tao_core::commands::Builtin::Clear => {
                            state.history.clear();
                            state.messages.clear();
                            state.error = None;
                        }
                        tao_core::commands::Builtin::Mode(m) => {
                            state.mode = m;
                            engine.set_mode(m);
                        }
                        tao_core::commands::Builtin::ModeCycle => {
                            state.mode = match state.mode {
                                PermissionMode::Default => PermissionMode::Plan,
                                PermissionMode::Plan => PermissionMode::AcceptEdits,
                                PermissionMode::AcceptEdits => PermissionMode::Default,
                                PermissionMode::Bypass => PermissionMode::Bypass,
                            };
                            engine.set_mode(state.mode);
                        }
                        tao_core::commands::Builtin::Sessions => {
                            let msg = match tao_core::recorder::session_dir(cwd) {
                                Some(dir) => list_sessions(&dir),
                                None => "HOME 未设置,无法定位会话".into(),
                            };
                            state.push_history(HistoryCell::tool(&msg));
                        }
                        tao_core::commands::Builtin::Compact => {
                            state.running = true;
                            match tao_core::compact::compact(
                                client.as_ref(),
                                model,
                                &state.messages,
                                tao_core::DEFAULT_KEEP_LAST,
                                &**recorder,
                            )
                            .await
                            {
                                Ok(m) => {
                                    state.messages = m;
                                    state.push_history(HistoryCell::tool("已压缩上下文"));
                                }
                                Err(e) => state.error = Some(format!("压缩失败: {e}")),
                            }
                            state.running = false;
                        }
                        tao_core::commands::Builtin::Init => {
                            let path = cwd.join("AGENTS.md");
                            let existed = path.exists();
                            let tmpl = "# AGENTS.md\n\n本项目对 tao 的指令:\n\n- 编码规范:\n- 优先:\n- 避免:\n";
                            match std::fs::write(&path, tmpl) {
                                Ok(_) => state.push_history(HistoryCell::tool(&format!(
                                    "{} AGENTS.md(模板,请编辑)",
                                    if existed { "overwrote" } else { "created" }
                                ))),
                                Err(e) => state.error = Some(format!("写入失败: {e}")),
                            }
                        }
                        tao_core::commands::Builtin::Hooks => {
                            let msg = format!(
                                "pre_tool_use:{} post_tool_use:{} session_start:{} session_end:{} \
                                 user_prompt_submit:{} notification:{} subagent_stop:{} stop:{}",
                                hooks.pre_tool_use.len(),
                                hooks.post_tool_use.len(),
                                hooks.session_start.len(),
                                hooks.session_end.len(),
                                hooks.user_prompt_submit.len(),
                                hooks.notification.len(),
                                hooks.subagent_stop.len(),
                                hooks.stop.len(),
                            );
                            state.push_history(HistoryCell::tool(&msg));
                        }
                        tao_core::commands::Builtin::Mcp => {
                            let mcp: Vec<String> = tools
                                .specs()
                                .iter()
                                .filter_map(|s| {
                                    let p: Vec<&str> = s.name.splitn(3, "__").collect();
                                    (p.len() == 3 && p[0] == "mcp")
                                        .then(|| format!("{} ({}): {}", p[1], p[2], s.description))
                                })
                                .collect();
                            let msg = if mcp.is_empty() {
                                "无 MCP 工具".into()
                            } else {
                                mcp.join("\n")
                            };
                            state.push_history(HistoryCell::tool(&msg));
                        }
                        tao_core::commands::Builtin::Agent => {
                            let agents = tao_core::load_agents(cwd);
                            let msg = if agents.is_empty() {
                                "无子 agent".into()
                            } else {
                                agents
                                    .iter()
                                    .map(|a| format!("- {}: {}", a.name, a.description))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            };
                            state.push_history(HistoryCell::tool(&msg));
                        }
                        tao_core::commands::Builtin::Model => {
                            state.push_history(HistoryCell::tool(&format!("model: {model}")));
                        }
                        tao_core::commands::Builtin::Cost => {
                            let u = &state.usage;
                            state.push_history(HistoryCell::tool(&format!(
                                "usage: input={} (cached={}), output={} (reasoning={}), total={}",
                                u.input,
                                u.cached_input,
                                u.output,
                                u.reasoning,
                                u.total(),
                            )));
                        }
                        tao_core::commands::Builtin::Rewind => {
                            // 列出最近 checkpoint 栈(最多 20 个)。
                            // /rewind 仅显示列表;回退到最近 checkpoint 仍用 /rollback。
                            // TODO:支持 /rewind N 回退到第 N 个 checkpoint(多级回退)。
                            match tao_core::ShadowRepo::init(cwd) {
                                Ok(shadow) => match shadow.checkpoint_history(20) {
                                    Ok(rows) if rows.is_empty() => {
                                        state.push_history(HistoryCell::tool(
                                            "无 checkpoint(Edit/Write 前自动快照)",
                                        ));
                                    }
                                    Ok(rows) => {
                                        let mut msg = String::from("checkpoint 栈:\n");
                                        for (i, (hash, time)) in rows.iter().enumerate() {
                                            msg.push_str(&format!(
                                                "  {:>2}. {}  {}  tao checkpoint\n",
                                                i + 1,
                                                hash,
                                                time,
                                            ));
                                        }
                                        msg.push_str("\n回退到最近 checkpoint 用 /rollback");
                                        state.push_history(HistoryCell::tool(&msg));
                                    }
                                    Err(e) => {
                                        state.error = Some(format!("读取 checkpoint 栈失败: {e}"))
                                    }
                                },
                                Err(e) => state.error = Some(format!("shadow 仓库初始化失败: {e}")),
                            }
                        }
                        tao_core::commands::Builtin::Rollback => {
                            // 回滚文件到最近一次 shadow checkpoint(HEAD)。
                            // /rewind 可查看 checkpoint 栈;多级回退(/rewind N)TODO。
                            match tao_core::ShadowRepo::init(cwd) {
                                Ok(shadow) => match shadow.latest_commit() {
                                    Ok(Some(hash)) => {
                                        let short: String = hash.chars().take(8).collect();
                                        match shadow.rollback(&hash) {
                                            Ok(()) => state.push_history(HistoryCell::tool(
                                                &format!("已回滚到最近 checkpoint {short}"),
                                            )),
                                            Err(e) => {
                                                state.error = Some(format!("rollback 失败: {e}"))
                                            }
                                        }
                                    }
                                    Ok(None) => state.push_history(HistoryCell::tool(
                                        "无 checkpoint(Edit/Write 前自动快照)",
                                    )),
                                    Err(e) => {
                                        state.error = Some(format!("读取最近 checkpoint 失败: {e}"))
                                    }
                                },
                                Err(e) => state.error = Some(format!("shadow 仓库初始化失败: {e}")),
                            }
                        }
                        tao_core::commands::Builtin::Diff => {
                            // 显示最近 checkpoint 的改动 diff stat(`git show HEAD --stat`)。
                            match tao_core::ShadowRepo::init(cwd) {
                                Ok(shadow) => match shadow.diff_last() {
                                    Ok(diff) if diff.is_empty() => {
                                        state.push_history(HistoryCell::tool(
                                            "无 checkpoint diff(Edit/Write 前自动快照)",
                                        ));
                                    }
                                    Ok(diff) => {
                                        state.push_history(HistoryCell::tool(&diff));
                                    }
                                    Err(e) => state.error = Some(format!("读取 diff 失败: {e}")),
                                },
                                Err(e) => state.error = Some(format!("shadow 仓库初始化失败: {e}")),
                            }
                        }
                    }
                    return Ok(false);
                }

                // user 消息文本(slash markdown 模板展开 or 普通 prompt)
                let user_text = if prompt.starts_with('/') {
                    let (name, args) = tao_core::commands::split_name_args(&prompt);
                    let cmds = tao_core::commands::load_commands(cwd);
                    match cmds.iter().find(|c| c.name == name) {
                        Some(cmd) => tao_core::commands::expand(&cmd.body, &args, cwd),
                        None => {
                            state.error = Some(format!("未知命令: {prompt}"));
                            return Ok(false);
                        }
                    }
                } else {
                    prompt
                };

                let turn_id = TurnId::new(uuid::Uuid::new_v4().to_string());
                recorder.record(LogEvent::UserInput {
                    content: vec![Content::text(&user_text)],
                    turn_id: turn_id.clone(),
                });
                state.push_history(HistoryCell::user(&user_text));
                state.messages.push(ModelMessage::User {
                    content: vec![ModelContent::text(user_text)],
                });
                state.running = true;
                state.live_text.clear();
                state.error = None;

                let client = client.clone();
                let tools = tools.clone();
                let system = system.to_vec();
                let cwd = cwd.to_path_buf();
                let model = model.to_string();
                let tx = turn_tx.clone();
                let mut messages = state.messages.clone();
                let cancel = CancellationToken::new();
                let shadow = tao_core::ShadowRepo::init(&cwd).ok();
                state.current_cancel = Some(cancel.clone());
                let engine = engine.clone();
                let approver = approver.clone();
                let recorder = recorder.clone();
                let hooks = hooks.clone();
                let session_id = session_id.clone();

                tokio::spawn(async move {
                    let req = ModelRequest {
                        model,
                        system,
                        messages: vec![],
                        tools: vec![],
                        reasoning: None,
                        max_output_tokens: 4096,
                        temperature: None,
                        metadata: RequestMeta {
                            session_id: Some(session_id.as_ref().to_string()),
                            turn_id: Some(turn_id.as_ref().to_string()),
                        },
                    };
                    let config = TurnConfig::default();
                    let result = run_turn(
                        client.as_ref(),
                        &tools,
                        &engine,
                        &*approver,
                        &*recorder,
                        &hooks,
                        shadow.as_ref(),
                        &req,
                        &mut messages,
                        &config,
                        &cwd,
                        &cancel,
                        |ev| {
                            let _ = tx.send(ev);
                        },
                    )
                    .await;
                    if let Err(e) = result {
                        let _ = tx.send(TurnEvent::Error(e.to_string()));
                    }
                    let _ = tx.send(TurnEvent::TurnComplete {
                        stop_reason: tao_protocol::content::StopReason::EndTurn,
                        steps: 0,
                    });
                });
            }
            Ok(false)
        }
        (KeyCode::Esc, _) if state.running => {
            if let Some(c) = state.current_cancel.take() {
                c.cancel();
            }
            Ok(false)
        }
        (KeyCode::Char(c), _) if !state.running => {
            state.input.push(c);
            Ok(false)
        }
        (KeyCode::Backspace, _) if !state.running => {
            state.input.pop();
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn handle_turn_event(ev: TurnEvent, state: &mut UiState) {
    match ev {
        TurnEvent::TextDelta(t) => {
            state.push_live(&t);
        }
        TurnEvent::ToolCallBegin { tool, .. } => {
            state.tool_status = Some((format!("▶ {tool}"), true));
        }
        TurnEvent::ToolExecEnd { ok, summary, .. } => {
            let mark = if ok { "✓" } else { "✗" };
            let status = format!("{mark} {summary}");
            if !state.live_text.is_empty() {
                let cell = HistoryCell::assistant(std::mem::take(&mut state.live_text));
                state.push_history(cell);
            }
            state.push_history(HistoryCell::tool(&status));
            state.tool_status = Some((status, ok));
        }
        TurnEvent::ApprovalRequest {
            call_id,
            kind,
            detail,
        } => {
            state.pending_approval = Some(PendingApproval {
                call_id,
                kind,
                tool: detail.tool.unwrap_or_default(),
                command: detail.command,
                pattern_suggestion: detail.pattern_suggestion,
            });
        }
        TurnEvent::ApprovalResolved { .. } => {
            state.pending_approval = None;
        }
        TurnEvent::TurnComplete { .. } => {
            if !state.live_text.is_empty() {
                let cell = HistoryCell::assistant(std::mem::take(&mut state.live_text));
                state.push_history(cell);
            }
            state.running = false;
            state.tool_status = None;
            state.current_cancel = None;
        }
        TurnEvent::Error(msg) => {
            state.error = Some(msg);
            state.running = false;
            state.current_cancel = None;
        }
        TurnEvent::Usage(u) => {
            // 累计 input/output(每轮模型流结束发一次)。
            state.usage.input += u.input;
            state.usage.cached_input += u.cached_input;
            state.usage.output += u.output;
            state.usage.reasoning += u.reasoning;
        }
        _ => {}
    }
}

/// 视口页高:终端高度减去状态行(1)+ 输入框(3)。
/// 用于 PgUp/PgDn 滚动步长。取不到时 fallback 20。
fn page_height() -> usize {
    crossterm::terminal::size()
        .map(|(_, h)| (h as usize).saturating_sub(4))
        .unwrap_or(20)
        .max(1)
}

/// 列会话目录(TUI `/sessions` 用,含 title)。
fn list_sessions(dir: &std::path::Path) -> String {
    if !dir.exists() {
        return "(无会话)".into();
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.metadata().ok().and_then(|m| m.modified().ok())));
    if entries.is_empty() {
        return "(无会话)".into();
    }
    let mut s = String::new();
    for e in entries {
        let path = e.path();
        let id = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let id_short = &id[..8.min(id.len())];
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        // replay 提取 title
        let title = tao_core::replay::replay(&path)
            .ok()
            .and_then(|st| {
                st.title.or_else(|| {
                    st.messages.iter().find_map(|m| match m {
                        tao_core::model::ModelMessage::User { content } => {
                            content.iter().find_map(|c| match c {
                                tao_core::model::ModelContent::Text(t) => Some(t.clone()),
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                })
            })
            .unwrap_or_else(|| "(无标题)".into());
        let title_trunc = if title.len() > 30 {
            &title[..30]
        } else {
            &title
        };
        s.push_str(&format!("{title_trunc}  [{id_short}]  {size}B\n"));
    }
    s
}
