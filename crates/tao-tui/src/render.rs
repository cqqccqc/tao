//! TUI 渲染:inline viewport + 输入框 + 流式文本 + 工具状态。
//! 见 docs/design/tui.md §3。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SyntectStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

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
    /// L4b viewport 滚动偏移:N = 从底部往上 N 行(0 = 跟随底部/最新)。
    pub scroll_offset: usize,
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
                push_assistant_lines(&mut lines, t);
            }
            HistoryCell::Tool(s) => {
                for l in diff_lines(s) {
                    lines.push(l);
                }
            }
        }
    }

    // live text(进行中的 assistant 消息,流式 markdown)
    if !state.live_text.is_empty() {
        push_assistant_lines(&mut lines, state.live_text);
    }

    if state.history.is_empty() && state.live_text.is_empty() {
        lines.push(Line::from(Span::styled(
            "tao 已就绪。输入任务后按 Enter 发送。Ctrl+C/Ctrl+D 退出。",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // L4b viewport:auto-follow-to-bottom。scroll_offset = 从底部往上 N 行
    // (0 = 跟随底部/最新)。line_count 在 ratatui 0.29 是 unstable/private,
    // 用 lines.len() 近似(不含 wrap 行,滚动不精确但可用)。
    let viewport = chunks[0].height as usize;
    let total = lines.len();
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max_scroll = total.saturating_sub(viewport);
    let scroll_y = max_scroll
        .saturating_sub(state.scroll_offset)
        .min(u16::MAX as usize) as u16;
    f.render_widget(para.scroll((scroll_y, 0)), chunks[0]);

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

// ---- markdown / diff 渲染 ----

/// assistant 文本:第一行带 "tao " 前缀,后续 markdown 渲染行。
fn push_assistant_lines(lines: &mut Vec<Line>, text: &str) {
    let prefix = Span::styled(
        "tao ",
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    );
    let md = markdown_to_lines(text);
    match md.first() {
        Some(first) => {
            let mut spans = vec![prefix];
            spans.extend(first.spans.iter().cloned());
            lines.push(Line::from(spans));
        }
        None => lines.push(Line::from(prefix)),
    }
    for l in md.iter().skip(1) {
        lines.push(l.clone());
    }
}

// ---- L4d markdown 行缓存 ----

/// markdown 渲染结果 LRU 缓存(限 64,超则全清)。key = text hash。
static MD_CACHE: OnceLock<Mutex<HashMap<u64, Vec<Line<'static>>>>> = OnceLock::new();
const MD_CACHE_CAP: usize = 64;

fn md_cache() -> &'static Mutex<HashMap<u64, Vec<Line<'static>>>> {
    MD_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn text_hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// markdown → ratatui Lines(标题/段落/列表/代码块/行内代码/强调/Rule)。
///
/// L4d 行缓存:LRU-ish(HashMap + 限 64,超则全清)。key = text hash。
/// live_text 流式每次 text 不同,缓存命中低但 LRU 上限防爆破。
fn markdown_to_lines(text: &str) -> Vec<Line<'static>> {
    let key = text_hash(text);
    if let Some(hit) = md_cache().lock().unwrap().get(&key).cloned() {
        return hit;
    }
    let lines = parse_markdown(text);
    let mut cache = md_cache().lock().unwrap();
    if cache.len() >= MD_CACHE_CAP {
        cache.clear();
    }
    cache.insert(key, lines.clone());
    lines
}

/// markdown → ratatui Lines 实际解析(无缓存)。
fn parse_markdown(text: &str) -> Vec<Line<'static>> {
    use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
    let parser = Parser::new(text);
    let mut lines: Vec<Line> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut style = Style::default();
    let mut in_code = false;
    let mut code_lang: String = String::new();
    let mut code_buf: String = String::new();
    let mut list_depth: usize = 0;

    for event in parser {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                push_line(&mut spans, &mut lines);
                style = Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Cyan);
            }
            Event::End(TagEnd::Heading(_)) => {
                push_line(&mut spans, &mut lines);
                style = Style::default();
            }
            Event::Start(Tag::Paragraph) | Event::End(TagEnd::Paragraph) => {
                push_line(&mut spans, &mut lines);
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                // Flush any pending inline spans, then start buffering code text.
                push_line(&mut spans, &mut lines);
                in_code = true;
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.into_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code_buf.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                // Spans are empty during a code block (text went to code_buf),
                // but flush defensively in case of malformed input.
                push_line(&mut spans, &mut lines);
                in_code = false;
                lines.extend(highlight_code_block(&code_buf, &code_lang));
                code_buf.clear();
                style = Style::default();
            }
            Event::Start(Tag::List(_)) => list_depth += 1,
            Event::End(TagEnd::List(_)) => list_depth = list_depth.saturating_sub(1),
            Event::Start(Tag::Item) => {
                push_line(&mut spans, &mut lines);
                spans.push(Span::raw(format!(
                    "{}• ",
                    "  ".repeat(list_depth.saturating_sub(1))
                )));
            }
            Event::End(TagEnd::Item) => push_line(&mut spans, &mut lines),
            Event::Start(Tag::Emphasis) => style = style.add_modifier(Modifier::ITALIC),
            Event::End(TagEnd::Emphasis) => style = style.remove_modifier(Modifier::ITALIC),
            Event::Start(Tag::Strong) => style = style.add_modifier(Modifier::BOLD),
            Event::End(TagEnd::Strong) => style = style.remove_modifier(Modifier::BOLD),
            Event::Text(t) => {
                if in_code {
                    code_buf.push_str(&t);
                } else {
                    spans.push(Span::styled(t.to_string(), style));
                }
            }
            Event::Code(c) => spans.push(Span::styled(
                c.to_string(),
                Style::default().fg(Color::Yellow),
            )),
            Event::SoftBreak | Event::HardBreak => push_line(&mut spans, &mut lines),
            Event::Rule => {
                push_line(&mut spans, &mut lines);
                lines.push(Line::raw("─"));
            }
            _ => {}
        }
    }
    push_line(&mut spans, &mut lines);
    lines
}

