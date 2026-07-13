//! Minimal ANSI/SGR handling for log *content* (spec/7).
//!
//! One tokenizer feeds both consumers, which is the load-bearing invariant:
//! the plain text produced by [`strip`] (what filters match against and what
//! match byte-ranges index into) is exactly the concatenation of the segment
//! texts produced by [`segments`] (what the renderer styles). A third-party
//! parser could disagree with our stripper on unrecognized sequences and
//! silently shift every highlight offset.
//!
//! Recognized: CSI SGR (`ESC[...m`) is interpreted into styles. Tabs are
//! expanded to 8-column stops — ratatui's buffer silently drops any grapheme
//! containing a control character, so a raw `\t` would make columns collapse
//! (spec/8). Everything else — other CSI sequences, OSC, lone escapes, C0
//! controls, DEL — is removed from the text entirely, which doubles as the
//! terminal escape-injection defense noted in spec/0.

use ratatui::style::{Color, Modifier, Style};

/// One styled run of a parsed line.
pub struct Segment {
    pub text: String,
    pub style: Style,
}

/// True if the bytes contain anything the tokenizer would transform —
/// i.e. whether this line needs the parsed path at all.
pub fn needs_parse(bytes: &[u8]) -> bool {
    bytes.iter().any(|&b| b < 0x20 || b == 0x7f)
}

/// Plain text with every escape sequence and control byte removed.
pub fn strip(bytes: &[u8]) -> String {
    let mut out = String::new();
    tokenize(bytes, |text, _| out.push_str(text));
    out
}

/// Styled segments; concatenating their `text` yields exactly `strip(bytes)`.
pub fn segments(bytes: &[u8]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    tokenize(bytes, |text, style| match out.last_mut() {
        Some(last) if last.style == style => last.text.push_str(text),
        _ => out.push(Segment {
            text: text.to_owned(),
            style,
        }),
    });
    out
}

/// Walk the bytes, invoking `emit(text, current_style)` for each text run.
/// Tabs expand to 8-column stops; the column estimate counts chars, which is
/// exact for ASCII logs (double-width CJK may land a stop early — harmless).
fn tokenize(bytes: &[u8], mut emit: impl FnMut(&str, Style)) {
    let mut style = Style::new();
    let mut col = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => i = consume_escape(bytes, i, &mut style),
            b'\t' => {
                let pad = 8 - col % 8;
                emit(&" ".repeat(pad), style);
                col += pad;
                i += 1;
            }
            b if b < 0x20 || b == 0x7f => i += 1, // stray control
            _ => {
                let start = i;
                while i < bytes.len() && !(bytes[i] < 0x20 || bytes[i] == 0x7f) {
                    i += 1;
                }
                let text = String::from_utf8_lossy(&bytes[start..i]);
                col += text.chars().count();
                emit(&text, style);
            }
        }
    }
}

/// Consume one escape sequence starting at `bytes[i] == ESC`; returns the
/// index just past it. SGR sequences update `style`; everything else is
/// skipped.
fn consume_escape(bytes: &[u8], i: usize, style: &mut Style) -> usize {
    match bytes.get(i + 1) {
        Some(b'[') => {
            // CSI: parameters/intermediates until a final byte in 0x40..=0x7E.
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            if bytes.get(j) == Some(&b'm') {
                apply_sgr(&bytes[i + 2..j], style);
            }
            (j + 1).min(bytes.len())
        }
        Some(b']') => {
            // OSC: until BEL or ESC \ (string terminator).
            let mut j = i + 2;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    return j + 1;
                }
                if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                    return j + 2;
                }
                j += 1;
            }
            j
        }
        Some(_) => i + 2, // ESC + one char (charset selection etc.)
        None => i + 1,
    }
}

