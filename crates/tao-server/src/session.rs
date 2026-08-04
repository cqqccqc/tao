//! Wire 驱动层:把 `Op` 翻译成 core 调用,把 `TurnEvent` 翻译成 `EventMsg`,
//! 通过 `broadcast` 扇出给所有 attach 的客户端。proto(stdio)与 serve(TCP)共用。
//! 见 docs/design/protocol.md §5。
//!
//! v1:单 session;turn FIFO 排队;审批 oneshot 配对(`call_id` 回显);
//! `GetHistory`/`Compact`/`ResumeSession`/`CheckpointRollback` 等留后续(TODO)。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tao_protocol::event::SessionSummary;
use tao_protocol::ids::{SessionId, TurnId};
use tao_protocol::op::{Op, ReviewDecision, UserInput};
use tao_protocol::wire::PROTOCOL_VERSION;
use tao_protocol::{Content, Event, EventMsg, Submission};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use tao_core::config::Config;
use tao_core::model::{ModelContent, ModelMessage, ModelRequest, RequestMeta, SystemBlock};
use tao_core::permissions::{ApprovalRequest, Approver, PermissionEngine};
use tao_core::providers::ModelClient;
use tao_core::providers::registry::resolve;
use tao_core::recorder::{JsonlRecorder, Recorder, session_dir};
use tao_core::replay::replay;
use tao_core::session::{TurnConfig, TurnEvent, run_turn};
use tao_core::tools::ToolRegistry;

const EVENT_CHANNEL_CAPACITY: usize = 1024;
const OP_CHANNEL_CAPACITY: usize = 256;

/// per-session 共享状态(actor 与 turn task 共享)。
struct Shared {
    cwd: PathBuf,
    model: std::sync::Mutex<String>,
    client: Arc<dyn ModelClient>,
    tools: Arc<ToolRegistry>,
    engine: Arc<PermissionEngine>,
    /// Mutex 包裹以支持 ResumeSession 运行时切 recorder(fork / append)。
    recorder: std::sync::Mutex<Arc<JsonlRecorder>>,
    messages: Mutex<Vec<ModelMessage>>,
    system: Vec<SystemBlock>,
    config: Arc<Config>,
    /// Mutex 包裹:fork-resume 后 session_id 会变。
    session_id: std::sync::Mutex<SessionId>,
    seq: AtomicU64,
    event_tx: broadcast::Sender<Event>,
    pending_approvals: Mutex<HashMap<String, oneshot::Sender<ReviewDecision>>>,
}

impl Shared {
    /// 构造并发送一个 Event(seq 自增,broadcast 扇出)。
    fn emit(&self, id: &str, turn: Option<TurnId>, msg: EventMsg) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let ev = Event {
            id: id.to_string(),
            seq,
            turn,
            msg,
        };
        let _ = self.event_tx.send(ev);
    }

    /// 当前 model(SetModel 可变,Mutex 保护)。每次 lock+clone,调用不频繁。
    fn model_string(&self) -> String {
        self.model.lock().unwrap().clone()
    }
}

/// Wire 审批器:在 `pending_approvals` 注册 oneshot 等 `Op::ApprovalResponse`。
///
/// 注意:`TurnEvent::ApprovalRequest` 由 run_turn 在调用本方法**之前**经
/// on_event 发出(见 session.rs Ask 分支),故此处只注册 oneshot + 等待,
/// 不重复 emit——否则会双发。
struct WireApprover {
    shared: Arc<Shared>,
}

#[async_trait]
impl Approver for WireApprover {
    async fn request(&self, req: ApprovalRequest) -> ReviewDecision {
        let key = req.call_id.to_string();
        let (tx, rx) = oneshot::channel();
        self.shared.pending_approvals.lock().await.insert(key, tx);
        rx.await.unwrap_or(ReviewDecision::Deny)
    }
}

/// 客户端句柄:submit Op + 订阅事件流。Clone 给每个 attach 的连接。
#[derive(Clone)]
pub struct WireSessionHandle {
    op_tx: mpsc::Sender<Submission>,
    event_tx: broadcast::Sender<Event>,
}

