use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line as UiLine, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Tabs, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::ansi;
use crate::app::{App, InputMode, PromptEditor};
use crate::commands::COMMANDS;
use crate::search;
use crate::selection::VisualSelection;
use crate::tab::{TabState, ViewMode};
use crate::theme::Theme;

/// Rows consumed by chrome around the log viewport: tabs row + viewport
/// border (2) + status row, plus the prompt row in edit modes. `app.rs` uses
/// this for scroll math; keep it in lockstep with the layout below.
pub fn chrome_rows(input: &InputMode) -> u16 {
    4 + u16::from(matches!(
        input,
        InputMode::OpenPath(_)
            | InputMode::FilterEdit(_)
            | InputMode::SearchPrompt(_)
            | InputMode::ExportPath(_)
    ))
}

pub fn draw(f: &mut Frame, app: &App) {
    let theme = &app.theme;
    let input_rows = u16::from(matches!(
        app.input,
        InputMode::OpenPath(_)
            | InputMode::FilterEdit(_)
            | InputMode::SearchPrompt(_)
            | InputMode::ExportPath(_)
    ));
    let [tabs_area, view_area, status_area, input_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(input_rows),
    ])
    .areas(f.area());

    // Tabs row.
    let titles: Vec<UiLine> = app
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let name = t
                .state
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| t.state.path.display().to_string());
            let mut spans = vec![Span::raw(format!(" {}:{}", i + 1, name))];
            if t.state.unseen {
                spans.push(Span::styled(theme.unseen_marker, theme.chrome));
            }
            spans.push(Span::raw(" "));
            UiLine::from(spans)
        })
        .collect();
    f.render_widget(
        Tabs::new(titles)
            .select(app.active)
            .highlight_style(theme.tab_active)
            .divider(Span::styled(theme.divider, theme.chrome)),
        tabs_area,
    );

    // Viewport inside a themed border; the title carries the file path.
    let title = app
        .tabs
        .get(app.active)
        .map(|t| format!(" {} ", t.state.path.display()))
        .unwrap_or_else(|| String::from(" lucoa "));
    let block = Block::bordered()
        .border_set(theme.border_set)
        .border_style(theme.chrome)
        .title(Span::styled(title, theme.chrome));
    let inner = block.inner(view_area);
    f.render_widget(block, view_area);

    let status = if let Some(tab) = app.tabs.get(app.active) {
        let s = &tab.state;
        let height = inner.height as usize;
        let visual = match &app.input {
            InputMode::VisualSelection(selection) => Some(selection),
            _ => None,
        };
        let logical = viewport_lines(s, theme, height, visual);
        if s.wrap {
            // Wrap each logical line ourselves (exact, style-preserving) so
            // follow mode can bottom-align: ratatui's Wrap shows the *top*
            // of overflowing content and would cut the tail.
            let mut rows: Vec<UiLine> = logical
                .into_iter()
                .flat_map(|l| wrap_line(l, inner.width as usize))
                .collect();
            if s.follow {
                let skip = rows.len().saturating_sub(height);
                rows.drain(..skip);
            } else {
                rows.truncate(height);
            }
            f.render_widget(Paragraph::new(rows), inner);
        } else {
            f.render_widget(Paragraph::new(logical).scroll((0, s.h_scroll)), inner);
        }
        status_spans(s, theme)
    } else {
        f.render_widget(
            Paragraph::new("no file open — press o to open one, or pass files on the command line"),
            inner,
        );
        UiLine::from(Span::raw(" no tabs"))
    };

    let status = match &app.status_msg {
        Some(msg) => {
            let mut line = status;
            line.push_span(Span::styled(format!(" {} ", theme.divider), theme.chrome));
            line.push_span(Span::styled(msg.clone(), theme.error));
            line
        }
        None => status,
    };
    f.render_widget(Paragraph::new(status), status_area);

    let prompt = match &app.input {
        InputMode::Normal => None,
        InputMode::OpenPath(prompt) => Some(PromptLine {
            label: "open file: ",
            editor: prompt.editor(),
            ghost_suffix: prompt.ghost_suffix(),
        }),
        InputMode::FilterEdit(editor) => Some(PromptLine {
            label: "filter: ",
            editor,
            ghost_suffix: None,
        }),
        InputMode::SearchPrompt(editor) => Some(PromptLine {
            label: "search: ",
            editor,
            ghost_suffix: None,
        }),
        InputMode::ExportPath(editor) => Some(PromptLine {
            label: "export to: ",
            editor,
            ghost_suffix: None,
        }),
        InputMode::VisualSelection(_) | InputMode::HelpOverlay => None,
    };
    if let Some(prompt) = prompt {
        let (visible_text, cursor_col) = prompt_view(prompt.editor, input_area.width, prompt.label);
        let mut spans = vec![
            Span::styled(prompt.label, theme.chrome),
            Span::raw(visible_text),
        ];
        if let Some(suffix) = prompt.ghost_suffix {
            spans.push(Span::styled(suffix.to_owned(), theme.dim));
        }
        f.render_widget(Paragraph::new(UiLine::from(spans)), input_area);
        f.set_cursor_position((input_area.x + cursor_col, input_area.y));
    }

    if matches!(app.input, InputMode::HelpOverlay) {
        render_help_overlay(f, theme);
    }
}

