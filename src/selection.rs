use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use crate::tab::TabState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    Character,
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextPoint {
    pub line: usize,
    pub col: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualSelection {
    pub kind: SelectionKind,
    pub anchor: TextPoint,
    pub cursor: TextPoint,
}

impl VisualSelection {
    pub fn start(tab: &TabState, height: usize, kind: SelectionKind) -> Self {
        let line = tab
            .top_position(height)
            .min(tab.view_len().saturating_sub(1));
        let col = tab
            .view_line(line)
            .map_or(0, |l| byte_for_display_col(&l.text, tab.h_scroll as usize));
        let point = TextPoint { line, col };
        VisualSelection {
            kind,
            anchor: point,
            cursor: point,
        }
    }

    pub fn selected_range_for_text(&self, line: usize, text: &str) -> Option<Range<usize>> {
        let (start, end) = ordered_points(self.anchor, self.cursor);
        if line < start.line || line > end.line {
            return None;
        }
        match self.kind {
            SelectionKind::Line => Some(0..text.len()),
            SelectionKind::Character => {
                let range = if start.line == end.line {
                    clamp_boundary_in_text(text, start.col)..selected_end(text, end.col)
                } else if line == start.line {
                    clamp_boundary_in_text(text, start.col)..text.len()
                } else if line == end.line {
                    0..selected_end(text, end.col)
                } else {
                    0..text.len()
                };
                (range.start < range.end || text.is_empty()).then_some(range)
            }
        }
    }

    pub fn extract_text(&self, tab: &TabState) -> String {
        let (start, end) = ordered_points(self.anchor, self.cursor);
        let mut lines = Vec::new();
        for line_idx in start.line..=end.line {
            let Some(line) = tab.view_line(line_idx) else {
                continue;
            };
            let text = line.text.as_ref();
            match self.kind {
                SelectionKind::Line => lines.push(text.to_owned()),
                SelectionKind::Character => {
                    let range = self.selected_range_for_text(line_idx, text).unwrap_or(0..0);
                    lines.push(text[range].to_owned());
                }
            }
        }
        lines.join("\n")
    }

    pub fn selected_line_count(&self) -> usize {
        let (start, end) = ordered_points(self.anchor, self.cursor);
        end.line.saturating_sub(start.line) + 1
    }

    pub fn move_left(&mut self, tab: &TabState) {
        let Some(line) = tab.view_line(self.cursor.line) else {
            return;
        };
        self.cursor.col = previous_boundary(&line.text, self.cursor.col);
    }

    pub fn move_right(&mut self, tab: &TabState) {
        let Some(line) = tab.view_line(self.cursor.line) else {
            return;
        };
        self.cursor.col = next_boundary(&line.text, self.cursor.col);
    }

    pub fn move_down(&mut self, tab: &TabState) {
        let len = tab.view_len();
        if len == 0 {
            return;
        }
        self.cursor.line = (self.cursor.line + 1).min(len - 1);
        self.clamp_col(tab);
    }

    pub fn move_up(&mut self, tab: &TabState) {
        self.cursor.line = self.cursor.line.saturating_sub(1);
        self.clamp_col(tab);
    }

    pub fn move_next_word(&mut self, tab: &TabState) {
        self.move_word(tab, WordMotion::NextStart);
    }

    pub fn move_previous_word(&mut self, tab: &TabState) {
        self.move_word(tab, WordMotion::PreviousStart);
    }

    pub fn move_word_end(&mut self, tab: &TabState) {
        self.move_word(tab, WordMotion::End);
    }

    fn move_word(&mut self, tab: &TabState, motion: WordMotion) {
        let Some(line) = tab.view_line(self.cursor.line) else {
            return;
        };
        self.cursor.col = match motion {
            WordMotion::NextStart => next_word_start(&line.text, self.cursor.col),
            WordMotion::PreviousStart => previous_word_start(&line.text, self.cursor.col),
            WordMotion::End => word_end(&line.text, self.cursor.col),
        };
    }

    fn clamp_col(&mut self, tab: &TabState) {
        if let Some(line) = tab.view_line(self.cursor.line) {
            self.cursor.col = clamp_boundary_in_text(&line.text, self.cursor.col);
        }
    }
}

#[derive(Clone, Copy)]
enum WordMotion {
    NextStart,
    PreviousStart,
    End,
}

fn ordered_points(a: TextPoint, b: TextPoint) -> (TextPoint, TextPoint) {
    if (a.line, a.col) <= (b.line, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

fn next_boundary(text: &str, col: usize) -> usize {
    let col = clamp_boundary_in_text(text, col);
    if col >= text.len() {
        return text.len();
    }
    col + text[col..].chars().next().map_or(0, |ch| ch.len_utf8())
}

fn selected_end(text: &str, col: usize) -> usize {
    next_boundary(text, col)
}

fn previous_boundary(text: &str, col: usize) -> usize {
    let col = clamp_boundary_in_text(text, col);
    if col == 0 {
        return 0;
    }
    text[..col].char_indices().last().map_or(0, |(idx, _)| idx)
}

fn clamp_boundary_in_text(text: &str, mut col: usize) -> usize {
    col = col.min(text.len());
    while col > 0 && col < text.len() && !text.is_char_boundary(col) {
        col -= 1;
    }
    col
}

fn next_word_start(text: &str, col: usize) -> usize {
    let mut seen_word = false;
    for (idx, ch) in text[clamp_boundary_in_text(text, col)..].char_indices() {
        let abs = clamp_boundary_in_text(text, col) + idx;
        if is_word(ch) {
            if !seen_word && abs > col {
                return abs;
            }
            seen_word = true;
        } else if seen_word {
            seen_word = false;
        }
    }
    text.len()
}

fn previous_word_start(text: &str, col: usize) -> usize {
    let col = clamp_boundary_in_text(text, col);
    let mut in_word = false;
    let mut last_start = 0usize;
    for (idx, ch) in text[..col].char_indices() {
        if is_word(ch) {
            if !in_word {
                last_start = idx;
                in_word = true;
            }
        } else {
            in_word = false;
        }
    }
    last_start
}

fn word_end(text: &str, col: usize) -> usize {
    let col = clamp_boundary_in_text(text, col);
    let mut in_word = false;
    for (idx, ch) in text[col..].char_indices() {
        let abs = col + idx;
        if is_word(ch) {
            in_word = true;
        } else if in_word {
            return previous_boundary(text, abs);
        }
    }
    previous_boundary(text, text.len())
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn byte_for_display_col(text: &str, target_col: usize) -> usize {
    let mut width = 0usize;
    for (idx, ch) in text.char_indices() {
        if width >= target_col {
            return idx;
        }
        width += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tab::{Line, TabState};
    use std::path::PathBuf;

    fn tab() -> TabState {
        let mut tab = TabState::new(0, PathBuf::from("x"), 100);
        tab.push_batch(vec![
            Line::from_bytes(b"alpha one"),
            Line::from_bytes(b"beta two"),
            Line::from_bytes(b"gamma three"),
        ]);
        tab.follow = false;
        tab.anchor = Some(0);
        tab
    }

    #[test]
    fn character_selection_extracts_word_and_multiline_text() {
        let tab = tab();
        let mut sel = VisualSelection::start(&tab, 5, SelectionKind::Character);
        sel.move_word_end(&tab);
        assert_eq!(sel.extract_text(&tab), "alpha");

        sel.move_down(&tab);
        sel.move_right(&tab);
        assert_eq!(sel.extract_text(&tab), "alpha one\nbeta t");
    }

    #[test]
    fn line_selection_extracts_full_lines() {
        let tab = tab();
        let mut sel = VisualSelection::start(&tab, 5, SelectionKind::Line);
        sel.move_down(&tab);
        assert_eq!(sel.extract_text(&tab), "alpha one\nbeta two");
        assert_eq!(sel.selected_line_count(), 2);
    }

    #[test]
    fn word_motions_move_between_words() {
        let tab = tab();
        let mut sel = VisualSelection::start(&tab, 5, SelectionKind::Character);
        sel.move_next_word(&tab);
        assert_eq!(sel.cursor.col, 6);
        sel.move_word_end(&tab);
        assert_eq!(sel.cursor.col, 8);
        sel.move_previous_word(&tab);
        assert_eq!(sel.cursor.col, 6);
    }
}
