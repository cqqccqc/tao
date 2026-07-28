//! TUI 渲染:inline viewport + 输入框 + 流式文本 + 工具状态。
//! 见 docs/design/tui.md §3。

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tao_protocol::event::ApprovalKind;
use tao_protocol::ids::CallId;
use tao_protocol::permission::PermissionMode;

/// 历史单元格(append-only;只有 live cell 可变——M1 用 live_text 代替)。
#[derive(Debug, Clone)]
pub enum HistoryCell {
    User(String),
    Assistant(String),
    Tool(String),
}

impl HistoryCell {
    pub fn user(text: &str) -> Self {
        Self::User(text.to_owned())
    }
    pub fn assistant(text: String) -> Self {
        Self::Assistant(text)
    }
    pub fn tool(status: &str) -> Self {
        Self::Tool(status.to_owned())
    }
}

/// 待审批的调用(渲染弹窗用)。
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub call_id: CallId,
    pub kind: ApprovalKind,
    pub tool: String,
    pub command: Option<Vec<String>>,
    pub pattern_suggestion: Option<String>,
}

/// 渲染所需的只读视图。
pub struct RenderState<'a> {
    pub input: &'a str,
    pub history: &'a [HistoryCell],
    pub live_text: &'a str,
    pub tool_status: Option<&'a (String, bool)>,
    pub running: bool,
    pub error: Option<&'a str>,
    pub model: &'a str,
    pub mode: PermissionMode,
    pub pending_approval: Option<&'a PendingApproval>,
}

pub fn draw(f: &mut Frame, state: &RenderState) {
    let area = f.area();

    // 布局:历史区(flex) + 状态行 + 输入框
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(area);

    // ---- 历史区 ----
    let mut lines: Vec<Line> = Vec::new();
    for cell in state.history {
        match cell {
            HistoryCell::User(t) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "你 ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(t),
                ]));
            }
            HistoryCell::Assistant(t) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "tao ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(t),
                ]));
            }
            HistoryCell::Tool(s) => {
                lines.push(Line::from(vec![Span::styled(
                    format!("  {s}"),
                    Style::default().fg(Color::DarkGray),
                )]));
            }
        }
    }

    // live text(进行中的 assistant 消息)
    if !state.live_text.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                "tao ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(state.live_text),
        ]));
    }

    if state.history.is_empty() && state.live_text.is_empty() {
        lines.push(Line::from(Span::styled(
            "tao 已就绪。输入任务后按 Enter 发送。Ctrl+C/Ctrl+D 退出。",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let history = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(history, chunks[0]);

    // ---- 状态行 ----
    let mode_color = match state.mode {
        PermissionMode::Bypass => Color::Red,
        PermissionMode::Plan => Color::Magenta,
        PermissionMode::AcceptEdits => Color::Green,
        PermissionMode::Default => Color::Yellow,
    };
    let status_parts: Vec<Span> = vec![
        Span::styled(
            format!(" {} ", state.model),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(" │ "),
        Span::styled(
            format!(" {:?} ", state.mode),
            Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" │ "),
        if state.running {
            Span::styled("● working", Style::default().fg(Color::Blue))
        } else {
            Span::styled("○ idle", Style::default().fg(Color::DarkGray))
        },
        Span::raw(" │ "),
        if let Some((status, ok)) = state.tool_status {
            let color = if *ok { Color::Green } else { Color::Red };
            Span::styled(status.as_str(), Style::default().fg(color))
        } else {
            Span::raw("")
        },
    ];
    f.render_widget(Paragraph::new(Line::from(status_parts)), chunks[1]);

    // ---- 输入框 ----
    let input_block = Block::default().borders(Borders::ALL).title(Span::styled(
        if state.running {
            " (running, Esc 不可中断) "
        } else {
            " prompt "
        },
        Style::default().fg(Color::Cyan),
    ));
    let input_text: &str = state.input;
    let input_para = Paragraph::new(input_text)
        .block(input_block)
        .wrap(Wrap { trim: false });
    f.render_widget(input_para, chunks[2]);

    // 光标
    if !state.running {
        let cursor_y = chunks[2].y + 1;
        let cursor_x = chunks[2].x + 1 + state.input.chars().count() as u16;
        f.set_cursor_position((cursor_x, cursor_y));
    }

    // 错误行(覆盖在历史区底部)
    if let Some(err) = state.error {
        let err_line = Line::from(vec![Span::styled(
            format!(" [error] {err}"),
            Style::default().fg(Color::Red),
        )]);
        let err_area = ratatui::layout::Rect::new(
            chunks[0].x,
            chunks[0].bottom().saturating_sub(1),
            chunks[0].width,
            1,
        );
        f.render_widget(Paragraph::new(err_line), err_area);
    }

    // ---- 审批弹窗(覆盖最上层)----
    if let Some(p) = state.pending_approval {
        let area = centered_rect(64, 40, f.area());
        let mut lines: Vec<Line> = vec![Line::from(vec![
            Span::styled("工具: ", Style::default().fg(Color::DarkGray)),
            Span::raw(p.tool.as_str()),
        ])];
        if let Some(cmd) = &p.command {
            lines.push(Line::from(vec![
                Span::styled("命令: ", Style::default().fg(Color::DarkGray)),
                Span::raw(cmd.join(" ")),
            ]));
        }
        if let Some(sug) = &p.pattern_suggestion {
            lines.push(Line::from(vec![
                Span::styled("建议规则: ", Style::default().fg(Color::DarkGray)),
                Span::styled(sug.as_str(), Style::default().fg(Color::Cyan)),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(
                "y",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("=批准  "),
            Span::styled(
                "s",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("=本次+会话  "),
            Span::styled(
                "n",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("=拒绝  "),
            Span::styled(
                "a",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("=中止"),
        ]));
        let popup = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(
            Span::styled(
                format!(" 审批请求 {:?} ", p.kind),
                Style::default().fg(Color::Yellow),
            ),
        ));
        f.render_widget(Clear, area);
        f.render_widget(popup, area);
    }
}

/// 居中弹窗区域(百分比宽/高)。
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
