//! TUI 主循环:tokio select! crossterm 事件 + TurnEvent 流 + tick。
//! 见 docs/design/tui.md §2。

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tao_core::config::{Config, LoadOpts};
use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::providers::ModelClient;
use tao_core::providers::registry::resolve;
use tao_core::session::{TurnConfig, TurnEvent, run_turn};
use tao_core::tools::ToolRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::render::{HistoryCell, RenderState};

type Term = Terminal<CrosstermBackend<Stdout>>;

pub struct TuiOpts {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub load_opts: LoadOpts,
}

pub async fn run() -> anyhow::Result<()> {
    run_with_load_opts(LoadOpts::default()).await
}

pub async fn run_with_load_opts(load_opts: LoadOpts) -> anyhow::Result<()> {
    let opts = TuiOpts {
        cwd: std::env::current_dir()?,
        model: None,
        load_opts,
    };
    run_with_opts(opts).await
}

async fn run_with_opts(opts: TuiOpts) -> anyhow::Result<()> {
    let config = Config::load(&opts.load_opts)?;
    let (client, default_model) = resolve(&config).map_err(|e| anyhow::anyhow!("{e}"))?;
    let model = opts.model.unwrap_or(default_model);

    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = app_loop(&mut terminal, client, model, &opts.cwd).await;

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
}

impl UiState {
    fn new() -> Self {
        Self {
            input: String::new(),
            history: Vec::new(),
            live_text: String::new(),
            tool_status: None,
            messages: Vec::new(),
            running: false,
            error: None,
        }
    }
}

async fn app_loop(
    terminal: &mut Term,
    client: Arc<dyn ModelClient>,
    model: String,
    cwd: &std::path::Path,
) -> anyhow::Result<()> {
    let mut state = UiState::new();
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

    let tools = Arc::new(ToolRegistry::builtin());
    let system = vec![SystemBlock {
        text: "你是 tao,一个 Rust 编写的 coding agent。\
               你可以通过 Bash / Read / Write 工具帮助用户完成任务。\
               优先用最少的步骤解决问题。"
            .into(),
        cache_breakpoint: None,
    }];

    loop {
        let render_state = RenderState {
            input: &state.input,
            history: &state.history,
            live_text: &state.live_text,
            tool_status: state.tool_status.as_ref(),
            running: state.running,
            error: state.error.as_deref(),
            model: &model,
        };
        terminal.draw(|f| crate::render::draw(f, &render_state))?;

        tokio::select! {
            Some(key) = key_rx.recv() => {
                if handle_key(key, &mut state, &client, &tools, &system, &model, cwd, &turn_tx).await? {
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
) -> anyhow::Result<bool> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
        | (KeyCode::Char('d'), KeyModifiers::CONTROL) => Ok(true),
        (KeyCode::Enter, _) if !state.running => {
            let prompt = state.input.clone();
            if !prompt.is_empty() {
                state.input.clear();
                state.history.push(HistoryCell::user(&prompt));
                state.messages.push(ModelMessage::User {
                    content: vec![ModelContent::text(prompt)],
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

                tokio::spawn(async move {
                    let req = ModelRequest {
                        model,
                        system,
                        messages: vec![],
                        tools: vec![],
                        reasoning: None,
                        max_output_tokens: 4096,
                        temperature: None,
                        metadata: RequestMeta::default(),
                    };
                    let config = TurnConfig::default();
                    let result = run_turn(
                        client.as_ref(),
                        &tools,
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
            state.error = Some("M1 无法中途中断 turn(按 Ctrl+C 退出)".into());
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
            state.live_text.push_str(&t);
        }
        TurnEvent::ToolCallBegin { tool, .. } => {
            state.tool_status = Some((format!("▶ {tool}"), true));
        }
        TurnEvent::ToolExecEnd { ok, summary, .. } => {
            let mark = if ok { "✓" } else { "✗" };
            let status = format!("{mark} {summary}");
            if !state.live_text.is_empty() {
                state
                    .history
                    .push(HistoryCell::assistant(std::mem::take(&mut state.live_text)));
            }
            state.history.push(HistoryCell::tool(&status));
            state.tool_status = Some((status, ok));
        }
        TurnEvent::TurnComplete { .. } => {
            if !state.live_text.is_empty() {
                state
                    .history
                    .push(HistoryCell::assistant(std::mem::take(&mut state.live_text)));
            }
            state.running = false;
            state.tool_status = None;
        }
        TurnEvent::Error(msg) => {
            state.error = Some(msg);
            state.running = false;
        }
        _ => {}
    }
}
