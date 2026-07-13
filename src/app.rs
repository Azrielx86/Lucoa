use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio::time::{Duration, MissedTickBehavior, interval};

use crate::export;
use crate::filter::{self, CompiledFilter, FilterSpec};
use crate::history::{HistoryStore, fuzzy_matches};
use crate::selection::{SelectionKind, VisualSelection};
use crate::tab::{ExportState, FilterState, Line, TabConfig, TabId, TabState};
use crate::theme::Theme;
use crate::ui;
use crate::watcher::{self, TabHandles};

pub enum AppEvent {
    Batch {
        tab: TabId,
        lines: Vec<Line>,
        /// Filter generation the reader matched these lines under.
        filter_gen: u64,
    },
    /// Pre-existing tail of the file, from the backward scan at open. Kept
    /// separate from `Batch` so old content never trips the unseen marker.
    InitialScan {
        tab: TabId,
        lines: Vec<Line>,
    },
    RecomputeDone {
        tab: TabId,
        generation: u64,
        snap_base: u64,
        matched_flags: Vec<bool>,
    },
    FileError {
        tab: TabId,
        msg: String,
    },
}

pub struct Tab {
    pub state: TabState,
    pub handles: TabHandles,
}

pub enum InputMode {
    Normal,
    /// Typing a path for a new tab.
    OpenPath(OpenPathPrompt),
    /// Editing the active tab's filter string.
    FilterEdit(PromptEditor),
    /// Literal case-insensitive search in the active view.
    SearchPrompt(PromptEditor),
    /// Typing the output path to start exporting matched lines.
    ExportPath(PromptEditor),
    VisualSelection(VisualSelection),
    HelpOverlay,
}

#[derive(Default)]
pub struct OpenPathPrompt {
    editor: PromptEditor,
    completion: PathCompletionState,
}

impl OpenPathPrompt {
    pub fn editor(&self) -> &PromptEditor {
        &self.editor
    }

    pub fn ghost_suffix(&self) -> Option<&str> {
        self.completion.ghost_suffix.as_deref()
    }

    fn editor_mut(&mut self) -> &mut PromptEditor {
        &mut self.editor
    }

    fn complete(&mut self, history: &[PathBuf]) {
        match complete_path(self.editor.text(), self.editor.cursor(), history) {
            Ok(outcome) => {
                if let Some(before_cursor) = outcome.before_cursor {
                    self.editor.replace_before_cursor(&before_cursor);
                }
                self.completion.ghost_suffix = outcome.ghost_suffix;
                self.completion.hint = outcome.hint;
            }
            Err(e) => {
                self.completion.ghost_suffix = None;
                self.completion.hint = Some(e.to_string());
            }
        }
    }

    fn clear_completion(&mut self) -> Option<String> {
        self.completion.ghost_suffix = None;
        self.completion.hint.take()
    }

    fn completion_hint(&self) -> Option<&str> {
        self.completion.hint.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn with_editor(editor: PromptEditor) -> Self {
        Self {
            editor,
            completion: PathCompletionState::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_ghost_suffix(&mut self, suffix: String) {
        self.completion.ghost_suffix = Some(suffix);
    }
}

#[derive(Default)]
struct PathCompletionState {
    ghost_suffix: Option<String>,
    hint: Option<String>,
}

#[derive(Default)]
pub struct PromptEditor {
    text: String,
    cursor: usize,
}

impl PromptEditor {
    pub fn new(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }

    #[cfg(test)]
    pub(crate) fn with_cursor(text: String, cursor: usize) -> Self {
        assert!(text.is_char_boundary(cursor));
        assert!(cursor <= text.len());
        Self { text, cursor }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn into_text(self) -> String {
        self.text
    }

    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .last()
            .map_or(0, |(idx, _)| idx);
    }

    fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let ch = self.text[self.cursor..]
            .chars()
            .next()
            .expect("cursor is on a char boundary before text end");
        self.cursor += ch.len_utf8();
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn insert_str(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn replace_before_cursor(&mut self, replacement: &str) {
        self.text.replace_range(..self.cursor, replacement);
        self.cursor = replacement.len();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .last()
            .map_or(0, |(idx, _)| idx);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }

    fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.cursor
            + self.text[self.cursor..]
                .chars()
                .next()
                .expect("cursor is on a char boundary before text end")
                .len_utf8();
        self.text.drain(self.cursor..next);
    }
}

enum EditResult {
    Pending,
    Cancel,
    Submit(String),
}

struct PathCompletionOutcome {
    before_cursor: Option<String>,
    ghost_suffix: Option<String>,
    hint: Option<String>,
}

struct CompletionCandidate {
    replacement: String,
    display: String,
    history: bool,
}

fn complete_path(
    text: &str,
    cursor: usize,
    history: &[PathBuf],
) -> io::Result<PathCompletionOutcome> {
    let before = &text[..cursor];
    let history_candidates: Vec<CompletionCandidate> = history
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .filter(|path| fuzzy_matches(path, before))
        .map(|path| CompletionCandidate {
            display: path.clone(),
            replacement: path,
            history: true,
        })
        .collect();
    if !history_candidates.is_empty() {
        return Ok(completion_from_candidates(
            before,
            "",
            history_candidates,
            true,
        ));
    }

    let split = split_path_fragment(before);
    let parent = parent_path(split.parent);
    let fragment_starts_hidden = split.fragment.starts_with('.');

    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !fragment_starts_hidden {
            continue;
        }
        if !name.starts_with(split.fragment) {
            continue;
        }
        let mut insert_name = name;
        if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            insert_name.push(std::path::MAIN_SEPARATOR);
        }
        candidates.push(CompletionCandidate {
            replacement: format!("{}{}", split.parent, insert_name),
            display: insert_name,
            history: false,
        });
    }
    candidates.sort_by(|a, b| a.display.cmp(&b.display));
    Ok(completion_from_candidates(
        before,
        split.fragment,
        candidates,
        false,
    ))
}

fn completion_from_candidates(
    before: &str,
    fragment: &str,
    candidates: Vec<CompletionCandidate>,
    fuzzy_history: bool,
) -> PathCompletionOutcome {
    match candidates.as_slice() {
        [] => PathCompletionOutcome {
            before_cursor: None,
            ghost_suffix: None,
            hint: Some(String::from("no matches")),
        },
        [candidate] => PathCompletionOutcome {
            before_cursor: Some(candidate.replacement.clone()),
            ghost_suffix: None,
            hint: None,
        },
        _ => {
            let before_cursor;
            let ghost_suffix;
            if fuzzy_history {
                before_cursor = None;
                ghost_suffix = candidates
                    .first()
                    .and_then(|candidate| candidate.replacement.strip_prefix(before))
                    .filter(|suffix| !suffix.is_empty())
                    .map(str::to_owned);
            } else {
                let common = common_prefix(
                    candidates
                        .iter()
                        .map(|candidate| candidate.display.as_str())
                        .collect::<Vec<_>>()
                        .as_slice(),
                );
                let inserted = common.len() > fragment.len();
                before_cursor = inserted.then(|| {
                    candidates[0]
                        .replacement
                        .strip_suffix(&candidates[0].display)
                        .map_or_else(|| common.clone(), |parent| format!("{parent}{common}"))
                });
                let ghost_base = if inserted { common.as_str() } else { fragment };
                ghost_suffix = candidates
                    .first()
                    .and_then(|candidate| candidate.display.strip_prefix(ghost_base))
                    .filter(|suffix| !suffix.is_empty())
                    .map(str::to_owned);
            }
            PathCompletionOutcome {
                before_cursor,
                ghost_suffix,
                hint: Some(completion_hint(&candidates)),
            }
        }
    }
}

struct PathSplit<'a> {
    parent: &'a str,
    fragment: &'a str,
}

