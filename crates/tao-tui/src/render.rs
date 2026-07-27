//! TUI 渲染:inline viewport + 输入框 + 流式文本 + 工具状态。
//! 见 docs/design/tui.md §3。

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

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

/// 渲染所需的只读视图。
pub struct RenderState<'a> {
    pub input: &'a str,
    pub history: &'a [HistoryCell],
    pub live_text: &'a str,
    pub tool_status: Option<&'a (String, bool)>,
    pub running: bool,
    pub error: Option<&'a str>,
    pub model: &'a str,
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
    let status_parts: Vec<Span> = vec![
        Span::styled(
            format!(" {} ", state.model),
            Style::default().fg(Color::Yellow),
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
}