impl WireSessionHandle {
    pub async fn submit(&self, sub: Submission) {
        let _ = self.op_tx.send(sub).await;
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.event_tx.subscribe()
    }
}

/// spawn 一个 wire session:解析 config + provider + 工具 + recorder,启动 actor。
pub async fn spawn(config: Config) -> anyhow::Result<(WireSessionHandle, SessionId)> {
    let cwd = std::env::current_dir()?;
    let config = Arc::new(config);
    let (client, model) =
        resolve(&config).map_err(|e| anyhow::anyhow!("provider 解析失败: {e}"))?;

    let mut tools = ToolRegistry::builtin();
    tao_mcp::load_mcp_tools(
        &mut tools,
        &config.mcp_servers,
        config.mcp_tool_budget,
        config.mcp_lazy,
    )
    .await;
    let tools = Arc::new(tools);

    let engine = Arc::new(PermissionEngine::new(
        config.permission_mode,
        config.permissions.rules.clone(),
    ));

    let (recorder, session_id) = JsonlRecorder::create(&cwd, config.config_fingerprint(&model))
        .map_err(|e| anyhow::anyhow!("会话创建失败: {e}"))?;
    let recorder = Arc::new(recorder);

    let mut system: Vec<SystemBlock> = Vec::new();
    if let Some(instr) = tao_core::instructions::load(&cwd) {
        system.push(SystemBlock {
            text: instr,
            cache_breakpoint: None,
        });
    }
    if let Some(p) = tao_core::skills::skills_prompt(&tao_core::skills::load_skills(&cwd)) {
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

    let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
    let (op_tx, op_rx) = mpsc::channel(OP_CHANNEL_CAPACITY);

    let shared = Arc::new(Shared {
        cwd,
        model: std::sync::Mutex::new(model),
        client,
        tools,
        engine,
        recorder: std::sync::Mutex::new(recorder),
        messages: Mutex::new(Vec::new()),
        system,
        config,
        session_id: std::sync::Mutex::new(session_id.clone()),
        seq: AtomicU64::new(0),
        event_tx: event_tx.clone(),
        pending_approvals: Mutex::new(HashMap::new()),
    });

    let handle = WireSessionHandle { op_tx, event_tx };

    tokio::spawn(run_actor(shared, op_rx));

    Ok((handle, session_id))
}

/// actor 主循环:分发 Op;turn 完成后出队下一个。
async fn run_actor(shared: Arc<Shared>, mut op_rx: mpsc::Receiver<Submission>) {
    let mut current: Option<CancellationToken> = None;
    let mut queue: VecDeque<Submission> = VecDeque::new();
    let (turn_done_tx, mut turn_done_rx) = mpsc::channel::<()>(1);

    loop {
        tokio::select! {
            sub = op_rx.recv() => match sub {
                None => break,
                Some(sub) => {
                    if matches!(sub.op, Op::Shutdown) {
                        break;
                    }
                    dispatch_op(&shared, &mut current, &mut queue, &turn_done_tx, sub).await;
                }
            },
            // 仅当有活跃 turn 时才等完成信号(否则 turn_done_rx 已关闭会空转)
            _ = turn_done_rx.recv(), if current.is_some() => {
                current = None;
                if let Some(next) = queue.pop_front() {
                    let cancel = CancellationToken::new();
                    current = Some(cancel.clone());
                    start_turn(shared.clone(), next, cancel, turn_done_tx.clone());
                }
            }
        }
    }
}

/// 分发单个 Op。turn 进行中到达的 UserTurn 入队(不阻塞审批/查询)。
async fn dispatch_op(
    shared: &Arc<Shared>,
    current: &mut Option<CancellationToken>,
    queue: &mut VecDeque<Submission>,
    turn_done_tx: &mpsc::Sender<()>,
    sub: Submission,
) {
    let sub_id = sub.id.clone();
    match sub.op {
        Op::Hello { .. } => {
            shared.emit(
                &sub_id,
                None,
                EventMsg::SessionConfigured {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: shared.session_id.lock().unwrap().clone(),
                    model: shared.model_string(),
                    permission_mode: shared.engine.mode(),
                    cwd: shared.cwd.clone(),
                },
            );
        }
        Op::UserTurn { .. } => {
            if current.is_some() {
                queue.push_back(sub);
            } else {
                let cancel = CancellationToken::new();
                *current = Some(cancel.clone());
                start_turn(shared.clone(), sub, cancel, turn_done_tx.clone());
            }
        }
        Op::Interrupt { abandon_queued } => {
            if let Some(c) = current {
                c.cancel();
            }
            if abandon_queued {
                queue.clear();
            }
        }
        Op::ApprovalResponse { call_id, decision } => {
            let key = call_id.to_string();
            let mut p = shared.pending_approvals.lock().await;
            if let Some(tx) = p.remove(&key) {
                let _ = tx.send(decision);
            }
        }
        Op::SetPermissionMode { mode } => {
            shared.engine.set_mode(mode);
            shared.emit(&sub_id, None, EventMsg::PermissionModeChanged { mode });
        }
        Op::ListSessions => {
            let sessions = list_sessions(&shared.cwd);
            shared.emit(&sub_id, None, EventMsg::SessionQuerySessions { sessions });
        }
        Op::GetHistory { after_seq, limit } => {
            // 从 recorder 日志重放 LogLine→Event(含 seq)。
            let path = shared.recorder.lock().unwrap().path();
            let events = replay_history_events(&path, &sub_id, after_seq, limit);
            shared.emit(
                &sub_id,
                None,
                EventMsg::SessionQueryHistory { events, done: true },
            );
        }
        Op::Compact { .. } => {
            if current.is_some() {
                shared.emit(
                    &sub_id,
                    None,
                    EventMsg::Error {
                        message: "turn 进行中,无法 compact".into(),
                        retryable: true,
                    },
                );
            } else {
                let messages = shared.messages.lock().await.clone();
                let cm = shared
                    .config
                    .small_model
                    .as_deref()
                    .unwrap_or(shared.model_string().as_str())
                    .to_string();
                let recorder = shared.recorder.lock().unwrap().clone();
                match tao_core::compact(
                    shared.client.as_ref(),
                    &cm,
                    &messages,
                    tao_core::DEFAULT_KEEP_LAST,
                    recorder.as_ref(),
                )
                .await
                {
                    Ok(new) => {
                        let n = new.len();
                        *shared.messages.lock().await = new;
                        shared.emit(
                            &sub_id,
                            None,
                            EventMsg::BackgroundEvent {
                                message: format!("compacted → {n} messages"),
                            },
                        );
                    }
                    Err(e) => {
                        shared.emit(
                            &sub_id,
                            None,
                            EventMsg::Error {
                                message: format!("compact 失败: {e}"),
                                retryable: false,
                            },
                        );
                    }
                }
            }
        }
        Op::ListMcpTools => {
            let tools: Vec<tao_protocol::McpToolInfo> = shared
                .tools
                .specs()
                .iter()
                .filter_map(|s| {
                    let parts: Vec<&str> = s.name.splitn(3, "__").collect();
                    (parts.len() == 3 && parts[0] == "mcp").then(|| tao_protocol::McpToolInfo {
                        server: parts[1].to_string(),
                        name: parts[2].to_string(),
                        description: s.description.clone(),
                    })
                })
                .collect();
            shared.emit(
                &sub_id,
                None,
                EventMsg::SessionQueryMcpTools {
                    tools,
                    health: vec![],
                },
            );
        }
        Op::ListModels => {
            let models = vec![tao_protocol::ModelInfo {
                id: shared.model_string(),
                provider: shared.config.current_provider_id().unwrap_or_default(),
                display: shared.model_string(),
                context_window: shared.client.context_window(shared.model_string().as_str()),
                supports_thinking: false,
                supports_images: false,
            }];
            shared.emit(&sub_id, None, EventMsg::SessionQueryModels { models });
        }
        Op::SetModel { model } => {
            *shared.model.lock().unwrap() = model.clone();
            shared.emit(
                &sub_id,
                None,
                EventMsg::BackgroundEvent {
                    message: format!("model → {model}"),
                },
            );
        }
        Op::AddPermissionRule { rule, scope: _ } => {
            // Session scope:加到 engine 运行时规则。Project/User 持久化写 config 留 TODO。
            shared.engine.add_rule(rule);
            shared.emit(
                &sub_id,
                None,
                EventMsg::BackgroundEvent {
                    message: "permission rule added (session scope)".into(),
                },
            );
        }
        Op::CheckpointRollback { checkpoint_id } => {
            // 找该 checkpoint 的 shadow_commit + seq,回滚文件 + 截断对话
            let recorder = shared.recorder.lock().unwrap().clone();
            match find_checkpoint_commit(&recorder.path(), &checkpoint_id) {
                Some((commit, cp_seq)) => {
                    let shadow = tao_core::ShadowRepo::init(&shared.cwd).ok();
                    match shadow {
                        Some(s) => match s.rollback(&commit) {
                            Ok(_) => {
                                // 截断 recorder 日志(只保留 <= cp_seq 的行)
                                if let Err(e) = recorder.truncate_to_seq(cp_seq) {
                                    tracing::warn!("recorder 截断失败(非致命): {e}");
                                }
                                // replay 截断后的日志 → 更新内存 messages
                                let path = recorder.path();
                                match tao_core::replay::replay(&path) {
                                    Ok(state) => {
                                        *shared.messages.lock().await = state.messages;
                                    }
                                    Err(e) => {
                                        tracing::warn!("replay 截断后失败: {e}");
                                    }
                                }
                                shared.emit(
                                    &sub_id,
                                    None,
                                    EventMsg::BackgroundEvent {
                                        message: format!(
                                            "rolled back files + conversation to checkpoint {checkpoint_id}"
                                        ),
                                    },
                                );
                            }
                            Err(e) => shared.emit(
                                &sub_id,
                                None,
                                EventMsg::Error {
                                    message: format!("rollback 失败: {e}"),
                                    retryable: false,
                                },
                            ),
                        },
                        None => shared.emit(
                            &sub_id,
                            None,
                            EventMsg::Error {
                                message: "shadow repo 不可用".into(),
                                retryable: false,
                            },
                        ),
                    }
                }
                None => shared.emit(
                    &sub_id,
                    None,
                    EventMsg::Error {
                        message: format!("checkpoint {checkpoint_id} 未找到"),
                        retryable: false,
                    },
                ),
            }
        }
        Op::ResumeSession { session_id, fork } => {
            // 切 recorder:replay 旧会话 → messages + 新 recorder(append 或 fork)
            let path = match tao_core::recorder::session_file_path(&shared.cwd, &session_id) {
                Some(p) => p,
                None => {
                    shared.emit(
                        &sub_id,
                        None,
                        EventMsg::Error {
                            message: "HOME 未设置,无法定位会话日志".into(),
                            retryable: false,
                        },
                    );
                    return;
                }
            };
            match tao_core::replay::replay(&path) {
                Ok(state) => {
                    let fp = shared.config.config_fingerprint(&shared.model_string());
                    // fork:新会话;否则 append 续写原会话
                    let (new_recorder, new_session_id) = if fork {
                        match JsonlRecorder::create_fork(&shared.cwd, &session_id, fp) {
                            Ok(r) => r,
                            Err(e) => {
                                shared.emit(
                                    &sub_id,
                                    None,
                                    EventMsg::Error {
                                        message: format!("创建 fork 会话失败: {e}"),
                                        retryable: false,
                                    },
                                );
                                return;
                            }
                        }
                    } else {
                        match JsonlRecorder::open_existing(&session_id, &shared.cwd, fp) {
                            Ok(r) => (r, session_id.clone()),
                            Err(e) => {
                                shared.emit(
                                    &sub_id,
                                    None,
                                    EventMsg::Error {
                                        message: format!("打开会话失败: {e}"),
                                        retryable: false,
                                    },
                                );
                                return;
                            }
                        }
                    };
                    // 运行时切 recorder + session_id
                    *shared.recorder.lock().unwrap() = Arc::new(new_recorder);
                    *shared.session_id.lock().unwrap() = new_session_id.clone();
                    *shared.messages.lock().await = state.messages;
                    shared.emit(
                        &sub_id,
                        None,
                        EventMsg::SessionConfigured {
                            protocol_version: PROTOCOL_VERSION,
                            session_id: new_session_id,
                            model: shared.model_string(),
                            permission_mode: shared.engine.mode(),
                            cwd: shared.cwd.clone(),
                        },
                    );
                }
                Err(e) => {
                    shared.emit(
                        &sub_id,
                        None,
                        EventMsg::Error {
                            message: format!("重放会话失败: {e}"),
                            retryable: false,
                        },
                    );
                }
            }
        }
        Op::ResumeEvents { after_seq } => {
            // 复用 GetHistory replay 逻辑,按 after_seq 过滤(只返回 seq > after_seq)。
            // limit=0 表示无上限。
            let path = shared.recorder.lock().unwrap().path();
            let events = replay_history_events(&path, &sub_id, after_seq, 0);
            shared.emit(
                &sub_id,
                None,
                EventMsg::SessionQueryHistory { events, done: true },
            );
        }
        other => {
            shared.emit(
                &sub_id,
                None,
                EventMsg::Error {
                    message: format!("Op 暂未实现: {other:?}"),
                    retryable: false,
                },
            );
        }
    }
}

/// 启动一个 turn task:record + (auto-compact) + run_turn + 存回 messages + 通知 actor。
fn start_turn(
    shared: Arc<Shared>,
    sub: Submission,
    cancel: CancellationToken,
    turn_done_tx: mpsc::Sender<()>,
) {
    let Op::UserTurn { turn_id, input } = sub.op else {
        return;
    };
    let turn_id = TurnId::new(turn_id);
    let sub_id = sub.id;

    tokio::spawn(async move {
        let recorder = shared.recorder.lock().unwrap().clone();

        let text: String = input
            .iter()
            .filter_map(|i| match i {
                UserInput::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");

        recorder.record(tao_protocol::log::LogEvent::UserInput {
            content: vec![Content::text(&text)],
            turn_id: turn_id.clone(),
        });

        // 取出 messages 的 owned 副本(避免跨 await 持锁)
        let mut messages = shared.messages.lock().await.clone();
        messages.push(ModelMessage::User {
            content: vec![ModelContent::text(&text)],
        });

        // auto compact
        let threshold = (shared.client.context_window(shared.model_string().as_str()) as f32
            * shared.config.auto_compact_at) as u64;
        if tao_core::approx_tokens(&messages) > threshold {
            let model_str = shared.model_string();
            let cm = shared
                .config
                .small_model
                .as_deref()
                .unwrap_or(model_str.as_str());
            if let Ok(compacted) = tao_core::compact(
                shared.client.as_ref(),
                cm,
                &messages,
                tao_core::DEFAULT_KEEP_LAST,
                recorder.as_ref(),
            )
            .await
            {
                messages = compacted;
            }
        }

        let req = ModelRequest {
            model: shared.model_string(),
            system: shared.system.clone(),
            messages: vec![],
            tools: shared.tools.specs(),
            reasoning: shared.config.reasoning_effort,
            max_output_tokens: 4096,
            temperature: None,
            metadata: RequestMeta {
                session_id: Some(shared.session_id.lock().unwrap().to_string()),
                turn_id: Some(turn_id.to_string()),
            },
        };
        let config_turn = TurnConfig {
            max_steps: shared.config.max_turn_steps,
            trusted_projects: shared.config.trusted_projects.clone(),
        };
        let shadow = tao_core::ShadowRepo::init(&shared.cwd).ok();
        let approver = WireApprover {
            shared: shared.clone(),
        };

        let shared_evt = shared.clone();
        let sub_id_evt = sub_id.clone();
        let turn_id_evt = turn_id.clone();
        let result = run_turn(
            shared.client.as_ref(),
            &shared.tools,
            shared.engine.as_ref(),
            &approver,
            recorder.as_ref(),
            &shared.config.hooks,
            shadow.as_ref(),
            &req,
            &mut messages,
            &config_turn,
            &shared.cwd,
            &cancel,
            move |ev| {
                if let Some(msg) = turn_event_to_msg(&ev, &turn_id_evt) {
                    shared_evt.emit(&sub_id_evt, Some(turn_id_evt.clone()), msg);
                }
            },
        )
        .await;

        // 存回 messages
        *shared.messages.lock().await = messages;

        let complete_turn_id = turn_id.clone();
        match result {
            Ok(r) => shared.emit(
                &sub_id,
                Some(complete_turn_id.clone()),
                EventMsg::TurnComplete {
                    turn_id: complete_turn_id.clone(),
                    usage: r.usage,
                    stop_reason: r.stop_reason,
                },
            ),
            Err(e) => shared.emit(
                &sub_id,
                Some(complete_turn_id.clone()),
                EventMsg::Error {
                    message: e.to_string(),
                    retryable: false,
                },
            ),
        }
        let _ = turn_done_tx.send(()).await;
    });
}

/// TurnEvent → EventMsg 翻译(exec 的 run_json 为参考)。
fn turn_event_to_msg(ev: &TurnEvent, _turn_id: &TurnId) -> Option<EventMsg> {
    match ev {
        TurnEvent::TextDelta(t) => Some(EventMsg::AgentMessageDelta { text: t.clone() }),
        TurnEvent::ThinkingDelta(t) => Some(EventMsg::ReasoningDelta { text: t.clone() }),
        TurnEvent::ToolCallBegin { call_id, tool } => Some(EventMsg::ToolCallBegin {
            call_id: call_id.clone(),
            tool: tool.clone(),
            summary: String::new(),
        }),
        // ToolCallEnd(参数完整)不发:等 ToolExecEnd 发带 summary 的 ToolCallEnd,避免双发
        TurnEvent::ToolCallEnd { .. } => None,
        // ToolExecBegin 不发:ToolCallBegin 已标记调用开始,避免双发
        TurnEvent::ToolExecBegin { .. } => None,
        TurnEvent::ToolExecEnd {
            call_id,
            ok,
            summary,
        } => Some(EventMsg::ToolCallEnd {
            call_id: call_id.clone(),
            ok: *ok,
            summary: summary.clone(),
        }),
        TurnEvent::ModelMessageEnd { .. } => None,
        TurnEvent::Usage(u) => Some(EventMsg::TokenCount {
            used: u.total(),
            // TODO:从 shared.client.context_window(model) 取真实 window;
            // turn_event_to_msg 无 shared 句柄,暂用 0(v1 简化)。
            window: 0,
        }),
        TurnEvent::TurnComplete { .. } => None, // 由 start_turn 在 run_turn 返回后发
        TurnEvent::ApprovalRequest {
            call_id,
            kind,
            detail,
        } => Some(EventMsg::ApprovalRequest {
            call_id: call_id.clone(),
            kind: *kind,
            detail: detail.clone(),
        }),
        TurnEvent::ApprovalResolved { .. } => None,
        TurnEvent::BackgroundEvent(msg) => Some(EventMsg::BackgroundEvent {
            message: msg.clone(),
        }),
        TurnEvent::Error(msg) => Some(EventMsg::Error {
            message: msg.clone(),
            retryable: false,
        }),
    }
}

/// 从 recorder 日志重放 LogLine→Event(含 seq),供 GetHistory / ResumeEvents 共用。
/// 映射:AssistantMessage → EventMsg::AgentMessage{text}(Text content join);
/// 其它(UserInput/ToolResult/...)跳过。`after_seq > 0` 时跳过 seq <= after_seq。
/// `limit=0` 视为无上限。Event{id=sub_id, seq=log_seq, turn=None, msg}。
///
/// 注意:仅读 `recorder.path()`(当前段);跨 rotate 段的历史需扫所有段(留 TODO)。
fn replay_history_events(path: &Path, sub_id: &str, after_seq: u64, limit: u32) -> Vec<Event> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut events: Vec<Event> = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(ll) = serde_json::from_str::<tao_protocol::log::LogLine>(line) else {
            continue;
        };
        if after_seq > 0 && ll.seq <= after_seq {
            continue;
        }
        if let tao_protocol::log::LogEvent::AssistantMessage { content, .. } = &ll.event {
            let text: String = content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            if !text.is_empty() {
                events.push(Event {
                    id: sub_id.to_string(),
                    seq: ll.seq,
                    turn: None,
                    msg: EventMsg::AgentMessage { text },
                });
            }
        }
        if limit > 0 && events.len() >= limit as usize {
            break;
        }
    }
    events
}

/// 在会话日志中找 `checkpoint_id` 对应的 shadow_commit hash + seq。
///
/// 注意:仅读传入的单个 path(当前段);跨 rotate 段的 checkpoint 需扫所有段(留 TODO)。
fn find_checkpoint_commit(
    path: &Path,
    checkpoint_id: &tao_protocol::ids::CheckpointId,
) -> Option<(String, u64)> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let Ok(ll) = serde_json::from_str::<tao_protocol::log::LogLine>(line) else {
            continue;
        };
        if let tao_protocol::log::LogEvent::Checkpoint {
            checkpoint_id: cid,
            shadow_commit,
        } = &ll.event
            && cid == checkpoint_id
        {
            return Some((shadow_commit.clone(), ll.seq));
        }
    }
    None
}