fn split_path_fragment(input: &str) -> PathSplit<'_> {
    match input
        .char_indices()
        .rev()
        .find(|&(_, c)| is_path_separator(c))
    {
        Some((idx, sep)) => {
            let fragment_start = idx + sep.len_utf8();
            PathSplit {
                parent: &input[..fragment_start],
                fragment: &input[fragment_start..],
            }
        }
        None => PathSplit {
            parent: "",
            fragment: input,
        },
    }
}

fn parent_path(parent: &str) -> PathBuf {
    if parent.is_empty() {
        PathBuf::from(".")
    } else {
        let path = Path::new(parent);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    }
}

fn is_path_separator(c: char) -> bool {
    #[cfg(windows)]
    {
        c == '/' || c == '\\'
    }
    #[cfg(not(windows))]
    {
        c == std::path::MAIN_SEPARATOR
    }
}

fn common_prefix(names: &[&str]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix = String::new();
    for ch in first.chars() {
        let next_len = prefix.len() + ch.len_utf8();
        if names
            .iter()
            .all(|name| name[next_len - ch.len_utf8()..].starts_with(ch))
        {
            prefix.push(ch);
        } else {
            break;
        }
    }
    prefix
}

fn completion_hint(candidates: &[CompletionCandidate]) -> String {
    let mut names: Vec<&str> = candidates
        .iter()
        .take(5)
        .map(|candidate| candidate.display.as_str())
        .collect();
    if candidates.len() > names.len() {
        names.push("...");
    }
    let label = if candidates.first().is_some_and(|c| c.history) {
        "history matches"
    } else {
        "matches"
    };
    format!("{} {label}: {}", candidates.len(), names.join(" "))
}

/// Shared line-editing keys for the prompt modes.
fn edit_text(editor: &mut PromptEditor, key: KeyEvent) -> EditResult {
    match key.code {
        KeyCode::Esc => EditResult::Cancel,
        KeyCode::Enter => {
            let editor = std::mem::take(editor);
            EditResult::Submit(editor.into_text())
        }
        KeyCode::Left => {
            editor.move_left();
            EditResult::Pending
        }
        KeyCode::Right => {
            editor.move_right();
            EditResult::Pending
        }
        KeyCode::Home => {
            editor.move_home();
            EditResult::Pending
        }
        KeyCode::End => {
            editor.move_end();
            EditResult::Pending
        }
        KeyCode::Backspace => {
            editor.backspace();
            EditResult::Pending
        }
        KeyCode::Delete => {
            editor.delete();
            EditResult::Pending
        }
        KeyCode::Char(c) if is_text_input(key.modifiers) => {
            editor.insert_char(c);
            EditResult::Pending
        }
        _ => EditResult::Pending,
    }
}

/// Whether a Char key event is text (should be inserted) rather than a
/// shortcut. CTRL+ALT together is AltGr: on many layouts (e.g. Spanish,
/// where `\` is AltGr-typed) Windows synthesizes both typed and *pasted*
/// characters with those modifiers — dropping them ate every backslash of a
/// pasted path (spec/9). Only a bare CTRL or bare ALT means a shortcut.
fn is_text_input(m: KeyModifiers) -> bool {
    let ctrl_alt = KeyModifiers::CONTROL | KeyModifiers::ALT;
    m.contains(ctrl_alt) || !m.intersects(ctrl_alt)
}

