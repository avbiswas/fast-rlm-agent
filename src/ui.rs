//! The view. Everything renders into one vertical stack (the transcript),
//! including tool calls (header + output) and inline diffs. The only floating
//! element is the multiple-choice question selector.

use ratatui::{
    layout::{Constraint, Direction, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Frame,
};

use crate::app::{App, Item, Mode, QuestionModal, ToolBody, ToolRun};
use crate::markdown;
use crate::tools::ToolStatus;

const ACCENT: Color = Color::Rgb(136, 192, 208);
const GREEN: Color = Color::Rgb(163, 190, 140);
const RED: Color = Color::Rgb(191, 97, 106);
const YELLOW: Color = Color::Rgb(235, 203, 139);
const DIM: Color = Color::DarkGray;
const ADD_BG: Color = Color::Rgb(28, 46, 28);
const DEL_BG: Color = Color::Rgb(54, 28, 30);

/// Cap on how many output lines a tool dumps inline.
const MAX_OUTPUT_LINES: usize = 16;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let input_h = (app.composer.line_count() as u16 + 2).clamp(3, 10);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(input_h)])
        .split(frame.area());

    draw_transcript(frame, app, chunks[0]);
    draw_input(frame, app, chunks[1]);

    if let Mode::Question(modal) = &app.mode {
        draw_question(frame, modal);
    }
}

// ---- transcript ----------------------------------------------------------

fn draw_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let mut title = if app.streaming {
        " chat · ⏳ working — Esc to cancel".to_string()
    } else {
        " chat".to_string()
    };
    // Live cache telemetry: cached/prompt tokens of the latest request.
    // Cached collapsing to 0 on a growing conversation = a cache regression.
    if let Some(u) = &app.usage {
        title.push_str(&format!(
            " · {} cached / {} in · {} out",
            fmt_tokens(u.cached_tokens),
            fmt_tokens(u.prompt_tokens),
            fmt_tokens(u.completion_tokens),
        ));
    }
    title.push(' ');
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);

    let text = build_transcript(app);
    let total = text.lines.len() as u16;
    let viewport = inner.height;
    let max_scroll = total.saturating_sub(viewport);

    if app.follow {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll >= max_scroll {
            app.follow = true;
        }
    }

    frame.render_widget(
        Paragraph::new(text)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((app.scroll, 0)),
        area,
    );

    if total > viewport {
        let mut state = ScrollbarState::new(total as usize)
            .viewport_content_length(viewport as usize)
            .position(app.scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            inner,
            &mut state,
        );
    }
}

fn build_transcript(app: &App) -> Text<'static> {
    if app.items.is_empty() {
        let cfg = &app.config;
        let yn = |on: bool| if on { ("set", GREEN) } else { ("missing", RED) };
        let (api_label, api_color) = yn(cfg.api_key.is_some());
        let (exa_label, exa_color) = yn(cfg.web_search_enabled());
        return Text::from(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Welcome to fast-rlm-agent.",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            cfg_line("  Model", &cfg.model, ACCENT),
            cfg_line("  Endpoint", cfg.base_host(), ACCENT),
            Line::from(vec![
                Span::styled("  API_KEY     ", Style::default().fg(DIM)),
                Span::styled(api_label, Style::default().fg(api_color)),
            ]),
            Line::from(vec![
                Span::styled("  Web search  ", Style::default().fg(DIM)),
                Span::styled(format!("Exa ({exa_label})"), Style::default().fg(exa_color)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Ask for something and watch the tool-call loop run.",
                Style::default().fg(DIM),
            )),
            Line::from(Span::styled(
                "  (Alt+Enter = newline · Enter = send · Esc = quit/cancel)",
                Style::default().fg(DIM),
            )),
        ]);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    for item in &app.items {
        match item {
            Item::User(text) => {
                lines.push(role_header("You", ACCENT));
                for raw in text.lines() {
                    lines.push(Line::from(raw.to_string()));
                }
            }
            Item::Assistant(text) => {
                lines.push(role_header("Agent", GREEN));
                lines.extend(markdown::render(text).lines);
            }
            Item::Note(text) => {
                lines.push(Line::from(Span::styled(
                    format!("  {text}"),
                    Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
                )));
            }
            Item::Tool(run) => lines.extend(render_tool(run)),
        }
        lines.push(Line::from(""));
    }
    Text::from(lines)
}

fn cfg_line(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), Style::default().fg(DIM)),
        Span::styled(value.to_string(), Style::default().fg(color)),
    ])
}

