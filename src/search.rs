use std::ops::Range;

use crate::tab::TabState;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub current: Option<SearchMatch>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub line: usize,
    pub range: Range<usize>,
}

impl SearchState {
    pub fn clear(&mut self) {
        self.query.clear();
        self.current = None;
    }

    pub fn apply(&mut self, tab: &TabState, query: String, height: usize) -> bool {
        self.query = query;
        if self.query.is_empty() {
            self.current = None;
            return false;
        }
        self.current = find_next_from(tab, &self.query, tab.top_position(height), 0);
        self.current.is_some()
    }

    pub fn next(&mut self, tab: &TabState, height: usize) -> bool {
        if self.query.is_empty() {
            return false;
        }
        let (line, col) = self
            .current
            .as_ref()
            .map_or((tab.top_position(height), 0), |m| (m.line, m.range.end));
        self.current = find_next_from(tab, &self.query, line, col);
        self.current.is_some()
    }

    pub fn previous(&mut self, tab: &TabState, height: usize) -> bool {
        if self.query.is_empty() {
            return false;
        }
        let (line, col) = self
            .current
            .as_ref()
            .map_or((tab.top_position(height), usize::MAX), |m| {
                (m.line, m.range.start)
            });
        self.current = find_previous_from(tab, &self.query, line, col);
        self.current.is_some()
    }
}

pub fn ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let (lower_text, lower_to_original) = lower_with_byte_map(text);
    let lower_query = query.to_lowercase();
    if lower_query.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut start = 0usize;
    while let Some(rel) = lower_text[start..].find(&lower_query) {
        let lower_start = start + rel;
        let lower_end = lower_start + lower_query.len();
        let orig_start = lower_to_original[lower_start];
        let orig_end = lower_to_original[lower_end];
        if orig_start < orig_end {
            out.push(orig_start..orig_end);
        }
        start = lower_end.max(lower_start + 1);
    }
    out
}

fn find_next_from(
    tab: &TabState,
    query: &str,
    start_line: usize,
    start_col: usize,
) -> Option<SearchMatch> {
    let len = tab.view_len();
    if len == 0 {
        return None;
    }
    let start_line = start_line.min(len - 1);
    for offset in 0..len {
        let line = (start_line + offset) % len;
        let min_col = if offset == 0 { start_col } else { 0 };
        let text = tab.view_line(line)?.text.as_ref();
        if let Some(range) = ranges(text, query).into_iter().find(|r| r.start >= min_col) {
            return Some(SearchMatch { line, range });
        }
    }
    None
}

fn find_previous_from(
    tab: &TabState,
    query: &str,
    start_line: usize,
    start_col: usize,
) -> Option<SearchMatch> {
    let len = tab.view_len();
    if len == 0 {
        return None;
    }
    let start_line = start_line.min(len - 1);
    for offset in 0..len {
        let line = (start_line + len - offset) % len;
        let max_col = if offset == 0 { start_col } else { usize::MAX };
        let text = tab.view_line(line)?.text.as_ref();
        if let Some(range) = ranges(text, query)
            .into_iter()
            .rev()
            .find(|r| r.start < max_col)
        {
            return Some(SearchMatch { line, range });
        }
    }
    None
}

fn lower_with_byte_map(text: &str) -> (String, Vec<usize>) {
    let mut lower = String::new();
    let mut map = Vec::new();
    for (idx, ch) in text.char_indices() {
        for lower_ch in ch.to_lowercase() {
            let mut buf = [0; 4];
            let s = lower_ch.encode_utf8(&mut buf);
            lower.push_str(s);
            map.extend(std::iter::repeat_n(idx, s.len()));
        }
    }
    map.push(text.len());
    (lower, map)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tab::{Line, TabState, ViewMode};
    use std::path::PathBuf;

    fn tab() -> TabState {
        let mut tab = TabState::new(0, PathBuf::from("x"), 100);
        let mut lines = vec![
            Line::from_bytes(b"alpha one"),
            Line::from_bytes(b"Beta two"),
            Line::from_bytes(b"literal .* token"),
            Line::from_bytes(b"gamma beta"),
        ];
        lines[1].matched = true;
        lines[3].matched = true;
        tab.push_batch(lines);
        tab
    }

    #[test]
    fn ranges_are_literal_and_case_insensitive() {
        assert_eq!(ranges("One one .*", "ONE"), [0..3, 4..7]);
        assert_eq!(
            ranges("literal .* token", ".*"),
            std::iter::once(8..10).collect::<Vec<_>>()
        );
    }

    #[test]
    fn search_moves_in_all_highlight_and_only_modes() {
        let mut tab = tab();
        let mut search = SearchState::default();
        assert!(search.apply(&tab, String::from("beta"), 3));
        assert_eq!(search.current.as_ref().unwrap().line, 1);
        assert!(search.next(&tab, 3));
        assert_eq!(search.current.as_ref().unwrap().line, 3);
        assert!(search.previous(&tab, 3));
        assert_eq!(search.current.as_ref().unwrap().line, 1);

        tab.mode = ViewMode::Highlight;
        assert!(search.apply(&tab, String::from("beta"), 3));
        assert_eq!(search.current.as_ref().unwrap().line, 1);

        tab.mode = ViewMode::Only;
        assert!(search.apply(&tab, String::from("beta"), 3));
        assert_eq!(search.current.as_ref().unwrap().line, 0);
        assert!(search.next(&tab, 3));
        assert_eq!(search.current.as_ref().unwrap().line, 1);
    }
}