struct PromptLine<'a> {
    label: &'static str,
    editor: &'a PromptEditor,
    ghost_suffix: Option<&'a str>,
}

fn prompt_view<'a>(editor: &'a PromptEditor, width: u16, label: &str) -> (&'a str, u16) {
    let label_width = UnicodeWidthStr::width(label) as u16;
    let text_width = width.saturating_sub(label_width);
    if text_width == 0 {
        return ("", label_width.min(width.saturating_sub(1)));
    }

    let text = editor.text();
    let cursor = editor.cursor();
    let mut start = 0;
    while UnicodeWidthStr::width(&text[start..cursor]) as u16 >= text_width {
        let ch = text[start..]
            .chars()
            .next()
            .expect("start remains before cursor while prompt is too wide");
        start += ch.len_utf8();
    }

    let cursor_offset = UnicodeWidthStr::width(&text[start..cursor]) as u16;
    (&text[start..], label_width + cursor_offset)
}

fn render_help_overlay(f: &mut Frame, theme: &Theme) {
    let area = centered_rect(f.area(), 74, 24);
    let block = Block::bordered()
        .border_set(theme.border_set)
        .border_style(theme.chrome)
        .title(Span::styled(" help ", theme.chrome));

    let mut lines = Vec::new();
    let mut current_section = "";
    for command in COMMANDS {
        if command.section != current_section {
            if !current_section.is_empty() {
                lines.push(UiLine::raw(""));
            }
            current_section = command.section;
            lines.push(UiLine::from(Span::styled(
                current_section.to_owned(),
                theme.tab_active,
            )));
        }
        lines.push(UiLine::from(vec![
            Span::styled(format!("  {:<24}", command.keys), theme.chrome),
            Span::styled(command.description, theme.overlay_style),
        ]));
    }

    f.render_widget(Clear, area);
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(lines)
            .style(theme.overlay_style)
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn centered_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = area.width.min(max_width).max(1);
    let height = area.height.min(max_height).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// Status bar: `MODE │ filter │ buffered/total │ matches │ rate │ export │ FOLLOW`.
fn status_spans<'a>(s: &'a TabState, theme: &Theme) -> UiLine<'a> {
    let sep = Span::styled(format!(" {} ", theme.divider), theme.chrome);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(s.mode.label(), theme.tab_active),
        sep.clone(),
    ];

    if s.filter.raw.is_empty() {
        spans.push(Span::raw("filter:-"));
    } else {
        spans.push(Span::raw(format!(
            "filter:\"{}\" ({}{})",
            s.filter.raw,
            if s.filter.is_regex { "re" } else { "lit" },
            if s.filter.case_insensitive { ",i" } else { "" },
        )));
    }
    spans.push(sep.clone());

    spans.push(Span::raw(format!("{}/{}", s.buffer.len(), s.lines_total)));
    spans.push(sep.clone());
    spans.push(Span::raw(format!("{} matches", s.matches.len())));
    spans.push(sep.clone());

    // A stale rate (no batches for a while) reads as 0, not a frozen number.
    let rate = if s.rate_window_start.elapsed() > std::time::Duration::from_secs(3) {
        0
    } else {
        s.rate
    };
    spans.push(Span::raw(format!("{rate}/s")));
    spans.push(sep.clone());

    match &s.export {
        Some(e) => spans.push(Span::styled(
            format!("export:{}", e.path.display()),
            theme.chrome,
        )),
        None => spans.push(Span::raw("export:off")),
    }
    spans.push(sep.clone());

    if s.wrap {
        spans.push(Span::styled("WRAP", theme.chrome));
        spans.push(sep);
    } else if s.h_scroll > 0 {
        spans.push(Span::raw(format!(">{}", s.h_scroll)));
        spans.push(sep);
    }

    if s.follow {
        spans.push(Span::styled("FOLLOW", theme.follow));
    } else {
        spans.push(Span::styled("PAUSED", theme.paused));
    }

    if let Some(e) = &s.filter.error {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!("ERR {e}"), theme.error));
    }
    UiLine::from(spans)
}