fn role_header(label: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(
        label.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

// ---- unified tool rendering ----------------------------------------------

fn render_tool(run: &ToolRun) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Header: ● Verb(arg)
    let bullet = match run.status {
        ToolStatus::Pending | ToolStatus::Running => YELLOW,
        ToolStatus::Done => GREEN,
        ToolStatus::Rejected | ToolStatus::Failed => RED,
    };
    lines.push(Line::from(vec![
        Span::styled("● ", Style::default().fg(bullet)),
        Span::styled(
            run.verb.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("({})", run.arg), Style::default().fg(DIM)),
    ]));

    // Summary: ⎿ <summary or status>
    let summary = run.summary.clone().unwrap_or_else(|| {
        match run.status {
            ToolStatus::Pending => "awaiting approval",
            ToolStatus::Running => "running…",
            _ => "",
        }
        .to_string()
    });
    if !summary.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  ⎿  ", Style::default().fg(DIM)),
            Span::styled(summary, Style::default().fg(DIM)),
        ]));
    }

    // Body
    match &run.body {
        ToolBody::Diff { path, old, new } => lines.extend(render_diff(path, old, new)),
        ToolBody::Text { output: Some(text) } => lines.extend(render_output(text)),
        ToolBody::Text { output: None } => {}
    }

    // Approval prompt, inline below the call.
    if run.status == ToolStatus::Pending {
        let question = if run.verb == "Bash" {
            "Run this command?"
        } else {
            "Apply this change?"
        };
        lines.push(approval_prompt(question));
    }

    lines
}

/// Indented command/search output, truncated to a sane number of lines.
fn render_output(text: &str) -> Vec<Line<'static>> {
    let all: Vec<&str> = text.lines().collect();
    let shown = all.len().min(MAX_OUTPUT_LINES);
    let mut lines: Vec<Line<'static>> = all[..shown]
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                format!("     {l}"),
                Style::default().fg(Color::Gray),
            ))
        })
        .collect();
    if all.len() > shown {
        lines.push(Line::from(Span::styled(
            format!("     … (+{} more lines)", all.len() - shown),
            Style::default().fg(DIM),
        )));
    }
    lines
}

/// Numbered, syntax-highlighted unified diff. Long runs of unchanged lines
/// are collapsed to ±`DIFF_CONTEXT` around each change, so a small edit in a
/// large file shows hunks instead of the whole file.
fn render_diff(path: &str, old: &str, new: &str) -> Vec<Line<'static>> {
    use similar::{ChangeTag, TextDiff};

    const DIFF_CONTEXT: usize = 3;

    let lang = lang_of(path);
    let new_hl = markdown::highlight_code_lines(new, lang);
    let old_hl = markdown::highlight_code_lines(old, lang);

    let diff = TextDiff::from_lines(old, new);
    let changes: Vec<_> = diff.iter_all_changes().collect();
    let n = changes.len();

    // Keep every changed line plus DIFF_CONTEXT lines around it.
    let mut keep = vec![false; n];
    for (i, change) in changes.iter().enumerate() {
        if change.tag() != ChangeTag::Equal {
            let lo = i.saturating_sub(DIFF_CONTEXT);
            let hi = (i + DIFF_CONTEXT).min(n.saturating_sub(1));
            for k in keep.iter_mut().take(hi + 1).skip(lo) {
                *k = true;
            }
        }
    }

    let mut lines = Vec::new();
    let mut i = 0;
    while i < n {
        if !keep[i] {
            let start = i;
            while i < n && !keep[i] {
                i += 1;
            }
            lines.push(Line::from(Span::styled(
                format!("       ··· {} unchanged lines ···", i - start),
                Style::default().fg(DIM),
            )));
            continue;
        }

        let change = &changes[i];
        let (sign, sign_color, bg, idx, hl) = match change.tag() {
            ChangeTag::Insert => ("+", GREEN, Some(ADD_BG), change.new_index(), &new_hl),
            ChangeTag::Delete => ("-", RED, Some(DEL_BG), change.old_index(), &old_hl),
            ChangeTag::Equal => (" ", DIM, None, change.new_index(), &new_hl),
        };

        let lineno = idx
            .map(|n| format!("{:>4} ", n + 1))
            .unwrap_or_else(|| "     ".to_string());

        let mut spans = vec![
            Span::styled(lineno, Style::default().fg(DIM)),
            Span::styled(format!("{sign} "), Style::default().fg(sign_color)),
        ];
        if let Some(code) = idx.and_then(|i| hl.get(i)) {
            for s in code {
                let mut style = s.style;
                if let Some(bg) = bg {
                    style = style.bg(bg);
                }
                spans.push(Span::styled(s.content.clone().into_owned(), style));
            }
        }

        let mut line = Line::from(spans);
        if let Some(bg) = bg {
            line = line.style(Style::default().bg(bg));
        }
        lines.push(line);
        i += 1;
    }
    lines
}