/// 扫描会话目录,replay 每个 JSONL → SessionSummary(按 updated_at 倒序)。
fn list_sessions(cwd: &Path) -> Vec<SessionSummary> {
    let Some(dir) = session_dir(cwd) else {
        return vec![];
    };
    if !dir.exists() {
        return vec![];
    }
    let mut out: Vec<SessionSummary> = Vec::new();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = e.path();
        if !path.extension().is_some_and(|x| x == "jsonl") {
            continue;
        }
        let meta = e.metadata().ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if let Ok(state) = replay(&path) {
            out.push(SessionSummary {
                id: state.id,
                parent: state.parent,
                title: state.title,
                cwd: state.cwd,
                updated_at_ms: mtime,
                size_bytes: size,
            });
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_at_ms));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_request_translates() {
        let ev = TurnEvent::ApprovalRequest {
            call_id: tao_protocol::ids::CallId::new("c1"),
            kind: tao_protocol::event::ApprovalKind::Tool,
            detail: tao_protocol::event::ApprovalDetail {
                rule_matched: None,
                command: None,
                files: None,
                tool: Some("Bash".into()),
                args_summary: None,
                pattern_suggestion: None,
            },
        };
        let msg = turn_event_to_msg(&ev, &TurnId::new("t1"));
        assert!(matches!(msg, Some(EventMsg::ApprovalRequest { .. })));
    }

    #[test]
    fn text_delta_translates() {
        let ev = TurnEvent::TextDelta("hi".into());
        let msg = turn_event_to_msg(&ev, &TurnId::new("t1"));
        assert!(matches!(msg, Some(EventMsg::AgentMessageDelta { .. })));
    }

    #[test]
    fn approval_resolved_skipped() {
        let ev = TurnEvent::ApprovalResolved {
            call_id: tao_protocol::ids::CallId::new("c1"),
            decision: ReviewDecision::Approve,
        };
        assert!(turn_event_to_msg(&ev, &TurnId::new("t1")).is_none());
    }
}
