use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Render Markdown into terminal-width-aware, styled lines.
///
/// Parsing and wrapping live behind this single interface so completed messages and
/// partial streaming messages always use the same Markdown semantics.
pub(super) fn render(markdown: &str, width: usize, base_style: Style) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let mut renderer = Renderer::new(base_style);
    for event in Parser::new_ext(markdown, options) {
        renderer.handle(event);
    }
    renderer.finish(width.max(1))
}

#[derive(Debug)]
struct ListState {
    next_number: Option<u64>,
}

impl ListState {
    fn marker(&mut self) -> String {
        match &mut self.next_number {
            Some(number) => {
                let marker = format!("{number}. ");
                *number = number.saturating_add(1);
                marker
            }
            None => "• ".to_string(),
        }
    }
}

#[derive(Debug)]
struct ItemState {
    marker: String,
    first_line_written: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RichLineKind {
    Normal,
    Rule,
}

#[derive(Debug)]
struct RichLine {
    spans: Vec<Span<'static>>,
    first_prefix: String,
    continuation_prefix: String,
    preserve_whitespace: bool,
    kind: RichLineKind,
}

struct Renderer {
    base_style: Style,
    lines: Vec<RichLine>,
    current: Vec<Span<'static>>,
    lists: Vec<ListState>,
    items: Vec<ItemState>,
    links: Vec<String>,
    quote_depth: usize,
    heading: Option<HeadingLevel>,
    emphasis_depth: usize,
    strong_depth: usize,
    strike_depth: usize,
    code_block: bool,
    image_depth: usize,
}

impl Renderer {
    fn new(base_style: Style) -> Self {
        Self {
            base_style,
            lines: Vec::new(),
            current: Vec::new(),
            lists: Vec::new(),
            items: Vec::new(),
            links: Vec::new(),
            quote_depth: 0,
            heading: None,
            emphasis_depth: 0,
            strong_depth: 0,
            strike_depth: 0,
            code_block: false,
            image_depth: 0,
        }
    }

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.append_text(&text),
            Event::Code(code) => {
                let style = self.current_style().patch(
                    Style::default()
                        .fg(Color::LightMagenta)
                        .bg(Color::Rgb(35, 35, 35)),
                );
                self.append(&code, style);
            }
            Event::InlineMath(math) => {
                let style = self
                    .current_style()
                    .patch(Style::default().fg(Color::Magenta));
                self.append(&format!("${math}$"), style);
            }
            Event::DisplayMath(math) => {
                self.flush_line();
                let style = self
                    .current_style()
                    .patch(Style::default().fg(Color::Magenta));
                self.append(&format!("$${math}$$"), style);
                self.flush_line();
                self.push_blank();
            }
            Event::Html(html) | Event::InlineHtml(html) => self.append_text(&html),
            Event::FootnoteReference(label) => {
                let style = self.current_style().patch(Style::default().fg(Color::Cyan));
                self.append(&format!("[^{label}]"), style);
            }
            Event::SoftBreak => self.append(" ", self.current_style()),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                let (first_prefix, continuation_prefix) = self.prefixes();
                self.lines.push(RichLine {
                    spans: Vec::new(),
                    first_prefix,
                    continuation_prefix,
                    preserve_whitespace: false,
                    kind: RichLineKind::Rule,
                });
                self.push_blank();
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.append(
                    marker,
                    self.current_style().patch(Style::default().fg(Color::Cyan)),
                );
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.code_block = true;
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    let style = self.base_style.patch(Style::default().fg(Color::DarkGray));
                    self.append(&format!("{language}"), style);
                    self.flush_line();
                }
            }
            Tag::List(start) => {
                self.flush_line();
                self.lists.push(ListState { next_number: start });
            }
            Tag::Item => {
                self.flush_line();
                let marker = self
                    .lists
                    .last_mut()
                    .map(ListState::marker)
                    .unwrap_or_else(|| "• ".to_string());
                self.items.push(ItemState {
                    marker,
                    first_line_written: false,
                });
            }
            Tag::Emphasis => self.emphasis_depth = self.emphasis_depth.saturating_add(1),
            Tag::Strong => self.strong_depth = self.strong_depth.saturating_add(1),
            Tag::Strikethrough => self.strike_depth = self.strike_depth.saturating_add(1),
            Tag::Link { dest_url, .. } => self.links.push(dest_url.into_string()),
            Tag::Image { dest_url, .. } => {
                self.image_depth = self.image_depth.saturating_add(1);
                self.links.push(dest_url.into_string());
                self.append("[image: ", self.current_style());
            }
            Tag::FootnoteDefinition(label) => {
                self.flush_line();
                let style = self.current_style().patch(Style::default().fg(Color::Cyan));
                self.append(&format!("[^{label}]: "), style);
            }
            Tag::TableRow => {
                self.flush_line();
                self.append(
                    "│ ",
                    self.base_style.patch(Style::default().fg(Color::DarkGray)),
                );
            }
            Tag::DefinitionListTitle => {
                self.flush_line();
                self.strong_depth = self.strong_depth.saturating_add(1);
            }
            Tag::DefinitionListDefinition => {
                self.flush_line();
                self.append("  ", self.base_style);
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                if self.items.is_empty() {
                    self.push_blank();
                }
            }
            TagEnd::Heading(_) => {
                self.flush_line();
                self.heading = None;
                self.push_blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.push_blank();
            }
            TagEnd::CodeBlock => {
                self.flush_line();
                self.code_block = false;
                self.push_blank();
            }
            TagEnd::List(_) => {
                self.flush_line();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.push_blank();
                }
            }
            TagEnd::Item => {
                self.flush_line();
                self.items.pop();
            }
            TagEnd::Emphasis => self.emphasis_depth = self.emphasis_depth.saturating_sub(1),
            TagEnd::Strong => self.strong_depth = self.strong_depth.saturating_sub(1),
            TagEnd::Strikethrough => self.strike_depth = self.strike_depth.saturating_sub(1),
            TagEnd::Link => self.finish_link(),
            TagEnd::Image => {
                self.append("]", self.current_style());
                self.finish_link();
                self.image_depth = self.image_depth.saturating_sub(1);
            }
            TagEnd::FootnoteDefinition => {
                self.flush_line();
                self.push_blank();
            }
            TagEnd::TableCell => self.append(
                " │ ",
                self.base_style.patch(Style::default().fg(Color::DarkGray)),
            ),
            TagEnd::TableRow => self.flush_line(),
            TagEnd::TableHead => {
                let (first_prefix, continuation_prefix) = self.prefixes();
                self.lines.push(RichLine {
                    spans: Vec::new(),
                    first_prefix,
                    continuation_prefix,
                    preserve_whitespace: false,
                    kind: RichLineKind::Rule,
                });
            }
            TagEnd::Table => self.push_blank(),
            TagEnd::DefinitionListTitle => {
                self.strong_depth = self.strong_depth.saturating_sub(1);
                self.flush_line();
            }
            TagEnd::DefinitionListDefinition => self.flush_line(),
            _ => {}
        }
    }

    fn finish_link(&mut self) {
        let destination = self.links.pop().unwrap_or_default();
        if destination.is_empty() {
            return;
        }
        let style = self.base_style.patch(Style::default().fg(Color::DarkGray));
        self.append(&format!(" ({destination})"), style);
    }

    fn append_text(&mut self, text: &str) {
        let style = self.current_style();
        if self.code_block {
            let style = style.patch(
                Style::default()
                    .fg(Color::LightCyan)
                    .bg(Color::Rgb(28, 28, 28)),
            );
            for (index, part) in text.split('\n').enumerate() {
                if index > 0 {
                    self.flush_line();
                }
                if !part.is_empty() {
                    self.append(part, style);
                }
            }
        } else {
            self.append(text, style);
        }
    }

    fn append(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.current.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(text);
            return;
        }
        self.current.push(Span::styled(text.to_string(), style));
    }

    fn current_style(&self) -> Style {
        let mut style = self.base_style;
        if let Some(level) = self.heading {
            let color = match level {
                HeadingLevel::H1 => Color::LightGreen,
                HeadingLevel::H2 => Color::LightCyan,
                HeadingLevel::H3 => Color::LightBlue,
                _ => Color::Cyan,
            };
            style = style.patch(Style::default().fg(color).add_modifier(Modifier::BOLD));
        }
        if self.emphasis_depth > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strong_depth > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.strike_depth > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if !self.links.is_empty() && self.image_depth == 0 {
            style = style.patch(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::UNDERLINED),
            );
        }
        style
    }

    fn prefixes(&self) -> (String, String) {
        let quote = "│ ".repeat(self.quote_depth);
        let code = if self.code_block { "  " } else { "" };
        if let Some(item) = self.items.last() {
            let indent = "  ".repeat(self.lists.len().saturating_sub(1));
            let continuation = " ".repeat(UnicodeWidthStr::width(item.marker.as_str()));
            let first_marker = if item.first_line_written {
                continuation.clone()
            } else {
                item.marker.clone()
            };
            (
                format!("{quote}{indent}{first_marker}{code}"),
                format!("{quote}{indent}{continuation}{code}"),
            )
        } else {
            (format!("{quote}{code}"), format!("{quote}{code}"))
        }
    }

    fn flush_line(&mut self) {
        if self.current.is_empty() {
            return;
        }
        let (first_prefix, continuation_prefix) = self.prefixes();
        self.lines.push(RichLine {
            spans: std::mem::take(&mut self.current),
            first_prefix,
            continuation_prefix,
            preserve_whitespace: self.code_block,
            kind: RichLineKind::Normal,
        });
        if let Some(item) = self.items.last_mut() {
            item.first_line_written = true;
        }
    }

    fn push_blank(&mut self) {
        if self
            .lines
            .last()
            .is_some_and(|line| line.spans.is_empty() && line.kind == RichLineKind::Normal)
        {
            return;
        }
        self.lines.push(RichLine {
            spans: Vec::new(),
            first_prefix: String::new(),
            continuation_prefix: String::new(),
            preserve_whitespace: false,
            kind: RichLineKind::Normal,
        });
    }

    fn finish(mut self, width: usize) -> Vec<Line<'static>> {
        self.flush_line();
        while self
            .lines
            .last()
            .is_some_and(|line| line.spans.is_empty() && line.kind == RichLineKind::Normal)
        {
            self.lines.pop();
        }

        let mut output = Vec::new();
        for line in self.lines {
            output.extend(wrap_line(line, width, self.base_style));
        }
        if output.is_empty() {
            output.push(Line::default());
        }
        output
    }
}

