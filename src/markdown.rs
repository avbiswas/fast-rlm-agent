//! Markdown → ratatui `Text`, with syntect-highlighted code blocks.
//!
//! This is intentionally compact: it handles the inline + block constructs you
//! actually see in chat (headings, emphasis, lists, blockquotes, code) and
//! leans on `pulldown-cmark`'s event stream rather than a full AST.

use std::sync::OnceLock;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Theme, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

/// Render a markdown string into owned ratatui `Text`.
pub fn render(input: &str) -> Text<'static> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(input, opts);

    let mut w = Writer::default();
    for event in parser {
        w.event(event);
    }
    w.finish()
}

#[derive(Default)]
struct Writer {
    lines: Vec<Line<'static>>,
    /// Spans accumulated for the line currently being built.
    spans: Vec<Span<'static>>,
    /// Current inline style (emphasis/strong/strikethrough stack into this).
    style: Style,
    /// Indentation depth + ordered-list counters (None = unordered).
    list_stack: Vec<Option<u64>>,
    /// Fenced code-block state.
    code: Option<(String, String)>, // (lang, buffer)
}

impl Writer {
    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => self.spans.push(Span::styled(
                t.into_string(),
                Style::default().fg(Color::Yellow),
            )),
            Event::SoftBreak => self.spans.push(Span::raw(" ")),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(24),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.flush_line();
                self.style = self.style.add_modifier(Modifier::BOLD);
                let hashes = "#".repeat(heading_level(level));
                self.spans.push(Span::styled(
                    format!("{hashes} "),
                    Style::default().fg(Color::Magenta),
                ));
            }
            Tag::Paragraph => self.flush_line(),
            Tag::Emphasis => self.style = self.style.add_modifier(Modifier::ITALIC),
            Tag::Strong => self.style = self.style.add_modifier(Modifier::BOLD),
            Tag::Strikethrough => self.style = self.style.add_modifier(Modifier::CROSSED_OUT),
            Tag::BlockQuote(_) => {
                self.flush_line();
                self.spans.push(Span::styled("▏ ", Style::default().fg(Color::DarkGray)));
            }
            Tag::List(start) => self.list_stack.push(start),
            Tag::Item => {
                self.flush_line();
                let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.spans.push(Span::styled(
                    format!("{indent}{marker}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split(|c: char| c == ',' || c.is_whitespace())
                            .next()
                            .unwrap_or("")
                            .to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.style = self.style.remove_modifier(Modifier::BOLD);
                self.flush_line();
                self.lines.push(Line::from(""));
            }
            TagEnd::Paragraph => {
                self.flush_line();
                self.lines.push(Line::from(""));
            }
            TagEnd::Emphasis => self.style = self.style.remove_modifier(Modifier::ITALIC),
            TagEnd::Strong => self.style = self.style.remove_modifier(Modifier::BOLD),
            TagEnd::Strikethrough => self.style = self.style.remove_modifier(Modifier::CROSSED_OUT),
            TagEnd::BlockQuote(_) => self.flush_line(),
            TagEnd::List(_) => {
                self.list_stack.pop();
            }
            TagEnd::Item => self.flush_line(),
            TagEnd::CodeBlock => {
                if let Some((lang, buf)) = self.code.take() {
                    self.lines.extend(highlight(&buf, &lang));
                    self.lines.push(Line::from(""));
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, t: &str) {
        if let Some((_, buf)) = self.code.as_mut() {
            buf.push_str(t);
        } else {
            self.spans.push(Span::styled(t.to_string(), self.style));
        }
    }

    fn flush_line(&mut self) {
        if !self.spans.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        }
    }

    fn finish(mut self) -> Text<'static> {
        self.flush_line();
        // Trim a trailing blank line for tidiness.
        if matches!(self.lines.last(), Some(l) if l.spans.is_empty()) {
            self.lines.pop();
        }
        Text::from(self.lines)
    }
}

fn heading_level(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Syntax-highlight `code`, returning the styled spans for each line (no
/// gutter, no trailing newline). Shared by markdown code blocks and the inline
/// diff renderer.
pub fn highlight_code_lines(code: &str, lang: &str) -> Vec<Vec<Span<'static>>> {
    let (syntax_set, theme) = assets();
    let syntax = syntax_set
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);

    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let spans = match highlighter.highlight_line(line, syntax_set) {
            Ok(ranges) => ranges
                .iter()
                .map(|(style, text)| {
                    let color = style.foreground;
                    Span::styled(
                        text.trim_end_matches('\n').to_string(),
                        Style::default().fg(Color::Rgb(color.r, color.g, color.b)),
                    )
                })
                .filter(|s| !s.content.is_empty())
                .collect(),
            Err(_) => vec![Span::raw(line.trim_end_matches('\n').to_string())],
        };
        out.push(spans);
    }
    out
}

/// Syntax-highlight a fenced code block into ratatui lines, with a left gutter.
fn highlight(code: &str, lang: &str) -> Vec<Line<'static>> {
    highlight_code_lines(code, lang)
        .into_iter()
        .map(|mut spans| {
            spans.insert(0, Span::raw("  "));
            Line::from(spans)
        })
        .collect()
}

/// Lazily-built syntax + theme assets. `two-face` ships a richer set of
/// language definitions and themes than syntect's bare defaults.
fn assets() -> (&'static SyntaxSet, &'static Theme) {
    static SYNTAX: OnceLock<SyntaxSet> = OnceLock::new();
    static THEME: OnceLock<Theme> = OnceLock::new();

    let syntax = SYNTAX.get_or_init(two_face::syntax::extra_newlines);
    let theme = THEME.get_or_init(|| {
        let themes = two_face::theme::extra();
        themes
            .get(two_face::theme::EmbeddedThemeName::Nord)
            .clone()
    });
    (syntax, theme)
}

// Keep the bare syntect ThemeSet import alive for users who want to swap in a
// custom theme without pulling two-face.
#[allow(dead_code)]
fn _default_theme() -> Theme {
    ThemeSet::load_defaults().themes["base16-ocean.dark"].clone()
}
