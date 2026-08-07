use super::theme::{
    ACCENT, ACCENT_SOFT, BG, BORDER_TYPE, DIM, ERROR, FG, INFO, MUTED, PANEL, SUCCESS, SURFACE,
    TOOL_PENDING_BG, USER_BG, WARNING,
};
use crate::tui::{
    app::{AgentState, App, AppInfo, AppMode, Message, MessageKind},
    markdown,
};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use textwrap::Options as WrapOptions;
use unicode_width::UnicodeWidthStr;

use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

pub const VIEWPORT_HEIGHT: u16 = 10;
const INPUT_PROMPT: &str = "> ";
const COMMAND_MENU_HEIGHT: u16 = 6;
const COMMAND_MENU_MAX_VISIBLE: usize = 5;
const TOOL_MESSAGE_MAX_LINES: usize = 3;
const WIDE_WELCOME_MIN_WIDTH: usize = 68;
const NOYA_LOGO: [&str; 5] = [
    "█   █  ███  █   █  ███",
    "██  █ █   █  █ █  █   █",
    "█ █ █ █   █   █   █████",
    "█  ██ █   █   █   █   █",
    "█   █  ███    █   █   █",
];

#[derive(Debug, Clone, Copy)]
pub struct StreamView {
    pub committed_lines: usize,
    pub render_width: usize,
}

pub fn draw(frame: &mut Frame, app: &App, stream_view: Option<StreamView>) {
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
    let command_menu_open = !app.command_suggestions().is_empty();
    let model_menu_open = app.mode == AppMode::SelectingModel;
    let menu_open = command_menu_open || model_menu_open;
    let [stream_area, status_area, input_area, command_menu_area] =
        tui_areas(frame.area(), menu_open);

    if !menu_open {
        render_stream(frame, stream_area, app, stream_view);
    }
    render_status(frame, status_area, app);
    render_input(frame, input_area, app);
    if model_menu_open {
        render_model_menu(frame, command_menu_area, app);
    } else if command_menu_open {
        render_command_menu(frame, command_menu_area, app);
    }
}

fn render_model_menu(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(
        Paragraph::new(model_menu_lines(app, usize::from(area.height)))
            .style(Style::default().bg(PANEL)),
        area,
    );
}

fn model_menu_lines(app: &App, height: usize) -> Vec<Line<'static>> {
    if app.model_choices.is_empty() || height == 0 {
        return Vec::new();
    }
    let selected = app.model_selection.min(app.model_choices.len() - 1);
    let visible_count = COMMAND_MENU_MAX_VISIBLE
        .min(height.saturating_sub(1).max(1))
        .min(app.model_choices.len());
    let start = selected
        .saturating_sub(visible_count - 1)
        .min(app.model_choices.len() - visible_count);
    let mut lines = Vec::with_capacity(visible_count + 1);
    for (index, choice) in app
        .model_choices
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_count)
    {
        let is_selected = index == selected;
        let selected_style = Style::default()
            .fg(ACCENT_SOFT)
            .add_modifier(Modifier::BOLD);
        let normal_style = Style::default().fg(FG);
        let mut line = Line::from(vec![
            Span::styled(
                if is_selected { "› " } else { "  " },
                if is_selected {
                    selected_style
                } else {
                    Style::default().fg(DIM)
                },
            ),
            Span::styled(
                format!("{:<12}", choice.model),
                if is_selected {
                    selected_style
                } else {
                    normal_style
                },
            ),
            Span::styled(
                format!("{:<26}", choice.model_id),
                if is_selected {
                    Style::default().fg(ACCENT_SOFT)
                } else {
                    Style::default().fg(DIM)
                },
            ),
            Span::styled(
                if choice.current { "✓ current" } else { "" },
                Style::default().fg(SUCCESS),
            ),
        ]);
        if is_selected {
            line.style = super::theme::selected_row();
        }
        lines.push(line);
    }
    if lines.len() < height {
        lines.push(Line::from(Span::styled(
            "  All models · unconfigured models will ask for base URL and API key · ↑/↓ select · Enter switch · Esc close",
            Style::default().fg(DIM),
        )));
    }
    lines
}