/// Build the visible rows for the current view mode. Only ever touches
/// `height` lines, so per-frame work is O(screen), never O(buffer) — this is
/// what makes mode cycling instant.
fn viewport_lines<'a>(
    s: &'a TabState,
    theme: &Theme,
    height: usize,
    visual: Option<&VisualSelection>,
) -> Vec<UiLine<'a>> {
    let filter = s.filter.compiled.as_deref();
    let top = s.top_position(height);
    match s.mode {
        ViewMode::All => s
            .buffer
            .iter()
            .enumerate()
            .skip(top)
            .take(height)
            .map(|(idx, line)| display_line(s, line, idx, None, theme, visual))
            .collect(),
        ViewMode::Highlight => s
            .buffer
            .iter()
            .enumerate()
            .skip(top)
            .take(height)
            .map(|(idx, l)| {
                let filter_ranges = filter
                    .filter(|_| l.matched)
                    .map(|f| f.ranges(&l.text))
                    .unwrap_or_default();
                if filter.is_some() && !l.matched {
                    let base = dimmed_line(l, theme);
                    let search_ranges = search::ranges(&l.text, &s.search.query);
                    let selection_range =
                        visual.and_then(|sel| sel.selected_range_for_text(idx, &l.text));
                    if let Some(range) = selection_range {
                        line_from_text_with_overlay(
                            &l.text,
                            std::slice::from_ref(&range),
                            theme.selection_style,
                        )
                    } else if search_ranges.is_empty() {
                        base
                    } else {
                        line_from_text_with_overlay(&l.text, &search_ranges, theme.match_style)
                    }
                } else {
                    display_line(s, l, idx, Some(&filter_ranges), theme, visual)
                }
            })
            .collect(),
        ViewMode::Only => s
            .matches
            .iter()
            .enumerate()
            .skip(top)
            .take(height)
            .map(|(pos, &seq)| {
                let idx = usize::try_from(seq - s.base_seq).expect("seq within buffer");
                display_line(s, &s.buffer[idx], pos, None, theme, visual)
            })
            .collect(),
    }
}

fn display_line<'a>(
    s: &TabState,
    line: &'a crate::tab::Line,
    view_pos: usize,
    filter_ranges: Option<&[Range<usize>]>,
    theme: &Theme,
    visual: Option<&VisualSelection>,
) -> UiLine<'a> {
    if let Some(range) = visual.and_then(|sel| sel.selected_range_for_text(view_pos, &line.text)) {
        return highlighted_line_with_style(
            line,
            std::slice::from_ref(&range),
            theme.selection_style,
        );
    }
    let search_ranges = search::ranges(&line.text, &s.search.query);
    if !search_ranges.is_empty() {
        return highlighted_line_with_style(line, &search_ranges, theme.match_style);
    }
    if let Some(ranges) = filter_ranges.filter(|ranges| !ranges.is_empty()) {
        highlighted_line(line, ranges, theme)
    } else {
        content_line(line)
    }
}