/// Turn prompt input into a path: trims whitespace and strips paired
/// surrounding quotes — Windows Explorer's "Copy as path" wraps the path in
/// `"…"`, which `File::open` then can't find (spec/10). `None` if nothing
/// remains.
fn clean_path(text: &str) -> Option<PathBuf> {
    let mut t = text.trim();
    while t.len() >= 2 {
        let b = t.as_bytes();
        let (first, last) = (b[0], b[b.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            t = t[1..t.len() - 1].trim();
        } else {
            break;
        }
    }
    (!t.is_empty()).then(|| PathBuf::from(t))
}

pub struct App {
    pub tabs: Vec<Tab>,
    pub active: usize,
    next_tab_id: TabId,
    event_tx: mpsc::Sender<AppEvent>,
    event_rx: mpsc::Receiver<AppEvent>,
    pub buffer_lines: usize,
    pub status_msg: Option<String>,
    pub input: InputMode,
    pub theme: Theme,
    /// Export writer handles, kept even after their tab closes so quit can
    /// await the final flush of every export ever started.
    export_handles: Vec<tokio::task::JoinHandle<u64>>,
    /// Viewport height from the last render; used for page-scroll distances.
    view_height: usize,
    /// Ctrl-L: clear the backend before the next draw, forcing a full
    /// repaint. Recovery from any terminal-side desync ratatui's cell diff
    /// cannot see (spec/8).
    force_clear: bool,
    dirty: bool,
    should_quit: bool,
    history_store: HistoryStore,
}

enum Step {
    Input(Event),
    App(AppEvent),
    Render,
    Idle,
}

impl App {
    pub fn new(buffer_lines: usize, theme: Theme) -> Self {
        let (event_tx, event_rx) = mpsc::channel(128);
        App {
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 0,
            event_tx,
            event_rx,
            buffer_lines,
            status_msg: None,
            input: InputMode::Normal,
            theme,
            export_handles: Vec::new(),
            view_height: 24,
            force_clear: false,
            dirty: true,
            should_quit: false,
            #[cfg(not(test))]
            history_store: HistoryStore::load_default(),
            #[cfg(test)]
            history_store: HistoryStore::memory(Vec::new()),
        }
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal, files: Vec<PathBuf>) -> Result<()> {
        for path in files {
            self.open_tab(path);
        }
        let mut input = EventStream::new();
        let mut tick = interval(Duration::from_millis(33));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        while !self.should_quit {
            // The `if self.dirty` guard disables the tick branch entirely when
            // there is nothing to draw, so an idle app never wakes up.
            let step = tokio::select! {
                maybe = input.next() => match maybe {
                    Some(Ok(ev)) => Step::Input(ev),
                    _ => Step::Idle,
                },
                maybe = self.event_rx.recv() => match maybe {
                    Some(ev) => Step::App(ev),
                    None => Step::Idle,
                },
                _ = tick.tick(), if self.dirty => Step::Render,
            };
            match step {
                Step::Input(ev) => {
                    self.handle_input(ev);
                    self.dirty = true;
                }
                Step::App(ev) => {
                    self.apply(ev);
                    // Drain a few more queued events to amortize wakeups.
                    for _ in 0..16 {
                        match self.event_rx.try_recv() {
                            Ok(ev) => self.apply(ev),
                            Err(_) => break,
                        }
                    }
                    self.dirty = true;
                }
                Step::Render => {
                    if self.force_clear {
                        terminal.clear()?;
                        self.force_clear = false;
                    }
                    let size = terminal.size()?;
                    self.view_height =
                        size.height.saturating_sub(ui::chrome_rows(&self.input)) as usize;
                    terminal.draw(|f| ui::draw(f, &self))?;
                    self.dirty = false;
                }
                Step::Idle => {}
            }
        }

        // Shutdown: dropping the tabs cascades — watchers stop, readers exit,
        // export senders drop. Awaiting the export handles guarantees every
        // export file got its final flush before the terminal is restored.
        self.tabs.clear();
        let handles: Vec<_> = self.export_handles.drain(..).collect();
        if !handles.is_empty() {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                futures_util::future::join_all(handles),
            )
            .await;
        }
        Ok(())
    }

    fn handle_input(&mut self, ev: Event) {
        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            // The wheel is captured (never scrolls the terminal itself) and
            // drives the same scroll model as the arrow keys.
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => self.scroll(-3),
                MouseEventKind::ScrollDown => self.scroll(3),
                _ => {}
            },
            // Bracketed paste (modern terminals): the whole string at once,
            // no synthesized keystrokes to misinterpret.
            Event::Paste(text) => self.handle_paste(&text),
            _ => {}
        }
    }

    fn handle_paste(&mut self, text: &str) {
        // Single-line prompts: a trailing newline in the clipboard must not
        // smuggle in a submit, and control chars have no business in a path.
        let clean = text.chars().filter(|c| !c.is_control()).collect::<String>();
        match &mut self.input {
            InputMode::Normal | InputMode::VisualSelection(_) | InputMode::HelpOverlay => {}
            InputMode::OpenPath(prompt) => {
                if !clean.is_empty() {
                    prompt.editor_mut().insert_str(&clean);
                    let old = prompt.clear_completion();
                    clear_completion_status(&mut self.status_msg, old.as_deref());
                }
            }
            InputMode::FilterEdit(editor)
            | InputMode::SearchPrompt(editor)
            | InputMode::ExportPath(editor) => {
                editor.insert_str(&clean);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Ctrl-C quits, Ctrl-L forces a full repaint — from any mode. AltGr
        // (CTRL+ALT) combos are text, not shortcuts (spec/9).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            match key.code {
                KeyCode::Char('c') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('l') => {
                    self.force_clear = true;
                    return;
                }
                _ => {}
            }
        }
        match &mut self.input {
            InputMode::OpenPath(prompt) => {
                if key.code == KeyCode::Tab {
                    let old = prompt.completion_hint().map(str::to_owned);
                    let history = self.history_store.entries().to_vec();
                    prompt.complete(&history);
                    replace_completion_status(
                        &mut self.status_msg,
                        old.as_deref(),
                        prompt.completion_hint(),
                    );
                    return;
                }

                let before_text = prompt.editor().text().to_owned();
                let before_cursor = prompt.editor().cursor();
                match edit_text(prompt.editor_mut(), key) {
                    EditResult::Pending => {
                        if prompt.editor().text() != before_text
                            || prompt.editor().cursor() != before_cursor
                        {
                            let old = prompt.clear_completion();
                            clear_completion_status(&mut self.status_msg, old.as_deref());
                        }
                    }
                    EditResult::Cancel => self.input = InputMode::Normal,
                    EditResult::Submit(text) => {
                        self.input = InputMode::Normal;
                        if let Some(path) = clean_path(&text) {
                            self.open_tab(path);
                        }
                    }
                }
            }
            InputMode::FilterEdit(editor) => match edit_text(editor, key) {
                EditResult::Pending => {}
                EditResult::Cancel => self.input = InputMode::Normal,
                EditResult::Submit(text) => {
                    self.input = InputMode::Normal;
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.state.filter.raw = text;
                        self.apply_filter_change();
                    }
                }
            },
            InputMode::SearchPrompt(editor) => match edit_text(editor, key) {
                EditResult::Pending => {}
                EditResult::Cancel => self.input = InputMode::Normal,
                EditResult::Submit(text) => {
                    self.input = InputMode::Normal;
                    self.apply_search_query(text);
                }
            },
            InputMode::ExportPath(editor) => match edit_text(editor, key) {
                EditResult::Pending => {}
                EditResult::Cancel => self.input = InputMode::Normal,
                EditResult::Submit(text) => {
                    self.input = InputMode::Normal;
                    if let Some(path) = clean_path(&text) {
                        self.start_export(path);
                    }
                }
            },
            InputMode::VisualSelection(selection) => match key.code {
                KeyCode::Esc => self.input = InputMode::Normal,
                KeyCode::Char('h') | KeyCode::Left => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        selection.move_left(&tab.state);
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        selection.move_down(&tab.state);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        selection.move_up(&tab.state);
                    }
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        selection.move_right(&tab.state);
                    }
                }
                KeyCode::Char('w') => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        selection.move_next_word(&tab.state);
                    }
                }
                KeyCode::Char('b') => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        selection.move_previous_word(&tab.state);
                    }
                }
                KeyCode::Char('e') => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        selection.move_word_end(&tab.state);
                    }
                }
                KeyCode::Char('y') => self.copy_visual_selection(),
                _ => {}
            },
            InputMode::HelpOverlay => match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                    self.input = InputMode::Normal;
                }
                _ => {}
            },
            InputMode::Normal => match key.code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Char('o') => self.input = InputMode::OpenPath(OpenPathPrompt::default()),
                KeyCode::Char('x') => self.close_tab(),
                KeyCode::Tab => self.switch_tab(1),
                KeyCode::BackTab => self.switch_tab(-1),
                KeyCode::Char(c @ '1'..='9') => {
                    self.switch_to_tab(c.to_digit(10).expect("digit") as usize - 1);
                }
                KeyCode::Char('/') => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        self.input =
                            InputMode::FilterEdit(PromptEditor::new(tab.state.filter.raw.clone()));
                    }
                }
                KeyCode::Char('s') => {
                    if let Some(tab) = self.tabs.get(self.active) {
                        self.input = InputMode::SearchPrompt(PromptEditor::new(
                            tab.state.search.query.clone(),
                        ));
                    }
                }
                KeyCode::Char('n') => self.search_next(),
                KeyCode::Char('p') => self.search_previous(),
                KeyCode::Char('v') => self.start_visual_selection(SelectionKind::Character),
                KeyCode::Char('V') => self.start_visual_selection(SelectionKind::Line),
                KeyCode::Char('?') => self.input = InputMode::HelpOverlay,
                KeyCode::Char('r') => self.toggle_filter(|f| f.is_regex = !f.is_regex),
                KeyCode::Char('i') => {
                    self.toggle_filter(|f| f.case_insensitive = !f.case_insensitive);
                }
                KeyCode::Char('m') => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.state.mode = tab.state.mode.next();
                    }
                }
                KeyCode::Char('e') => self.toggle_export(),
                KeyCode::Char('w') => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.state.toggle_wrap();
                    }
                }
                KeyCode::Left => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.state.hscroll(-8);
                    }
                }
                KeyCode::Right => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.state.hscroll(8);
                    }
                }
                KeyCode::Up => self.scroll(-1),
                KeyCode::Down => self.scroll(1),
                KeyCode::PageUp => self.scroll(-(self.view_height.max(2) as isize - 1)),
                KeyCode::PageDown => self.scroll(self.view_height.max(2) as isize - 1),
                KeyCode::Home => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.state.scroll_home();
                    }
                }
                KeyCode::End | KeyCode::Char('f') => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.state.resume_follow();
                    }
                }
                _ => {}
            },
        }
    }

    fn scroll(&mut self, delta: isize) {
        let height = self.view_height;
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.state.scroll(delta, height);
        }
    }

    /// Rebuild the reader-facing config from the tab's current state.
    fn publish_config(tab: &Tab) {
        tab.handles.config_tx.send_replace(TabConfig {
            filter: tab.state.filter.compiled.clone(),
            filter_gen: tab.state.filter.generation,
            export: tab.state.export.as_ref().map(|e| e.tx.clone()),
        });
    }

    fn toggle_export(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab.state.export.take().is_some() {
            // Dropping the sender (here and via the config below) closes the
            // channel; the writer task flushes and exits on its own.
            Self::publish_config(tab);
            self.status_msg = Some(String::from("export stopped"));
        } else {
            self.input = InputMode::ExportPath(PromptEditor::default());
        }
    }

    fn start_export(&mut self, path: PathBuf) {
        let event_tx = self.event_tx.clone();
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let (tx, handle) = export::spawn_export(path.clone(), tab.state.id, event_tx);
        // Dump the currently matched buffer first, then the reader takes over
        // for everything from here on (PRD: toggle-on forward + buffer dump).
        let dump: export::ExportBatch = tab
            .state
            .buffer
            .iter()
            .filter(|l| l.matched)
            .map(Line::export_text)
            .collect();
        if !dump.is_empty() {
            let _ = tx.try_send(dump); // capacity 1024, channel is brand new
        }
        tab.state.export = Some(ExportState { path, tx });
        Self::publish_config(tab);
        self.export_handles.push(handle);
    }

    fn toggle_filter(&mut self, change: impl FnOnce(&mut FilterState)) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            change(&mut tab.state.filter);
            self.apply_filter_change();
        }
    }

    /// Recompile the active tab's filter, publish it to the reader task, and
    /// kick off an off-thread re-match of the current buffer.
    fn apply_filter_change(&mut self) {
        let event_tx = self.event_tx.clone();
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let fs = &mut tab.state.filter;

        if fs.raw.is_empty() {
            fs.compiled = None;
            fs.error = None;
            fs.generation += 1;
            Self::publish_config(tab);
            tab.state.clear_matches();
            return;
        }

        let spec = FilterSpec {
            raw: fs.raw.clone(),
            is_regex: fs.is_regex,
            case_insensitive: fs.case_insensitive,
        };
        match CompiledFilter::compile(&spec) {
            Err(e) => fs.error = Some(e.to_string()), // keep the previous filter
            Ok(compiled) => {
                let compiled = Arc::new(compiled);
                fs.compiled = Some(compiled.clone());
                fs.error = None;
                fs.generation += 1;
                let generation = fs.generation;
                Self::publish_config(tab);
                // Cheap snapshot (Arc clones), expensive matching off-thread.
                let snapshot: Vec<Arc<str>> =
                    tab.state.buffer.iter().map(|l| l.text.clone()).collect();
                let snap_base = tab.state.base_seq;
                let id = tab.state.id;
                tokio::spawn(async move {
                    let flags = tokio::task::spawn_blocking(move || {
                        filter::recompute(&snapshot, &compiled)
                    })
                    .await
                    .expect("recompute task panicked");
                    let _ = event_tx
                        .send(AppEvent::RecomputeDone {
                            tab: id,
                            generation,
                            snap_base,
                            matched_flags: flags,
                        })
                        .await;
                });
            }
        }
    }

    fn apply_search_query(&mut self, query: String) {
        let height = self.view_height;
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if query.is_empty() {
            tab.state.search.clear();
            self.status_msg = None;
            return;
        }
        let mut search = std::mem::take(&mut tab.state.search);
        if search.apply(&tab.state, query, height) {
            let line = search.current.as_ref().map(|m| m.line);
            tab.state.search = search;
            if let Some(line) = line {
                tab.state.anchor_view_position(line);
            }
            self.status_msg = None;
        } else {
            tab.state.search = search;
            self.status_msg = Some(String::from("search: no matches"));
        }
    }

    fn search_next(&mut self) {
        self.search_move(true);
    }

    fn search_previous(&mut self) {
        self.search_move(false);
    }

    fn search_move(&mut self, forward: bool) {
        let height = self.view_height;
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let mut search = std::mem::take(&mut tab.state.search);
        let found = if forward {
            search.next(&tab.state, height)
        } else {
            search.previous(&tab.state, height)
        };
        let line = search.current.as_ref().map(|m| m.line);
        tab.state.search = search;
        if found {
            if let Some(line) = line {
                tab.state.anchor_view_position(line);
            }
            self.status_msg = None;
        } else {
            self.status_msg = Some(String::from("search: no query"));
        }
    }

    fn start_visual_selection(&mut self, kind: SelectionKind) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.state.view_len() == 0 {
            self.status_msg = Some(String::from("selection: no lines"));
            return;
        }
        self.input =
            InputMode::VisualSelection(VisualSelection::start(&tab.state, self.view_height, kind));
    }

    fn copy_visual_selection(&mut self) {
        let selection = match &self.input {
            InputMode::VisualSelection(selection) => selection.clone(),
            _ => return,
        };
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let text = selection.extract_text(&tab.state);
        match set_clipboard_text(&text) {
            Ok(()) => {
                let count = selection.selected_line_count();
                self.status_msg = Some(if count == 1 {
                    String::from("copied 1 line")
                } else {
                    format!("copied {count} lines")
                });
                self.input = InputMode::Normal;
            }
            Err(e) => {
                self.status_msg = Some(format!("copy failed: {e}"));
            }
        }
    }

    fn switch_tab(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        self.active = (self.active as isize + delta).rem_euclid(n) as usize;
        self.tabs[self.active].state.unseen = false;
    }

    fn switch_to_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            self.tabs[self.active].state.unseen = false;
        }
    }

    /// Close the active tab. Dropping its `TabHandles` stops the watcher,
    /// which cascades into the reader task exiting on its own.
    fn close_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        self.tabs.remove(self.active);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.state.unseen = false;
        }
    }

    fn apply(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Batch {
                tab,
                mut lines,
                filter_gen,
            } => {
                if let Some(idx) = self.tabs.iter().position(|t| t.state.id == tab) {
                    let state = &mut self.tabs[idx].state;
                    // Reader hadn't seen the latest filter yet: re-match this
                    // batch inline (bounded at BATCH_LINES, sub-millisecond).
                    if filter_gen != state.filter.generation {
                        let compiled = state.filter.compiled.clone();
                        for line in &mut lines {
                            line.matched =
                                compiled.as_ref().is_some_and(|f| f.is_match(&line.text));
                        }
                    }
                    let has_filter = state.filter.compiled.is_some();
                    let newsworthy = if has_filter {
                        lines.iter().any(|l| l.matched)
                    } else {
                        !lines.is_empty()
                    };
                    state.push_batch(lines);
                    if idx != self.active && newsworthy {
                        state.unseen = true;
                    }
                }
            }
            AppEvent::InitialScan { tab, mut lines } => {
                if let Some(t) = self.tab_mut(tab) {
                    // A filter may already be live if the scan was slow.
                    if t.state.filter.generation != 0 {
                        let compiled = t.state.filter.compiled.clone();
                        for line in &mut lines {
                            line.matched =
                                compiled.as_ref().is_some_and(|f| f.is_match(&line.text));
                        }
                    }
                    t.state.push_batch(lines);
                }
            }
            AppEvent::RecomputeDone {
                tab,
                generation,
                snap_base,
                matched_flags,
            } => {
                if let Some(t) = self.tab_mut(tab) {
                    t.state
                        .apply_recompute(generation, snap_base, &matched_flags);
                }
            }
            AppEvent::FileError { tab, msg } => {
                let name = self
                    .tab_mut(tab)
                    .map(|t| t.state.path.display().to_string())
                    .unwrap_or_default();
                self.status_msg = Some(format!("{name}: {msg}"));
            }
        }
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.state.id == id)
    }

    fn open_tab(&mut self, path: PathBuf) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        match watcher::spawn_tab(path.clone(), id, self.event_tx.clone(), self.buffer_lines) {
            Ok(handles) => {
                self.tabs.push(Tab {
                    state: TabState::new(id, path.clone(), self.buffer_lines),
                    handles,
                });
                self.active = self.tabs.len() - 1;
                self.history_store.record(path);
            }
            Err(e) => self.status_msg = Some(format!("{e:#}")),
        }
    }
}