fn tui_areas(area: Rect, command_menu_open: bool) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(if command_menu_open {
            COMMAND_MENU_HEIGHT
        } else {
            0
        }),
    ])
    .areas(area)
}

fn render_command_menu(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let lines = command_menu_lines(app, usize::from(area.height));
    frame.render_widget(Paragraph::new(lines).style(Style::default().bg(PANEL)), area);
}

fn command_menu_lines(app: &App, height: usize) -> Vec<Line<'static>> {
    let suggestions = app.command_suggestions();
    if suggestions.is_empty() || height == 0 {
        return Vec::new();
    }
    let selected = app.command_selection.min(suggestions.len() - 1);
    let visible_count = COMMAND_MENU_MAX_VISIBLE
        .min(height.saturating_sub(1).max(1))
        .min(suggestions.len());
    let start = selected
        .saturating_sub(visible_count - 1)
        .min(suggestions.len() - visible_count);
    let mut lines = Vec::with_capacity(visible_count + 1);
    for (index, command) in suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_count)
    {
        let is_selected = index == selected;
        let marker_style = Style::default()
            .fg(if is_selected { ACCENT_SOFT } else { DIM })
            .add_modifier(if is_selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
        let text_style = if is_selected {
            Style::default().fg(ACCENT_SOFT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(FG)
        };
        let description_style = if is_selected {
            Style::default().fg(ACCENT_SOFT)
        } else {
            Style::default().fg(DIM)
        };
        let usage = command.argument.map_or_else(
            || command.name.to_string(),
            |argument| format!("{} {argument}", command.name),
        );
        let mut line = Line::from(vec![
            Span::styled(if is_selected { "› " } else { "  " }, marker_style),
            Span::styled(format!("{usage:<20}"), text_style),
            Span::styled(command.description, description_style),
        ]);
        if is_selected {
            line.style = super::theme::selected_row();
        }
        lines.push(line);
    }
    if lines.len() < height {
        lines.push(Line::from(Span::styled(
            format!(
                "  ({}/{})  ↑/↓ navigate · Enter select · Esc close",
                selected + 1,
                suggestions.len()
            ),
            Style::default().fg(DIM),
        )));
    }
    lines
}

pub fn welcome_lines(info: &AppInfo, width: usize) -> Vec<Line<'static>> {
    let version = env!("CARGO_PKG_VERSION");
    let model = display_model_name(&info.model);
    let workspace = display_workspace(&info.workspace);
    let metadata = welcome_metadata(version, model, &info.model_id, workspace);
    let logo_style = Style::default()
        .fg(ACCENT)
        .add_modifier(Modifier::BOLD);

    if width < WIDE_WELCOME_MIN_WIDTH {
        let mut lines = NOYA_LOGO
            .iter()
            .map(|logo| Line::from(Span::styled(format!("  {logo}"), logo_style)))
            .collect::<Vec<_>>();
        lines.push(Line::default());
        lines.extend(metadata.into_iter().map(Line::from));
        lines.push(Line::default());
        lines.push(welcome_prompt());
        lines.push(Line::default());
        return lines;
    }

    let mut lines = Vec::with_capacity(NOYA_LOGO.len() + 3);
    for (index, logo) in NOYA_LOGO.iter().enumerate() {
        let mut spans = vec![Span::styled(format!("  {logo}   "), logo_style)];
        if let Some(detail) = metadata.get(index) {
            spans.extend(detail.iter().cloned());
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    lines.push(welcome_prompt());
    lines.push(Line::default());
    lines
}

fn welcome_metadata(
    version: &str,
    model: String,
    model_id: &str,
    workspace: String,
) -> [Vec<Span<'static>>; 3] {
    [
        vec![
            Span::styled(
                "Noya",
                Style::default()
                    .fg(FG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" v{version}"), Style::default().fg(DIM)),
        ],
        vec![
            Span::styled(
                model,
                Style::default()
                    .fg(ACCENT_SOFT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {model_id}"),
                Style::default().fg(DIM),
            ),
        ],
        vec![Span::styled(
            workspace,
            Style::default().fg(DIM),
        )],
    ]
}

fn welcome_prompt() -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "Welcome to Noya.",
            Style::default()
                .fg(ACCENT_SOFT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " Type a request or /help.",
            Style::default().fg(DIM),
        ),
    ])
}