/// Apply SGR parameters (the bytes between `ESC[` and `m`) to the style.
fn apply_sgr(params: &[u8], style: &mut Style) {
    let nums: Vec<u16> = params
        .split(|&b| b == b';' || b == b':')
        .map(|p| {
            p.iter()
                .filter(|b| b.is_ascii_digit())
                .fold(0u16, |acc, &b| {
                    acc.saturating_mul(10).saturating_add(u16::from(b - b'0'))
                })
        })
        .collect();

    let mut k = 0;
    while k < nums.len() {
        match nums[k] {
            0 => *style = Style::new(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            9 => *style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            29 => *style = style.remove_modifier(Modifier::CROSSED_OUT),
            30..=37 => style.fg = Some(ansi16(nums[k] - 30, false)),
            39 => style.fg = None,
            40..=47 => style.bg = Some(ansi16(nums[k] - 40, false)),
            49 => style.bg = None,
            90..=97 => style.fg = Some(ansi16(nums[k] - 90, true)),
            100..=107 => style.bg = Some(ansi16(nums[k] - 100, true)),
            38 | 48 => {
                let target_fg = nums[k] == 38;
                let color = match nums.get(k + 1) {
                    Some(5) => {
                        k += 2;
                        nums.get(k).map(|&n| Color::Indexed(n.min(255) as u8))
                    }
                    Some(2) => {
                        k += 4;
                        match (nums.get(k - 2), nums.get(k - 1), nums.get(k)) {
                            (Some(&r), Some(&g), Some(&b)) => Some(Color::Rgb(
                                r.min(255) as u8,
                                g.min(255) as u8,
                                b.min(255) as u8,
                            )),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(c) = color {
                    if target_fg {
                        style.fg = Some(c);
                    } else {
                        style.bg = Some(c);
                    }
                }
            }
            _ => {}
        }
        k += 1;
    }
}

fn ansi16(n: u16, bright: bool) -> Color {
    match (n, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        _ => Color::White,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert!(!needs_parse(b"hello world"));
        assert_eq!(strip(b"hello world"), "hello world");
    }

    #[test]
    fn tabs_expand_to_8_column_stops() {
        assert!(needs_parse(b"a\tb"));
        assert_eq!(strip(b"\tx"), "        x"); // col 0 -> stop 8
        assert_eq!(strip(b"a\tb"), "a       b"); // col 1 -> stop 8
        assert_eq!(strip(b"12345678\tx"), "12345678        x"); // col 8 -> stop 16
        // expansion inherits the active style
        let segs = segments(b"\x1b[31ma\tb\x1b[0m");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "a       b");
        assert_eq!(segs[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn sgr_is_stripped_and_styled() {
        let raw = b"\x1b[31mERROR\x1b[0m done";
        assert!(needs_parse(raw));
        assert_eq!(strip(raw), "ERROR done");
        let segs = segments(raw);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "ERROR");
        assert_eq!(segs[0].style.fg, Some(Color::Red));
        assert_eq!(segs[1].text, " done");
        assert_eq!(segs[1].style, Style::new());
    }

    #[test]
    fn segments_concat_equals_strip_always() {
        let cases: [&[u8]; 6] = [
            b"plain",
            b"\x1b[1;32mbold green\x1b[22m normal",
            b"\x1b]0;window title\x07text after osc",
            b"a\x1b[38;5;196mred256\x1b[39mb",
            b"a\x1b[38;2;141;182;0mapple\x1b[0mb",
            b"bad\x1b[999Xseq\x08\x0ctail\x7f",
        ];
        for raw in cases {
            let concat: String = segments(raw).into_iter().map(|s| s.text).collect();
            assert_eq!(concat, strip(raw), "alignment broken for {raw:?}");
        }
    }

    #[test]
    fn extended_colors_parse() {
        let segs = segments(b"\x1b[38;2;141;182;0mapple");
        assert_eq!(segs[0].style.fg, Some(Color::Rgb(141, 182, 0)));
        let segs = segments(b"\x1b[48;5;17mdeep");
        assert_eq!(segs[0].style.bg, Some(Color::Indexed(17)));
    }

    #[test]
    fn non_sgr_sequences_and_controls_are_removed() {
        // cursor movement, OSC title, bell, backspace, DEL
        let raw = b"\x1b[2Jclear\x1b]0;t\x07ed\x07 \x08x\x7f";
        assert_eq!(strip(raw), "cleared x");
    }

    #[test]
    fn multibyte_utf8_survives() {
        let raw = "\x1b[31mhéllo — ✓\x1b[0m".as_bytes();
        assert_eq!(strip(raw), "héllo — ✓");
    }

    #[test]
    fn truncated_sequence_at_end_does_not_panic() {
        assert_eq!(strip(b"text\x1b["), "text");
        assert_eq!(strip(b"text\x1b"), "text");
        assert_eq!(strip(b"text\x1b]osc never ends"), "text");
    }
}