struct LineBuilder {
    spans: Vec<Span<'static>>,
    body_width: usize,
    prefix_width: usize,
}

impl LineBuilder {
    fn new(prefix: &str, base_style: Style) -> Self {
        let spans = if prefix.is_empty() {
            Vec::new()
        } else {
            vec![Span::styled(
                prefix.to_string(),
                base_style.patch(Style::default().fg(Color::DarkGray)),
            )]
        };
        Self {
            spans,
            body_width: 0,
            prefix_width: UnicodeWidthStr::width(prefix),
        }
    }

    fn remaining(&self, width: usize) -> usize {
        width
            .saturating_sub(self.prefix_width)
            .saturating_sub(self.body_width)
    }

    fn append(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        self.body_width = self.body_width.saturating_add(UnicodeWidthStr::width(text));
        if let Some(last) = self.spans.last_mut()
            && last.style == style
        {
            last.content.to_mut().push_str(text);
        } else {
            self.spans.push(Span::styled(text.to_string(), style));
        }
    }

    fn finish(self) -> Line<'static> {
        Line::from(self.spans)
    }
}

fn wrap_line(line: RichLine, width: usize, base_style: Style) -> Vec<Line<'static>> {
    if line.kind == RichLineKind::Rule {
        let prefix_width = UnicodeWidthStr::width(line.first_prefix.as_str());
        let rule_width = width.saturating_sub(prefix_width).max(1);
        return vec![Line::from(vec![
            Span::styled(
                line.first_prefix,
                base_style.patch(Style::default().fg(Color::DarkGray)),
            ),
            Span::styled(
                "─".repeat(rule_width),
                base_style.patch(Style::default().fg(Color::DarkGray)),
            ),
        ])];
    }
    if line.spans.is_empty() {
        return vec![Line::default()];
    }

    let continuation_prefix = line.continuation_prefix;
    let mut output = Vec::new();
    let mut builder = LineBuilder::new(&line.first_prefix, base_style);
    for span in line.spans {
        for (token, whitespace) in tokens(&span.content) {
            if line.preserve_whitespace {
                append_characters(
                    &token,
                    span.style,
                    width,
                    &continuation_prefix,
                    base_style,
                    &mut builder,
                    &mut output,
                    true,
                );
            } else if whitespace {
                append_whitespace(
                    &token,
                    span.style,
                    width,
                    &continuation_prefix,
                    base_style,
                    &mut builder,
                    &mut output,
                );
            } else {
                append_word(
                    &token,
                    span.style,
                    width,
                    &continuation_prefix,
                    base_style,
                    &mut builder,
                    &mut output,
                );
            }
        }
    }
    output.push(builder.finish());
    output
}