#[cfg(not(test))]
fn set_clipboard_text(text: &str) -> anyhow::Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text.to_owned())?;
    Ok(())
}

#[cfg(test)]
fn set_clipboard_text(text: &str) -> anyhow::Result<()> {
    if text == "__lucoa_clipboard_failure__" {
        anyhow::bail!("test clipboard failure");
    }
    Ok(())
}

fn clear_completion_status(status_msg: &mut Option<String>, old_hint: Option<&str>) {
    if old_hint.is_some_and(|hint| status_msg.as_deref() == Some(hint)) {
        *status_msg = None;
    }
}

fn replace_completion_status(
    status_msg: &mut Option<String>,
    old_hint: Option<&str>,
    new_hint: Option<&str>,
) {
    match new_hint {
        Some(hint) => *status_msg = Some(hint.to_owned()),
        None => clear_completion_status(status_msg, old_hint),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use crossterm::event::KeyModifiers;
    use std::ffi::OsStr;
    use std::fs;

    fn path_input(parent: &Path, fragment: &str) -> String {
        format!(
            "{}{}{}",
            parent.display(),
            std::path::MAIN_SEPARATOR,
            fragment
        )
    }

    fn outcome(input: &str) -> PathCompletionOutcome {
        complete_path(input, input.len(), &[]).unwrap()
    }

    #[test]
    fn altgr_is_text_input_bare_ctrl_or_alt_is_not() {
        assert!(is_text_input(KeyModifiers::NONE));
        assert!(is_text_input(KeyModifiers::SHIFT));
        // AltGr = CTRL+ALT: how `\` arrives on Spanish/LA layouts, including
        // Windows-synthesized paste keystrokes (spec/9).
        assert!(is_text_input(KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert!(is_text_input(
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT
        ));
        assert!(!is_text_input(KeyModifiers::CONTROL));
        assert!(!is_text_input(KeyModifiers::ALT));
    }

    #[test]
    fn altgr_backslash_lands_in_prompt_buffer() {
        let mut editor = PromptEditor::new(String::from("C:"));
        let key = KeyEvent::new(
            KeyCode::Char('\\'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(matches!(edit_text(&mut editor, key), EditResult::Pending));
        assert_eq!(editor.text(), "C:\\");
        assert_eq!(editor.cursor(), 3);
    }

    #[test]
    fn prompt_editor_inserts_and_deletes_at_cursor() {
        let mut editor = PromptEditor::default();
        for c in ['a', 'c'] {
            assert!(matches!(
                edit_text(
                    &mut editor,
                    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
                ),
                EditResult::Pending
            ));
        }
        assert!(matches!(
            edit_text(
                &mut editor,
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)
            ),
            EditResult::Pending
        ));
        assert!(matches!(
            edit_text(
                &mut editor,
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)
            ),
            EditResult::Pending
        ));
        assert_eq!(editor.text(), "abc");
        assert_eq!(editor.cursor(), 2);

        assert!(matches!(
            edit_text(
                &mut editor,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
            ),
            EditResult::Pending
        ));
        assert_eq!(editor.text(), "ac");
        assert_eq!(editor.cursor(), 1);

        assert!(matches!(
            edit_text(
                &mut editor,
                KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)
            ),
            EditResult::Pending
        ));
        assert_eq!(editor.text(), "a");
        assert_eq!(editor.cursor(), 1);
    }

    #[test]
    fn prompt_editor_home_end_and_unicode_movement_are_char_safe() {
        let mut editor = PromptEditor::new(String::from("a漢b"));
        assert_eq!(editor.cursor(), "a漢b".len());

        assert!(matches!(
            edit_text(
                &mut editor,
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)
            ),
            EditResult::Pending
        ));
        assert_eq!(editor.cursor(), "a漢".len());

        assert!(matches!(
            edit_text(
                &mut editor,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
            ),
            EditResult::Pending
        ));
        assert_eq!(editor.text(), "ab");
        assert_eq!(editor.cursor(), 1);

        assert!(matches!(
            edit_text(
                &mut editor,
                KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)
            ),
            EditResult::Pending
        ));
        assert_eq!(editor.cursor(), 0);
        assert!(matches!(
            edit_text(&mut editor, KeyEvent::new(KeyCode::End, KeyModifiers::NONE)),
            EditResult::Pending
        ));
        assert_eq!(editor.cursor(), editor.text().len());
    }

    #[test]
    fn path_completion_completes_single_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.log"), "").unwrap();

        let out = outcome(&path_input(dir.path(), "alp"));

        assert_eq!(
            out.before_cursor.as_deref(),
            Some(path_input(dir.path(), "alpha.log").as_str())
        );
        assert_eq!(out.ghost_suffix, None);
        assert_eq!(out.hint, None);
    }

    #[test]
    fn path_completion_appends_separator_for_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("logs")).unwrap();

        let out = outcome(&path_input(dir.path(), "lo"));

        assert_eq!(
            out.before_cursor.as_deref(),
            Some(path_input(dir.path(), &format!("logs{}", std::path::MAIN_SEPARATOR)).as_str())
        );
    }

    #[test]
    fn path_completion_inserts_longest_common_prefix() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.log"), "").unwrap();
        fs::write(dir.path().join("alpine.log"), "").unwrap();

        let out = outcome(&path_input(dir.path(), "a"));

        assert_eq!(
            out.before_cursor.as_deref(),
            Some(path_input(dir.path(), "alp").as_str())
        );
        assert_eq!(out.ghost_suffix.as_deref(), Some("ha.log"));
        assert_eq!(out.hint.as_deref(), Some("2 matches: alpha.log alpine.log"));
    }

    #[test]
    fn path_completion_ambiguous_without_prefix_keeps_text_and_hints() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ab.log"), "").unwrap();
        fs::write(dir.path().join("ac.log"), "").unwrap();

        let out = outcome(&path_input(dir.path(), "a"));

        assert_eq!(out.before_cursor, None);
        assert_eq!(out.ghost_suffix.as_deref(), Some("b.log"));
        assert_eq!(out.hint.as_deref(), Some("2 matches: ab.log ac.log"));
    }

    #[test]
    fn path_completion_no_matches_leaves_input_unchanged() {
        let dir = tempfile::tempdir().unwrap();

        let out = outcome(&path_input(dir.path(), "none"));

        assert_eq!(out.before_cursor, None);
        assert_eq!(out.ghost_suffix, None);
        assert_eq!(out.hint.as_deref(), Some("no matches"));
    }

    #[test]
    fn path_completion_hidden_files_require_dot_fragment() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".secret.log"), "").unwrap();

        let out = outcome(&path_input(dir.path(), "s"));
        assert_eq!(out.hint.as_deref(), Some("no matches"));

        let out = outcome(&path_input(dir.path(), ".s"));
        assert_eq!(
            out.before_cursor.as_deref(),
            Some(path_input(dir.path(), ".secret.log").as_str())
        );
    }

    #[test]
    fn path_completion_supports_relative_and_absolute_parents() {
        let rel_dir = tempfile::tempdir_in(".").unwrap();
        fs::write(rel_dir.path().join("relative.log"), "").unwrap();
        let rel_parent = rel_dir.path().file_name().and_then(OsStr::to_str).unwrap();

        let out = outcome(&format!("{rel_parent}{}rel", std::path::MAIN_SEPARATOR));
        assert_eq!(
            out.before_cursor.as_deref(),
            Some(format!("{rel_parent}{}relative.log", std::path::MAIN_SEPARATOR).as_str())
        );

        let abs_dir = tempfile::tempdir().unwrap();
        fs::write(abs_dir.path().join("absolute.log"), "").unwrap();
        let out = outcome(&path_input(abs_dir.path(), "abs"));
        assert_eq!(
            out.before_cursor.as_deref(),
            Some(path_input(abs_dir.path(), "absolute.log").as_str())
        );
    }

    #[test]
    fn clean_path_strips_paired_quotes() {
        // Windows Explorer "Copy as path" format
        assert_eq!(
            clean_path("\"C:\\dev\\agent0.log.0\""),
            Some(PathBuf::from("C:\\dev\\agent0.log.0"))
        );
        assert_eq!(
            clean_path("  'C:\\x.log'  "),
            Some(PathBuf::from("C:\\x.log"))
        );
        assert_eq!(
            clean_path("C:\\plain.log"),
            Some(PathBuf::from("C:\\plain.log"))
        );
        assert_eq!(
            clean_path("\"C:\\with space\\a b.log\""),
            Some(PathBuf::from("C:\\with space\\a b.log"))
        );
        assert_eq!(clean_path("\"\""), None);
        assert_eq!(clean_path("   "), None);
        // a lone quote is not a pair — kept as typed
        assert_eq!(clean_path("\"C:\\odd"), Some(PathBuf::from("\"C:\\odd")));
    }

    #[test]
    fn paste_appends_to_prompt_and_strips_controls() {
        let mut app = App::new(100, Theme::ascii());
        app.input = InputMode::OpenPath(OpenPathPrompt::default());
        app.handle_paste("C:\\dev\\ServiceNOW\\agent\\logs\\agent0.log.0\r\n");
        match &app.input {
            InputMode::OpenPath(prompt) => {
                assert_eq!(
                    prompt.editor().text(),
                    "C:\\dev\\ServiceNOW\\agent\\logs\\agent0.log.0"
                );
            }
            _ => panic!("left OpenPath mode"),
        }
        // Paste in normal mode is a no-op, not a key storm.
        app.input = InputMode::Normal;
        app.handle_paste("qqqq");
        assert!(!app.should_quit);
    }

    #[test]
    fn paste_inserts_at_prompt_cursor() {
        let mut app = App::new(100, Theme::ascii());
        let mut editor = PromptEditor::new(String::from("ac"));
        editor.move_left();
        app.input = InputMode::OpenPath(OpenPathPrompt::with_editor(editor));

        app.handle_paste("b\r\n");

        match &app.input {
            InputMode::OpenPath(prompt) => {
                assert_eq!(prompt.editor().text(), "abc");
                assert_eq!(prompt.editor().cursor(), 2);
            }
            _ => panic!("left OpenPath mode"),
        }
    }

    #[test]
    fn ctrl_shortcuts_ignore_altgr_combos() {
        let mut app = App::new(100, Theme::ascii());
        app.handle_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert!(!app.should_quit, "AltGr+c must not quit");
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn tab_in_open_path_triggers_completion() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("target.log"), "").unwrap();
        let mut app = App::new(100, Theme::ascii());
        app.input = InputMode::OpenPath(OpenPathPrompt::with_editor(PromptEditor::new(
            path_input(dir.path(), "tar"),
        )));

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        match &app.input {
            InputMode::OpenPath(prompt) => {
                assert_eq!(prompt.editor().text(), path_input(dir.path(), "target.log"));
            }
            _ => panic!("left OpenPath mode"),
        }
    }

    #[tokio::test]
    async fn tab_in_normal_mode_still_switches_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.log");
        let second = dir.path().join("second.log");
        fs::write(&first, "").unwrap();
        fs::write(&second, "").unwrap();
        let mut app = App::new(100, Theme::ascii());
        app.open_tab(first);
        app.open_tab(second);
        app.active = 0;

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.active, 1);
    }

    #[test]
    fn tab_in_filter_and_export_prompts_remains_text_edit_noop() {
        let mut app = App::new(100, Theme::ascii());

        app.input = InputMode::FilterEdit(PromptEditor::new(String::from("abc")));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        match &app.input {
            InputMode::FilterEdit(editor) => assert_eq!(editor.text(), "abc"),
            _ => panic!("left FilterEdit mode"),
        }

        app.input = InputMode::ExportPath(PromptEditor::new(String::from("out.log")));
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        match &app.input {
            InputMode::ExportPath(editor) => assert_eq!(editor.text(), "out.log"),
            _ => panic!("left ExportPath mode"),
        }
    }

    #[test]
    fn completion_before_cursor_preserves_suffix_after_cursor() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.log"), "").unwrap();
        let mut editor = PromptEditor::new(format!("{}ZZ", path_input(dir.path(), "alp")));
        editor.move_left();
        editor.move_left();
        let mut app = App::new(100, Theme::ascii());
        app.input = InputMode::OpenPath(OpenPathPrompt::with_editor(editor));

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        match &app.input {
            InputMode::OpenPath(prompt) => {
                assert_eq!(
                    prompt.editor().text(),
                    format!("{}ZZ", path_input(dir.path(), "alpha.log"))
                );
                assert_eq!(
                    prompt.editor().cursor(),
                    path_input(dir.path(), "alpha.log").len()
                );
            }
            _ => panic!("left OpenPath mode"),
        }
    }

    fn app_with_lines(lines: Vec<&[u8]>) -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.log");
        fs::write(&path, "").unwrap();
        let mut app = App::new(100, Theme::ascii());
        app.open_tab(path);
        app.tabs[0]
            .state
            .push_batch(lines.into_iter().map(Line::from_bytes).collect());
        app.tabs[0].state.follow = false;
        app.tabs[0].state.anchor = Some(0);
        (app, dir)
    }

    #[tokio::test]
    async fn digit_keys_switch_directly_and_clear_unseen_marker() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.log");
        let second = dir.path().join("second.log");
        fs::write(&first, "").unwrap();
        fs::write(&second, "").unwrap();
        let mut app = App::new(100, Theme::ascii());
        app.open_tab(first);
        app.open_tab(second);
        app.active = 0;
        app.tabs[1].state.unseen = true;

        app.handle_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));

        assert_eq!(app.active, 1);
        assert!(!app.tabs[1].state.unseen);
    }

    #[tokio::test]
    async fn search_prompt_and_next_previous_anchor_matches() {
        let (mut app, _dir) = app_with_lines(vec![
            b"alpha".as_ref(),
            b"ERROR one".as_ref(),
            b"middle".as_ref(),
            b"error two".as_ref(),
        ]);
        app.view_height = 2;

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('E'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(app.input, InputMode::Normal));
        assert_eq!(app.tabs[0].state.search.query, "ERR");
        assert_eq!(app.tabs[0].state.search.current.as_ref().unwrap().line, 1);
        assert!(!app.tabs[0].state.follow);
        assert_eq!(app.tabs[0].state.anchor, Some(1));

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(app.tabs[0].state.search.current.as_ref().unwrap().line, 3);
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(app.tabs[0].state.search.current.as_ref().unwrap().line, 1);
    }

    #[tokio::test]
    async fn visual_selection_copy_success_and_failure_update_status() {
        let (mut app, _dir) = app_with_lines(vec![b"alpha".as_ref(), b"beta".as_ref()]);

        app.handle_key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT));
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        assert!(matches!(app.input, InputMode::Normal));
        assert_eq!(app.status_msg.as_deref(), Some("copied 2 lines"));

        let (mut app, _dir) = app_with_lines(vec![b"__lucoa_clipboard_failure__".as_ref()]);
        app.handle_key(KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT));
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|msg| msg.starts_with("copy failed:"))
        );
    }

    #[test]
    fn help_overlay_toggles_and_dismisses() {
        let mut app = App::new(100, Theme::ascii());

        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(matches!(app.input, InputMode::HelpOverlay));
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(matches!(app.input, InputMode::Normal));

        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.input, InputMode::Normal));
    }

    #[test]
    fn history_completion_is_fuzzy_and_precedes_filesystem_candidates() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.log"), "").unwrap();
        let history = vec![dir.path().join("agent.log")];

        let out = complete_path("ag", 2, &history).unwrap();
        let expected = dir.path().join("agent.log").to_string_lossy().into_owned();

        assert_eq!(out.before_cursor.as_deref(), Some(expected.as_str()));
    }
}