fn line_from_text_with_overlay<'a>(
    text: &'a str,
    ranges: &[Range<usize>],
    style: ratatui::style::Style,
) -> UiLine<'a> {
    UiLine::from(overlay_ranges(
        vec![ansi::Segment {
            text: text.to_owned(),
            style: ratatui::style::Style::new(),
        }],
        ranges,
        style,
    ))
}

/// A line in its original form: embedded ANSI colors re-rendered, plain
/// lines untouched (cheap path).
fn content_line(l: &crate::tab::Line) -> UiLine<'_> {
    match &l.raw {
        Some(raw) => UiLine::from(
            ansi::segments(raw.as_bytes())
                .into_iter()
                .map(|seg| Span::styled(seg.text, seg.style))
                .collect::<Vec<_>>(),
        ),
        None => UiLine::raw(l.text.as_ref()),
    }
}

/// Highlight mode, non-matching: everything dimmed, including the line's own
/// colors.
fn dimmed_line<'a>(l: &'a crate::tab::Line, theme: &Theme) -> UiLine<'a> {
    match &l.raw {
        Some(raw) => UiLine::from(
            ansi::segments(raw.as_bytes())
                .into_iter()
                .map(|seg| Span::styled(seg.text, seg.style.patch(theme.dim)))
                .collect::<Vec<_>>(),
        ),
        None => UiLine::styled(l.text.as_ref(), theme.dim),
    }
}

/// Highlight mode, matching: original colors kept, match ranges overlaid
/// with the theme's match style (yellow + bold).
fn highlighted_line<'a>(
    l: &'a crate::tab::Line,
    ranges: &[Range<usize>],
    theme: &Theme,
) -> UiLine<'a> {
    highlighted_line_with_style(l, ranges, theme.match_style)
}

fn highlighted_line_with_style<'a>(
    l: &'a crate::tab::Line,
    ranges: &[Range<usize>],
    overlay: ratatui::style::Style,
) -> UiLine<'a> {
    let segments = match &l.raw {
        Some(raw) => ansi::segments(raw.as_bytes()),
        None => vec![ansi::Segment {
            text: l.text.to_string(),
            style: ratatui::style::Style::new(),
        }],
    };
    UiLine::from(overlay_ranges(segments, ranges, overlay))
}

/// Hard-wrap one styled line into rows of at most `width` display columns,
/// preserving span styles across the split points. Char-accurate via
/// `unicode-width` (a double-width char never straddles a row boundary).
/// The line-level style (e.g. Highlight-mode dimming) is carried onto every row.
fn wrap_line(line: UiLine<'_>, width: usize) -> Vec<UiLine<'_>> {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return vec![line];
    }
    let line_style = line.style;
    let mut rows = Vec::new();
    let mut cur: Vec<Span> = Vec::new();
    let mut cur_w = 0usize;
    for span in line.spans {
        let content = span.content;
        let style = span.style;
        let mut chunk_start = 0usize;
        for (idx, ch) in content.char_indices() {
            let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + ch_w > width && cur_w > 0 {
                if idx > chunk_start {
                    cur.push(Span::styled(content[chunk_start..idx].to_owned(), style));
                }
                rows.push(UiLine::from(std::mem::take(&mut cur)).style(line_style));
                cur_w = 0;
                chunk_start = idx;
            }
            cur_w += ch_w;
        }
        if chunk_start < content.len() {
            cur.push(Span::styled(content[chunk_start..].to_owned(), style));
        }
    }
    rows.push(UiLine::from(cur).style(line_style));
    rows
}