// ---- syntect 代码块语法高亮 ----

/// 内置常用语言的 SyntaxSet,加载一次后缓存(newline-aware 模式)。
static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
/// 默认 ThemeSet,加载一次后缓存。
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

/// 从默认 ThemeSet 中选一个深色主题。优先 base16-ocean.dark,其次几个备选,
/// 最后 fallback 到任意一个(ThemeSet 至少包含一个主题,否则 panic)。
#[allow(clippy::explicit_auto_deref)]
fn default_theme() -> &'static Theme {
    static THEME: OnceLock<&'static Theme> = OnceLock::new();
    *THEME.get_or_init(|| {
        let ts = theme_set();
        [
            "base16-ocean.dark",
            "base16-eighties.dark",
            "base16-mocha.dark",
        ]
        .iter()
        .find_map(|name| ts.themes.get(*name))
        .or_else(|| ts.themes.values().next())
        .expect("syntect default ThemeSet must contain at least one theme")
    })
}

/// syntect Style → ratatui Style(前景色 + 字体修饰)。
fn syntect_style_to_ratatui(s: SyntectStyle) -> Style {
    let fg = Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b);
    let mut style = Style::default().fg(fg);
    let fs = s.font_style;
    if fs.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if fs.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if fs.contains(FontStyle::UNDERLINE) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}

/// 按 fenced lang 用 syntect 高亮代码块,每行一个 ratatui `Line`(2 空格缩进)。
///
/// 无 lang / 未识别语言时 fallback 为原来的 Gray 缩进渲染。
fn highlight_code_block(code: &str, lang: &str) -> Vec<Line<'static>> {
    let ss = syntax_set();
    let theme = default_theme();
    let syntax = if !lang.is_empty() {
        ss.find_syntax_by_token(lang)
            .or_else(|| ss.find_syntax_by_extension(lang))
            .or_else(|| ss.find_syntax_by_name(lang))
    } else {
        None
    };

    let Some(syntax) = syntax else {
        // Fallback:保持原有行为 —— Gray 着色 + 2 空格缩进。
        return code
            .lines()
            .map(|l| {
                Line::from(vec![Span::styled(
                    format!("  {l}"),
                    Style::default().fg(Color::Gray),
                )])
            })
            .collect();
    };

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut result: Vec<Line> = Vec::new();
    // newline-aware:给每行补一个 '\n' 再喂给 highlighter,这样多行构造
    // (块注释等)在 load_defaults_newlines() 下能正确解析。
    for line in code.lines() {
        let with_nl = format!("{line}\n");
        let ranges = highlighter.highlight_line(&with_nl, ss).unwrap_or_default();
        let mut spans: Vec<Span> = Vec::with_capacity(ranges.len() + 1);
        spans.push(Span::raw("  ")); // 2 空格缩进,与原渲染一致
        for (st, text) in ranges {
            // 去掉可能粘在 span 末尾的换行符(ratatui Span 不应含 '\n')
            let text = text.strip_suffix('\n').unwrap_or(text);
            if !text.is_empty() {
                spans.push(Span::styled(text.to_string(), syntect_style_to_ratatui(st)));
            }
        }
        result.push(Line::from(spans));
    }
    result
}

/// diff/工具输出:行首 +/-/@@/---/+++ 着色。
fn diff_lines(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|l| {
            let style = if l.starts_with('+') {
                Style::default().fg(Color::Green)
            } else if l.starts_with('-') {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(vec![Span::styled(format!("  {l}"), style)])
        })
        .collect()
}

fn push_line<'a>(spans: &mut Vec<Span<'a>>, lines: &mut Vec<Line<'a>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_heading_and_text() {
        let lines = markdown_to_lines("# 标题\n正文");
        assert!(lines.len() >= 2);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(joined.contains("标题"));
        assert!(joined.contains("正文"));
    }

    #[test]
    fn markdown_code_block() {
        let lines = markdown_to_lines("```\nfn main() {}\n```");
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(joined.contains("fn main() {}"));
    }

    #[test]
    fn markdown_inline_code() {
        let lines = markdown_to_lines("用 `cargo` 构建");
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(joined.contains("cargo"));
    }

    #[test]
    fn diff_lines_coloring() {
        let lines = diff_lines("+added\n-removed\n ctx");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Red));
    }
}
