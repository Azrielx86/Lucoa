use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;

/// Apple green — the UTF-8 theme's chrome accent.
pub const APPLE_GREEN: Color = Color::Rgb(0x8d, 0xb6, 0x00);

/// Pure-ASCII border set for legacy terminals.
const ASCII_BORDER: border::Set = border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// Everything the renderer needs to know about the terminal's capabilities.
/// Chosen once at startup (`--ascii` → `Theme::ascii()`, default →
/// `Theme::utf8()`); log *content* colors are the log's own and pass through
/// in both modes — the theme governs only lucoa's chrome.
pub struct Theme {
    pub border_set: border::Set,
    /// Chrome accent (borders, titles, markers).
    pub chrome: Style,
    /// The active tab.
    pub tab_active: Style,
    /// Matched span overlay (Highlight mode).
    pub match_style: Style,
    pub selection_style: Style,
    pub overlay_style: Style,
    /// Non-matching lines in Highlight mode; also dims their embedded colors.
    pub dim: Style,
    pub follow: Style,
    pub paused: Style,
    pub error: Style,
    /// Status-bar segment / tab divider.
    pub divider: &'static str,
    /// Marks a tab with unseen matches.
    pub unseen_marker: &'static str,
}

impl Theme {
    /// Modern terminals: rounded corners, RGB apple green.
    pub fn utf8() -> Self {
        Theme {
            border_set: border::ROUNDED,
            chrome: Style::new().fg(APPLE_GREEN),
            tab_active: Style::new()
                .fg(Color::Black)
                .bg(APPLE_GREEN)
                .add_modifier(Modifier::BOLD),
            match_style: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            selection_style: Style::new().fg(Color::Black).bg(Color::Yellow),
            overlay_style: Style::new().fg(Color::White).bg(Color::Black),
            dim: Style::new().add_modifier(Modifier::DIM),
            follow: Style::new().fg(APPLE_GREEN).add_modifier(Modifier::BOLD),
            paused: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            error: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            divider: "│",
            unseen_marker: "●",
        }
    }

    /// Legacy terminals: ASCII characters only, basic ANSI-16 colors only,
    /// and no reliance on the DIM attribute (DarkGray carries the meaning
    /// where DIM is unsupported).
    pub fn ascii() -> Self {
        Theme {
            border_set: ASCII_BORDER,
            chrome: Style::new().fg(Color::Green),
            tab_active: Style::new()
                .fg(Color::Green)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
            match_style: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            selection_style: Style::new().fg(Color::Black).bg(Color::Yellow),
            overlay_style: Style::new().fg(Color::White).bg(Color::Black),
            dim: Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            follow: Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            paused: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            error: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            divider: "|",
            unseen_marker: "*",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs(t: &Theme) -> Vec<&'static str> {
        let b = &t.border_set;
        vec![
            b.top_left,
            b.top_right,
            b.bottom_left,
            b.bottom_right,
            b.vertical_left,
            b.vertical_right,
            b.horizontal_top,
            b.horizontal_bottom,
            t.divider,
            t.unseen_marker,
        ]
    }

    fn colors(t: &Theme) -> Vec<Option<Color>> {
        [
            t.chrome,
            t.tab_active,
            t.match_style,
            t.selection_style,
            t.overlay_style,
            t.dim,
            t.follow,
            t.paused,
            t.error,
        ]
        .iter()
        .flat_map(|s| [s.fg, s.bg])
        .collect()
    }

    #[test]
    fn ascii_theme_uses_only_ascii_glyphs() {
        for g in glyphs(&Theme::ascii()) {
            assert!(g.is_ascii(), "non-ASCII glyph in ascii theme: {g:?}");
        }
    }

    #[test]
    fn ascii_theme_uses_only_ansi16_colors() {
        for c in colors(&Theme::ascii()).into_iter().flatten() {
            assert!(
                !matches!(c, Color::Rgb(..) | Color::Indexed(_)),
                "non-ANSI16 color in ascii theme: {c:?}"
            );
        }
    }

    #[test]
    fn utf8_theme_is_apple_green() {
        assert_eq!(APPLE_GREEN, Color::Rgb(141, 182, 0));
        assert_eq!(Theme::utf8().chrome.fg, Some(APPLE_GREEN));
        assert_eq!(Theme::utf8().tab_active.bg, Some(APPLE_GREEN));
    }
}