/// Split styled segments at match-range boundaries and patch the overlay
/// style inside the ranges. `ranges` are ascending, non-overlapping byte
/// offsets into the concatenated segment text (= the line's stripped text,
/// which is what the filter matched — so offsets align by construction).
fn overlay_ranges(
    segments: Vec<ansi::Segment>,
    ranges: &[Range<usize>],
    overlay: ratatui::style::Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut seg_start = 0usize;
    let mut r = 0usize;
    for seg in segments {
        let seg_end = seg_start + seg.text.len();
        let mut pos = 0usize;
        while pos < seg.text.len() {
            let abs = seg_start + pos;
            while r < ranges.len() && ranges[r].end <= abs {
                r += 1;
            }
            let (cut, in_match) = match ranges.get(r) {
                Some(range) if range.start <= abs => (range.end.min(seg_end) - seg_start, true),
                Some(range) if range.start < seg_end => (range.start - seg_start, false),
                _ => (seg.text.len(), false),
            };
            let style = if in_match {
                seg.style.patch(overlay)
            } else {
                seg.style
            };
            spans.push(Span::styled(seg.text[pos..cut].to_owned(), style));
            pos = cut;
        }
        seg_start = seg_end;
    }
    spans
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::app::{AppEvent, OpenPathPrompt, PromptEditor};
    use crate::tab::Line;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::style::{Color, Modifier, Style};
    use tokio::sync::mpsc;

    fn seg(text: &str, style: Style) -> ansi::Segment {
        ansi::Segment {
            text: text.into(),
            style,
        }
    }

    #[test]
    fn overlay_within_one_segment() {
        let spans = overlay_ranges(
            vec![seg("hello world", Style::new())],
            std::slice::from_ref(&(6..11)),
            Style::new().fg(Color::Yellow),
        );
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["hello ", "world"]);
        assert_eq!(spans[1].style.fg, Some(Color::Yellow));
        assert_eq!(spans[0].style.fg, None);
    }

    #[test]
    fn overlay_across_segment_boundary_keeps_underlying_colors() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        // text = "ERRor here", match range covers "ERRor" (0..5)
        let spans = overlay_ranges(
            vec![seg("ERR", red), seg("or here", blue)],
            std::slice::from_ref(&(0..5)),
            Style::new().add_modifier(Modifier::BOLD),
        );
        let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts, ["ERR", "or", " here"]);
        // overlay adds BOLD but the segments' own colors survive
        assert_eq!(spans[0].style.fg, Some(Color::Red));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].style.fg, Some(Color::Blue));
        assert!(spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert!(!spans[2].style.add_modifier.contains(Modifier::BOLD));
    }

    fn row_texts(rows: &[UiLine]) -> Vec<String> {
        rows.iter()
            .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wrap_line_splits_at_width_and_keeps_styles() {
        let red = Style::new().fg(Color::Red);
        let line = UiLine::from(vec![Span::raw("abcd"), Span::styled("EFGHIJ", red)]);
        let rows = wrap_line(line, 5);
        assert_eq!(row_texts(&rows), ["abcdE", "FGHIJ"]);
        // the styled span keeps its color on both sides of the split
        assert_eq!(rows[0].spans[1].style.fg, Some(Color::Red));
        assert_eq!(rows[1].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn wrap_line_never_splits_a_wide_char() {
        // width 3: "ab" (2 cols) + 漢 (2 cols) doesn't fit -> 漢 starts row 2
        let rows = wrap_line(UiLine::raw("ab漢x"), 3);
        assert_eq!(row_texts(&rows), ["ab", "漢x"]);
    }

    #[test]
    fn wrap_line_carries_line_style_onto_every_row() {
        // Highlight-mode dimming of plain lines is a line-level style.
        let dim = Style::new().add_modifier(Modifier::DIM);
        let rows = wrap_line(UiLine::styled("abcdefghij", dim), 4);
        assert_eq!(row_texts(&rows), ["abcd", "efgh", "ij"]);
        assert!(rows.iter().all(|r| r.style == dim));
    }

    #[test]
    fn wrap_line_short_and_empty_lines_are_single_rows() {
        assert_eq!(row_texts(&wrap_line(UiLine::raw("hi"), 10)), ["hi"]);
        assert_eq!(row_texts(&wrap_line(UiLine::raw(""), 10)), [""]);
    }

    #[test]
    fn overlay_with_no_ranges_is_identity() {
        let spans = overlay_ranges(vec![seg("abc", Style::new())], &[], Style::new());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "abc");
    }

    /// Build a real App with one tab on a temp file and some pushed lines.
    async fn test_app(theme: Theme) -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");
        std::fs::write(&path, "").unwrap();
        let (tx, _rx) = mpsc::channel::<AppEvent>(8);
        let handles = crate::watcher::spawn_tab(path.clone(), 0, tx, 100).unwrap();
        let mut app = App::new(100, theme);
        app.tabs.push(crate::app::Tab {
            state: crate::tab::TabState::new(0, path, 100),
            handles,
        });
        app.tabs[0].state.push_batch(vec![
            Line::from_bytes(b"plain line"),
            Line::from_bytes(b"\x1b[31mcolored ERROR line\x1b[0m"),
        ]);
        (app, dir)
    }

    fn draw_to_buffer(app: &App) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(60, 10)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        term.backend().buffer().clone()
    }

    #[tokio::test]
    async fn prompt_cursor_uses_editor_position_and_display_width() {
        let (mut app, _dir) = test_app(Theme::utf8()).await;
        app.input = InputMode::FilterEdit(PromptEditor::with_cursor(
            String::from("a漢bc"),
            "a漢".len(),
        ));

        let mut term = Terminal::new(TestBackend::new(60, 10)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();

        // The prompt row is y=9 in a 10-row frame. "filter: " is 8 cells,
        // and "a漢" is 3 cells, so the cursor lands at x=11.
        term.backend_mut()
            .assert_cursor_position(Position::new(11, 9));
    }

    #[tokio::test]
    async fn open_path_prompt_renders_dimmed_ghost_without_moving_cursor() {
        let (mut app, _dir) = test_app(Theme::ascii()).await;
        let mut prompt = OpenPathPrompt::with_editor(PromptEditor::new(String::from("lo")));
        prompt.set_ghost_suffix(String::from("gs/"));
        app.input = InputMode::OpenPath(prompt);

        let mut term = Terminal::new(TestBackend::new(60, 10)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        term.backend_mut()
            .assert_cursor_position(Position::new(13, 9));
        let buf = term.backend().buffer();
        assert_eq!(buf[(13, 9)].symbol(), "g");
        assert!(buf[(13, 9)].style().add_modifier.contains(Modifier::DIM));
        assert_eq!(buf[(15, 9)].symbol(), "/");
        assert!(buf[(15, 9)].style().add_modifier.contains(Modifier::DIM));
    }

    #[tokio::test]
    async fn filter_prompt_does_not_render_open_path_ghost_text() {
        let (mut app, _dir) = test_app(Theme::ascii()).await;
        app.input = InputMode::FilterEdit(PromptEditor::new(String::from("lo")));

        let buf = draw_to_buffer(&app);
        let prompt_row: String = (0..60).map(|x| buf[(x, 9)].symbol()).collect();

        assert!(prompt_row.contains("filter: lo"));
        assert!(!prompt_row.contains("logs/"));
    }

    #[tokio::test]
    async fn ascii_mode_renders_only_ascii_cells() {
        let (app, _dir) = test_app(Theme::ascii()).await;
        let buf = draw_to_buffer(&app);
        for cell in buf.content() {
            assert!(
                cell.symbol().is_ascii(),
                "non-ASCII cell {:?} in --ascii mode",
                cell.symbol()
            );
        }
    }

    #[tokio::test]
    async fn no_control_chars_reach_cells_and_tabs_render_expanded() {
        let (mut app, _dir) = test_app(Theme::utf8()).await;
        app.tabs[0]
            .state
            .push_batch(vec![Line::from_bytes(b"x\ty")]);
        let buf = draw_to_buffer(&app);
        for cell in buf.content() {
            assert!(
                cell.symbol().chars().all(|c| !c.is_control()),
                "control char in cell {:?} — would desync the terminal",
                cell.symbol()
            );
        }
        let area = *buf.area();
        let rows: Vec<String> = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("x       y")),
            "tab was not expanded to a column stop: {rows:?}"
        );
    }

    #[tokio::test]
    async fn wrap_follow_shows_line_tail_and_hscroll_shifts_content() {
        let (mut app, _dir) = test_app(Theme::ascii()).await;
        // 120-char line vs 58-col inner width (60 - borders)
        let long: String = (0..12).map(|i| format!("seg{i:02}-----")).collect();
        app.tabs[0]
            .state
            .push_batch(vec![Line::from_bytes(long.as_bytes())]);

        // Truncated by default: the tail is invisible.
        let all: String = draw_to_buffer(&app)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!all.contains("seg11"), "long line should be truncated");

        // Wrap + follow: the END of the line is on screen.
        app.tabs[0].state.wrap = true;
        let all: String = draw_to_buffer(&app)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(all.contains("seg11"), "wrap+follow must show the line tail");

        // Horizontal scroll: content shifts left by 10 columns.
        app.tabs[0].state.wrap = false;
        app.tabs[0].state.h_scroll = 10;
        let all: String = draw_to_buffer(&app)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            all.contains("seg01-----seg02"),
            "scrolled view should start at seg01"
        );
        assert!(
            !all.contains("seg00"),
            "columns left of the offset must be gone"
        );
    }

    #[tokio::test]
    async fn utf8_mode_has_rounded_corners_and_apple_green_chrome() {
        let (app, _dir) = test_app(Theme::utf8()).await;
        let buf = draw_to_buffer(&app);
        let all: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(all.contains('╭') && all.contains('╰'), "no rounded corners");
        assert!(
            buf.content()
                .iter()
                .any(|c| c.style().fg == Some(crate::theme::APPLE_GREEN)),
            "no apple-green chrome cell found"
        );
    }

    #[tokio::test]
    async fn colored_content_renders_and_dims_in_highlight_mode() {
        let (mut app, _dir) = test_app(Theme::utf8()).await;
        // All mode: the colored line's red survives.
        let buf = draw_to_buffer(&app);
        assert!(
            buf.content()
                .iter()
                .any(|c| c.style().fg == Some(Color::Red)),
            "embedded ANSI color was not rendered"
        );

        // Highlight mode with a filter matching only the plain line: the
        // colored line must be dimmed (DIM added, red kept).
        let state = &mut app.tabs[0].state;
        state.filter.compiled = Some(std::sync::Arc::new(
            crate::filter::CompiledFilter::compile(&crate::filter::FilterSpec {
                raw: "plain".into(),
                is_regex: false,
                case_insensitive: false,
            })
            .unwrap(),
        ));
        state.buffer[0].matched = true;
        state.mode = ViewMode::Highlight;
        let buf = draw_to_buffer(&app);
        assert!(
            buf.content()
                .iter()
                .any(|c| c.style().fg == Some(Color::Red)
                    && c.style().add_modifier.contains(Modifier::DIM)),
            "colored non-match was not dimmed"
        );
        // And the matched span carries the yellow+bold overlay.
        assert!(
            buf.content()
                .iter()
                .any(|c| c.style().fg == Some(Color::Yellow)
                    && c.style().add_modifier.contains(Modifier::BOLD)),
            "match overlay missing"
        );
    }

    #[tokio::test]
    async fn search_highlights_render_in_viewport() {
        let (mut app, _dir) = test_app(Theme::ascii()).await;
        app.tabs[0].state.search.query = String::from("error");

        let buf = draw_to_buffer(&app);

        assert!(
            buf.content()
                .iter()
                .any(|c| c.symbol().eq_ignore_ascii_case("e")
                    && c.style().fg == Some(Color::Yellow)
                    && c.style().add_modifier.contains(Modifier::BOLD)),
            "search match overlay missing"
        );
    }

    #[tokio::test]
    async fn visual_selection_renders_over_selected_text() {
        let (mut app, _dir) = test_app(Theme::ascii()).await;
        let selection = crate::selection::VisualSelection::start(
            &app.tabs[0].state,
            5,
            crate::selection::SelectionKind::Line,
        );
        app.input = InputMode::VisualSelection(selection);

        let buf = draw_to_buffer(&app);

        assert!(
            buf.content()
                .iter()
                .any(|c| c.symbol() == "p" && c.style().bg == Some(Color::Yellow)),
            "selection overlay missing"
        );
    }

    #[tokio::test]
    async fn help_overlay_renders_above_viewport() {
        let (mut app, _dir) = test_app(Theme::ascii()).await;
        app.input = InputMode::HelpOverlay;

        let mut term = Terminal::new(TestBackend::new(90, 30)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let buf = term.backend().buffer();
        let all: String = buf.content().iter().map(|c| c.symbol()).collect();

        assert!(all.contains("help"));
        assert!(all.contains("Normal"));
        assert!(all.contains("Search"));
    }
}