fn approval_prompt(question: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ▌ ", Style::default().fg(ACCENT)),
        Span::styled(
            format!("{question} "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("y", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
        Span::styled(" approve   ", Style::default().fg(DIM)),
        Span::styled("n", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
        Span::styled(" reject   ", Style::default().fg(DIM)),
        Span::styled("esc", Style::default().fg(DIM)),
        Span::styled(" cancel", Style::default().fg(DIM)),
    ])
}

/// Map a path to a syntect token (its extension works for rs/py/ts/…).
fn lang_of(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or("")
}

/// 999 → "999", 12_345 → "12.3k".
fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

// ---- input ---------------------------------------------------------------

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let (hint, color) = match app.mode {
        Mode::Approve { .. } => ("awaiting approval — y / n", YELLOW),
        Mode::Question(_) => ("answer the question above", YELLOW),
        Mode::Chat if app.streaming => ("working… Esc cancels", DIM),
        Mode::Chat => ("Enter send · Alt+Enter newline · Esc quit", DIM),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" message ")
        .title_bottom(Line::from(Span::styled(hint, Style::default().fg(color))).right_aligned());
    let inner = block.inner(area);

    let (cur_row, cur_col) = app.composer.cursor_row_col();
    let scroll_y = (cur_row as u16).saturating_sub(inner.height.saturating_sub(1));
    let scroll_x = (cur_col as u16).saturating_sub(inner.width.saturating_sub(1));

    frame.render_widget(
        Paragraph::new(app.composer.text())
            .block(block)
            .scroll((scroll_y, scroll_x)),
        area,
    );

    if matches!(app.mode, Mode::Chat) {
        let x = inner.x + (cur_col as u16).saturating_sub(scroll_x);
        let y = inner.y + (cur_row as u16).saturating_sub(scroll_y);
        frame.set_cursor_position((x, y));
    }
}

// ---- question modal ------------------------------------------------------

fn draw_question(frame: &mut Frame, modal: &QuestionModal) {
    let area = centered(frame.area(), 70, 60);
    frame.render_widget(Clear, area);

    let footer = if modal.question.multi_select {
        " ↑/↓ move   space toggle   enter confirm   esc cancel "
    } else {
        " ↑/↓ move   enter select   1-9 quick   esc cancel "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(format!(" ❓ {} ", modal.question.header))
        .title_bottom(Line::from(Span::styled(footer, Style::default().fg(DIM))).centered());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            modal.question.question.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    for (i, opt) in modal.question.options.iter().enumerate() {
        let active = i == modal.cursor;
        let marker = if modal.question.multi_select {
            if modal.selected[i] {
                "[x]"
            } else {
                "[ ]"
            }
        } else if active {
            "(•)"
        } else {
            "( )"
        };
        let pointer = if active { "▶ " } else { "  " };
        let label_style = if active {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(pointer.to_string(), Style::default().fg(ACCENT)),
            Span::styled(format!("{marker} "), Style::default().fg(DIM)),
            Span::styled(format!("{}. ", i + 1), Style::default().fg(DIM)),
            Span::styled(opt.label.clone(), label_style),
        ]));
        lines.push(Line::from(Span::styled(
            format!("        {}", opt.description),
            Style::default().fg(DIM),
        )));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let horizontal = Layout::horizontal([Constraint::Percentage(pct_x)])
        .flex(Flex::Center)
        .split(area);
    Layout::vertical([Constraint::Percentage(pct_y)])
        .flex(Flex::Center)
        .split(horizontal[0])[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn diff_collapses_unchanged_runs() {
        // 100 identical lines, one change in the middle.
        let old: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 50", "line fifty");

        let lines = render_diff("test.txt", &old, &new);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        // Both long equal runs collapse into "··· N unchanged lines ···".
        let separators: Vec<&String> = texts.iter().filter(|t| t.contains("unchanged")).collect();
        assert_eq!(separators.len(), 2, "lines: {texts:#?}");
        // ±3 context + 1 delete + 1 insert + 2 separators — not 100 lines.
        assert!(texts.len() <= 12, "got {} lines", texts.len());
        // The change itself is present with signs and line numbers.
        assert!(texts.iter().any(|t| t.contains("- line 50")));
        assert!(texts.iter().any(|t| t.contains("+ line fifty")));
        assert!(texts.iter().any(|t| t.trim_start().starts_with("51")));
    }

    #[test]
    fn small_diff_is_not_collapsed() {
        let lines = render_diff("t.txt", "a\nb\nc\n", "a\nB\nc\n");
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(!texts.iter().any(|t| t.contains("unchanged")));
        assert!(texts.iter().any(|t| t.contains("- b")));
        assert!(texts.iter().any(|t| t.contains("+ B")));
    }
}