fn display_model_name(model: &str) -> String {
    match model {
        "openai" => "OpenAI".to_string(),
        "deepseek" => "DeepSeek".to_string(),
        "qwen" => "Qwen".to_string(),
        "kimi" => "Kimi".to_string(),
        value => value.to_string(),
    }
}

fn display_workspace(workspace: &std::path::Path) -> String {
    let Some(home) = dirs::home_dir() else {
        return workspace.display().to_string();
    };
    let Ok(relative) = workspace.strip_prefix(&home) else {
        return workspace.display().to_string();
    };
    if relative.as_os_str().is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", relative.display())
    }
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
        if message.kind == MessageKind::Tool {
            limit_tool_message_lines(&mut lines, content_style);
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

fn limit_tool_message_lines(lines: &mut Vec<Line<'static>>, content_style: Style) {
    if lines.len() <= TOOL_MESSAGE_MAX_LINES {
        return;
    }

    lines.truncate(TOOL_MESSAGE_MAX_LINES);
    if let Some(last) = lines.last_mut() {
        last.spans
            .push(Span::styled(" …", content_style));
    }
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
        AgentState::Idle => ("Ready".to_string(), SUCCESS),
        AgentState::Thinking => ("Thinking".to_string(), INFO),
        AgentState::Generating => ("Generating".to_string(), ACCENT_SOFT),
        AgentState::RunningTool => ("Running tool".to_string(), WARNING),
        AgentState::WaitingApproval => ("Approval required".to_string(), WARNING),
        AgentState::Error => ("Error".to_string(), ERROR),
    };
    if !matches!(app.agent_state, AgentState::Idle | AgentState::Error) {
        label = format!("{} {label}", spinner());
    }
    if let Some(elapsed) = app.active_turn_elapsed() {
        label.push_str(&format!(
            " ({} · ↓ {} tokens)",
            format_elapsed(elapsed),
            app.active_turn_output_tokens()
        ));
    }
    let mut spans = vec![Span::styled(
        format!("· {label}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )];
    if let Some(status) = &app.status_message {
        spans.push(Span::styled(
            format!("  {status}"),
            Style::default().fg(DIM),
        ));
    }
    if app.background_notifications > 0 {
        spans.push(Span::styled(
            format!("  · {} background notification(s)", app.background_notifications),
            Style::default().fg(WARNING),
        ));
    }
    if !matches!(app.agent_state, AgentState::Idle | AgentState::Error) {
        spans.push(Span::styled("  Esc cancel", Style::default().fg(DIM)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn spinner() -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    static FRAME: AtomicUsize = AtomicUsize::new(0);
    FRAMES[FRAME.fetch_add(1, Ordering::Relaxed) % FRAMES.len()]
}

fn format_elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let border_style = if app.agent_state == AgentState::Idle {
        super::theme::muted_border()
    } else {
        super::theme::active_border()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BORDER_TYPE)
        .border_style(border_style)
        .style(Style::default().bg(SURFACE));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let prompt = Span::styled(
        INPUT_PROMPT,
        Style::default()
            .fg(ACCENT_SOFT)
            .add_modifier(Modifier::BOLD),
    );
    let prompt_width = UnicodeWidthStr::width(INPUT_PROMPT);
    let available = usize::from(inner.width).saturating_sub(prompt_width);
    let rendered_input = if app.model_setup_is_secret() {
        "•".repeat(app.input.chars().count())
    } else {
        app.input.clone()
    };
    let rendered_cursor = rendered_cursor_position(
        &app.input,
        &rendered_input,
        app.cursor_position,
        app.model_setup_is_secret(),
    );
    let (visible, cursor_column) = visible_input(&rendered_input, rendered_cursor, available);
    let text = if app.mode == AppMode::SelectingModel {
        Span::styled(
            "Select a model with ↑/↓ and Enter.",
            Style::default().fg(DIM),
        )
    } else if let Some(prompt) = app.model_setup_prompt() {
        if visible.is_empty() {
            Span::styled(prompt, Style::default().fg(DIM))
        } else {
            Span::styled(visible, input_text_style())
        }
    } else if visible.is_empty() {
        Span::styled(
            "Type a request. /help for commands.",
            Style::default().fg(DIM),
        )
    } else {
        Span::styled(visible, input_text_style())
    };
    frame.render_widget(Paragraph::new(Line::from(vec![prompt, text])), inner);

    let x = inner
        .x
        .saturating_add(u16::try_from(prompt_width + cursor_column).unwrap_or(u16::MAX))
        .min(inner.right().saturating_sub(1));
    frame.set_cursor_position((x, inner.y));
}

fn input_text_style() -> Style {
    Style::default().fg(FG)
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

fn rendered_cursor_position(
    input: &str,
    rendered_input: &str,
    cursor_byte: usize,
    secret: bool,
) -> usize {
    if secret {
        let character_count = input[..cursor_byte.min(input.len())].chars().count();
        rendered_input
            .char_indices()
            .nth(character_count)
            .map_or(rendered_input.len(), |(byte, _)| byte)
    } else {
        cursor_byte
    }
}

fn message_style(kind: MessageKind) -> (&'static str, Style, Style) {
    match kind {
        MessageKind::User => (
            "▸ You",
            Style::default().fg(INFO).add_modifier(Modifier::BOLD),
            Style::default().fg(FG).bg(USER_BG),
        ),
        MessageKind::Agent => (
            "◆ Noya",
            Style::default().fg(ACCENT_SOFT).add_modifier(Modifier::BOLD),
            Style::default().fg(FG),
        ),
        MessageKind::Tool => (
            "└─ Tool",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            Style::default().fg(MUTED).bg(TOOL_PENDING_BG),
        ),
        MessageKind::System => (
            "· System",
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
            Style::default().fg(MUTED),
        ),
        MessageKind::Error => (
            "✕ Error",
            Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
            Style::default().fg(ERROR),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{Message, MessageKind};
    use ratatui::layout::Alignment;
    use std::path::PathBuf;

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

    #[test]
    fn tool_messages_use_an_accented_label_and_muted_content() {
        let (label, label_style, content_style) = message_style(MessageKind::Tool);

        assert_eq!(label, "└─ Tool");
        assert_eq!(label_style.fg, Some(ACCENT));
        assert!(label_style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(content_style.fg, Some(MUTED));
    }

    #[test]
    fn typed_input_uses_the_theme_foreground() {
        assert_eq!(input_text_style().fg, Some(FG));
    }

    #[test]
    fn tool_messages_are_limited_to_three_visible_lines() {
        let message = Message::new(
            MessageKind::Tool,
            "run_command this is a very long tool invocation that should be truncated",
        );

        let lines = message_lines(&message, 24);
        let visible = &lines[..lines.len() - 1];
        let rendered = visible
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(visible.len(), TOOL_MESSAGE_MAX_LINES);
        assert!(rendered.ends_with("…"));
    }

    #[test]
    fn wide_welcome_shows_identity_model_and_workspace() {
        let info = AppInfo {
            workspace: PathBuf::from("/repo/noya"),
            model: "deepseek".to_string(),
            model_id: "deepseek-v4-flash".to_string(),
        };

        let rendered = welcome_lines(&info, 80)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains(&format!("Noya v{}", env!("CARGO_PKG_VERSION"))));
        assert!(rendered.contains("DeepSeek · deepseek-v4-flash"));
        assert!(rendered.contains("/repo/noya"));
        assert!(rendered.contains("Welcome to Noya."));
        assert!(rendered.contains("█   █  ███  █   █  ███"));
        assert!(!rendered.contains("System"));
    }

    #[test]
    fn compact_welcome_keeps_runtime_information() {
        let info = AppInfo {
            workspace: PathBuf::from("/repo"),
            model: "qwen".to_string(),
            model_id: "qwen3-coder-plus".to_string(),
        };

        let rendered = welcome_lines(&info, 40)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("Noya"));
        assert!(rendered.contains("Qwen · qwen3-coder-plus"));
        assert!(rendered.contains("/repo"));
        assert!(rendered.contains("█ █ █ █   █   █   █████"));
    }

    #[test]
    fn command_menu_renders_selection_descriptions_and_navigation_hint() {
        let mut app = App::new(AppInfo {
            workspace: PathBuf::from("/repo"),
            model: "qwen".to_string(),
            model_id: "qwen3-coder-plus".to_string(),
        });
        app.input = "/".to_string();
        app.input_changed();

        let rendered = command_menu_lines(&app, 6)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("› new"));
        assert!(rendered.contains("Start a new session"));
        assert!(rendered.contains("↑/↓ navigate"));
        assert!(rendered.contains("(1/19)"));
    }

    #[test]
    fn command_menu_is_laid_out_below_the_input() {
        let [stream, status, input, menu] = tui_areas(Rect::new(0, 0, 80, VIEWPORT_HEIGHT), true);

        assert!(stream.is_empty());
        assert_eq!(status.height, 1);
        assert_eq!(input.height, 3);
        assert_eq!(menu.y, input.bottom());
        assert_eq!(menu.height, COMMAND_MENU_HEIGHT);
    }

    #[test]
    fn input_area_leaves_a_row_for_text_inside_borders() {
        let [_, _, input, _] = tui_areas(Rect::new(0, 0, 80, VIEWPORT_HEIGHT), false);
        let inner = Block::default().borders(Borders::ALL).inner(input);

        assert_eq!(inner.height, 1);
    }

    #[test]
    fn formats_elapsed_status_for_seconds_and_minutes() {
        assert_eq!(format_elapsed(Duration::from_secs(9)), "9s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "2m 05s");
    }

    #[test]
    fn model_menu_renders_choices_and_current_marker() {
        let mut app = App::new(AppInfo {
            workspace: PathBuf::from("/repo"),
            model: "qwen".to_string(),
            model_id: "qwen3-coder-plus".to_string(),
        });
        app.open_model_menu(vec![
            crate::tui::app::ModelChoice {
                model: "deepseek".to_string(),
                model_id: "deepseek-v4-flash".to_string(),
                current: false,
            },
            crate::tui::app::ModelChoice {
                model: "qwen".to_string(),
                model_id: "qwen3-coder-plus".to_string(),
                current: true,
            },
        ]);

        let rendered = model_menu_lines(&app, 6)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("deepseek"));
        assert!(rendered.contains("› qwen"));
        assert!(rendered.contains("✓ current"));
        assert!(rendered.contains("All models"));
    }

    #[test]
    fn secret_input_maps_utf8_cursor_to_masked_input_index() {
        let input = "é";
        let rendered_cursor = rendered_cursor_position(input, "•", input.len(), true);
        let (visible, cursor_column) = visible_input("•", rendered_cursor, 10);

        assert_eq!(visible, "•");
        assert_eq!(cursor_column, 1);
    }
}