fn tokens(text: &str) -> Vec<(String, bool)> {
    let mut output = Vec::new();
    let mut current = String::new();
    let mut current_whitespace = None;
    for character in text.chars() {
        let whitespace = character.is_whitespace();
        if current_whitespace.is_some_and(|kind| kind != whitespace) {
            output.push((std::mem::take(&mut current), current_whitespace.unwrap()));
        }
        current_whitespace = Some(whitespace);
        current.push(character);
    }
    if let Some(whitespace) = current_whitespace {
        output.push((current, whitespace));
    }
    output
}

#[allow(clippy::too_many_arguments)]
fn append_word(
    word: &str,
    style: Style,
    width: usize,
    continuation_prefix: &str,
    base_style: Style,
    builder: &mut LineBuilder,
    output: &mut Vec<Line<'static>>,
) {
    let word_width = UnicodeWidthStr::width(word);
    let continuation_capacity = width
        .saturating_sub(UnicodeWidthStr::width(continuation_prefix))
        .max(1);
    if word_width <= builder.remaining(width) {
        builder.append(word, style);
        return;
    }
    if builder.body_width > 0 && word_width <= continuation_capacity {
        let next = LineBuilder::new(continuation_prefix, base_style);
        output.push(std::mem::replace(builder, next).finish());
        builder.append(word, style);
        return;
    }
    append_characters(
        word,
        style,
        width,
        continuation_prefix,
        base_style,
        builder,
        output,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn append_whitespace(
    whitespace: &str,
    style: Style,
    width: usize,
    continuation_prefix: &str,
    base_style: Style,
    builder: &mut LineBuilder,
    output: &mut Vec<Line<'static>>,
) {
    if builder.body_width == 0 {
        return;
    }
    append_characters(
        whitespace,
        style,
        width,
        continuation_prefix,
        base_style,
        builder,
        output,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
fn append_characters(
    text: &str,
    style: Style,
    width: usize,
    continuation_prefix: &str,
    base_style: Style,
    builder: &mut LineBuilder,
    output: &mut Vec<Line<'static>>,
    preserve_leading_whitespace: bool,
) {
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if character_width > builder.remaining(width) && builder.body_width > 0 {
            let next = LineBuilder::new(continuation_prefix, base_style);
            output.push(std::mem::replace(builder, next).finish());
        }
        if !preserve_leading_whitespace && builder.body_width == 0 && character.is_whitespace() {
            continue;
        }
        let mut encoded = [0; 4];
        builder.append(character.encode_utf8(&mut encoded), style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_common_markdown_blocks() {
        let markdown =
            "# 标题\n\n- first\n- **重点** and `code`\n\n> quote\n\n[docs](https://example.com)";
        let lines = render(markdown, 60, Style::default());
        let rendered = text(&lines);

        assert!(rendered.contains("标题"));
        assert!(rendered.contains("• first"));
        assert!(rendered.contains("• 重点 and code"));
        assert!(rendered.contains("│ quote"));
        assert!(rendered.contains("docs (https://example.com)"));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "重点" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "code" && span.style.fg == Some(Color::LightMagenta)
        }));
    }

    #[test]
    fn preserves_fenced_code_whitespace() {
        let lines = render(
            "```rust\nfn main() {\n    ok();\n}\n```",
            40,
            Style::default(),
        );
        let rendered = text(&lines);

        assert!(rendered.contains("  rust"));
        assert!(rendered.contains("      ok();"));
    }

    #[test]
    fn wraps_chinese_by_display_width_without_inserting_spaces() {
        let lines = render("中文输出非常自然", 8, Style::default());

        assert_eq!(text(&lines).replace('\n', ""), "中文输出非常自然");
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.to_string().as_str()) <= 8)
        );
    }

    #[test]
    fn accepts_incomplete_streaming_markdown() {
        let lines = render(
            "回答中：**加粗内容\n\n```rust\nfn main()",
            30,
            Style::default(),
        );
        let rendered = text(&lines);

        assert!(rendered.contains("回答中："));
        assert!(rendered.contains("fn main()"));
    }
}
