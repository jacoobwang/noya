use crate::tui::{
    app::{AgentState, App, Message, MessageKind},
    markdown,
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use textwrap::Options as WrapOptions;
use unicode_width::UnicodeWidthStr;

use std::sync::atomic::{AtomicUsize, Ordering};

pub const VIEWPORT_HEIGHT: u16 = 7;
const INPUT_PROMPT: &str = "> ";

#[derive(Debug, Clone, Copy)]
pub struct StreamView {
    pub committed_lines: usize,
    pub render_width: usize,
}

pub fn draw(frame: &mut Frame, app: &App, stream_view: Option<StreamView>) {
    let [stream_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    render_stream(frame, stream_area, app, stream_view);
    render_status(frame, status_area, app);
    render_input(frame, input_area, app);
}

pub fn message_lines(message: &Message, width: usize) -> Vec<Line<'static>> {
    let (label, label_style, content_style) = message_style(message.kind);
    let label_line = if message.kind == MessageKind::User {
        Line::from(vec![Span::styled(label, label_style), Span::raw("  ")])
    } else {
        Line::from(Span::styled(label, label_style))
    };
    let mut lines = vec![label_line];
    let available = width.max(4);
    if message.kind == MessageKind::Agent {
        let body_width = available.saturating_sub(2).max(1);
        for line in markdown::render(&message.content, body_width, content_style) {
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::styled("  ", content_style));
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    } else if message.kind == MessageKind::User {
        let options = WrapOptions::new(available.saturating_sub(2).max(1)).break_words(true);
        for raw in message.content.split('\n') {
            if raw.is_empty() {
                lines.push(Line::from("  "));
            } else {
                for wrapped in textwrap::wrap(raw, &options) {
                    lines.push(Line::from(vec![
                        Span::styled(wrapped.into_owned(), content_style),
                        Span::styled("  ", content_style),
                    ]));
                }
            }
        }
    } else {
        let options = WrapOptions::new(available)
            .initial_indent("  ")
            .subsequent_indent("  ")
            .break_words(true);
        for raw in message.content.split('\n') {
            if raw.is_empty() {
                lines.push(Line::from("  "));
            } else {
                for wrapped in textwrap::wrap(raw, &options) {
                    lines.push(Line::from(Span::styled(
                        wrapped.into_owned(),
                        content_style,
                    )));
                }
            }
        }
    }
    lines.push(Line::default());
    let alignment = if message.kind == MessageKind::User {
        Alignment::Right
    } else {
        Alignment::Left
    };
    for line in &mut lines {
        line.alignment = Some(alignment);
    }
    lines
}

pub fn render_transcript_buffer(lines: Vec<Line<'static>>, buffer: &mut Buffer) {
    Paragraph::new(lines).render(buffer.area, buffer);
    suppress_wide_character_continuations(buffer);
}

fn suppress_wide_character_continuations(buffer: &mut Buffer) {
    // Ratatui 0.29 `insert_before` prints every buffer cell instead of diffing it. A wide
    // grapheme's trailing placeholder must therefore be empty, or it becomes a visible space.
    for y in buffer.area.top()..buffer.area.bottom() {
        let mut x = buffer.area.left();
        while x < buffer.area.right() {
            let symbol_width = UnicodeWidthStr::width(buffer[(x, y)].symbol()).max(1);
            if symbol_width > 1 {
                for offset in 1..symbol_width {
                    let continuation_x =
                        x.saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
                    if continuation_x < buffer.area.right() {
                        buffer[(continuation_x, y)].set_symbol("");
                    }
                }
            }
            x = x.saturating_add(u16::try_from(symbol_width).unwrap_or(u16::MAX));
        }
    }
}

fn render_stream(frame: &mut Frame, area: Rect, app: &App, stream_view: Option<StreamView>) {
    if area.is_empty() {
        return;
    }
    let Some(message) = app.current_streaming_message() else {
        return;
    };
    let view = stream_view.unwrap_or(StreamView {
        committed_lines: 0,
        render_width: usize::from(area.width.max(1)),
    });
    let lines = message_lines(message, view.render_width);
    let final_separator = lines.len().saturating_sub(1);
    let start = view.committed_lines.min(final_separator);
    let visible = lines[start..final_separator].to_vec();
    if visible.is_empty() {
        return;
    }

    let height = u16::try_from(visible.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let visible = visible[visible.len().saturating_sub(usize::from(height))..].to_vec();
    let output_area = Rect::new(
        area.x,
        area.bottom().saturating_sub(height),
        area.width,
        height,
    );
    frame.render_widget(Paragraph::new(visible), output_area);
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let (mut label, color) = match app.agent_state {
        AgentState::Idle => ("Ready".to_string(), Color::Green),
        AgentState::Thinking => ("Thinking".to_string(), Color::Cyan),
        AgentState::Generating => ("Generating".to_string(), Color::Green),
        AgentState::RunningTool => ("Running tool".to_string(), Color::Yellow),
        AgentState::WaitingApproval => ("Approval required".to_string(), Color::Yellow),
        AgentState::Error => ("Error".to_string(), Color::Red),
    };
    if !matches!(app.agent_state, AgentState::Idle | AgentState::Error) {
        label = format!("{} {label}", spinner());
    }
    let mut spans = vec![Span::styled(
        label,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];
    if let Some(status) = &app.status_message {
        spans.push(Span::styled(
            format!(" | {status}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn spinner() -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    static FRAME: AtomicUsize = AtomicUsize::new(0);
    FRAMES[FRAME.fetch_add(1, Ordering::Relaxed) % FRAMES.len()]
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let prompt = Span::styled(
        INPUT_PROMPT,
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    );
    let prompt_width = UnicodeWidthStr::width(INPUT_PROMPT);
    let available = usize::from(inner.width).saturating_sub(prompt_width);
    let (visible, cursor_column) = visible_input(&app.input, app.cursor_position, available);
    let text = if visible.is_empty() {
        Span::styled(
            "Type a request. /help for commands.",
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw(visible)
    };
    frame.render_widget(Paragraph::new(Line::from(vec![prompt, text])), inner);

    let x = inner
        .x
        .saturating_add(u16::try_from(prompt_width + cursor_column).unwrap_or(u16::MAX))
        .min(inner.right().saturating_sub(1));
    frame.set_cursor_position((x, inner.y));
}

fn visible_input(input: &str, cursor_byte: usize, available: usize) -> (String, usize) {
    if available == 0 {
        return (String::new(), 0);
    }
    let cursor_byte = cursor_byte.min(input.len());
    let before = &input[..cursor_byte];
    let before_width = UnicodeWidthStr::width(before);
    if UnicodeWidthStr::width(input) <= available {
        return (input.to_string(), before_width);
    }

    let target_start = before_width.saturating_sub(available.saturating_sub(1));
    let mut display_column = 0;
    let mut start_byte = 0;
    for (byte, character) in input.char_indices() {
        if display_column >= target_start {
            start_byte = byte;
            break;
        }
        display_column += unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
    }
    let visible = input[start_byte..]
        .chars()
        .scan(0usize, |width, character| {
            let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
            if *width + character_width > available {
                None
            } else {
                *width += character_width;
                Some(character)
            }
        })
        .collect::<String>();
    (
        visible,
        UnicodeWidthStr::width(&input[start_byte..cursor_byte]),
    )
}

fn message_style(kind: MessageKind) -> (&'static str, Style, Style) {
    match kind {
        MessageKind::User => (
            "You",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Style::default(),
        ),
        MessageKind::Agent => (
            "Noya",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Style::default(),
        ),
        MessageKind::Tool => (
            "Tool",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Yellow),
        ),
        MessageKind::System => (
            "System",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Blue),
        ),
        MessageKind::Error => (
            "Error",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            Style::default().fg(Color::Red),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Message, MessageKind};
    use ratatui::layout::Alignment;

    #[test]
    fn conversation_messages_align_to_their_speaker_side() {
        let agent = message_lines(&Message::new(MessageKind::Agent, "model output"), 40);
        let user = message_lines(&Message::new(MessageKind::User, "user input"), 40);

        assert!(
            agent[..agent.len() - 1]
                .iter()
                .all(|line| line.alignment == Some(Alignment::Left))
        );
        assert!(
            user[..user.len() - 1]
                .iter()
                .all(|line| line.alignment == Some(Alignment::Right))
        );
    }

    #[test]
    fn message_lines_include_role_and_wrapped_content() {
        let message = Message::new(MessageKind::Agent, "你好，terminal world");

        let lines = message_lines(&message, 12);
        let rendered = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Noya"));
        assert!(rendered.contains("你好"));
        assert!(lines.len() >= 3);
    }

    #[test]
    fn chinese_transcript_buffer_does_not_print_wide_character_continuation_cells() {
        let message = Message::new(MessageKind::Agent, "中文输出非常不自然");
        let lines = message_lines(&message, 40);
        let height = u16::try_from(lines.len()).unwrap();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, height));

        render_transcript_buffer(lines, &mut buffer);

        assert_eq!(buffer[(2, 1)].symbol(), "中");
        assert_eq!(buffer[(3, 1)].symbol(), "");
        assert_eq!(buffer[(4, 1)].symbol(), "文");
        assert_eq!(buffer[(5, 1)].symbol(), "");
    }

    #[test]
    fn agent_transcript_renders_markdown_instead_of_showing_markers() {
        let message = Message::new(MessageKind::Agent, "## Result\n\nUse **cargo test** now.");

        let lines = message_lines(&message, 60);
        let rendered = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!rendered.contains("##"));
        assert!(!rendered.contains("**"));
        assert!(rendered.contains("Result"));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "cargo test" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }
}
