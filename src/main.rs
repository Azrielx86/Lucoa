mod ansi;
mod app;
mod commands;
mod export;
mod filter;
mod history;
mod search;
mod selection;
mod tab;
mod theme;
mod ui;
mod watcher;

use clap::Parser;
use std::path::PathBuf;

/// Tabbed TUI log tailer: filter, highlight, and export live logs.
#[derive(Parser)]
#[command(name = "lucoa", version)]
struct Cli {
    /// Files to tail; each opens in its own tab
    files: Vec<PathBuf>,

    /// Maximum lines kept in memory per tab
    #[arg(long, default_value_t = 100_000)]
    buffer_lines: usize,

    /// Legacy-terminal mode: ASCII-only characters and basic ANSI colors
    /// (default is a UTF-8 UI with rounded borders and RGB color)
    #[arg(long)]
    ascii: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let theme = if cli.ascii {
        theme::Theme::ascii()
    } else {
        theme::Theme::utf8()
    };
    // Installed before ratatui's hook so a panic unwinds in order: ratatui
    // restores the terminal, then this releases the mouse and paste modes.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        set_terminal_modes(false);
        prev_hook(info);
    }));
    let terminal = ratatui::init();
    set_terminal_modes(true);
    let result = app::App::new(cli.buffer_lines, theme)
        .run(terminal, cli.files)
        .await;
    set_terminal_modes(false);
    ratatui::restore();
    result
}

/// Mouse capture: without it, a wheel scroll moves the *terminal's* viewport
/// under our absolute-coordinate diffs and corrupts the screen (spec/8).
/// Bracketed paste: delivers pastes as one `Event::Paste` string — but only
/// on non-Windows, because crossterm's Windows event reader does not parse
/// the `ESC[200~` markers and they would leak in as key events; Windows
/// paste arrives as synthesized keystrokes, handled by the AltGr-aware text
/// input rules instead (spec/9).
fn set_terminal_modes(enable: bool) {
    use crossterm::event as ev;
    let mut out = std::io::stdout();
    if enable {
        let _ = crossterm::execute!(out, ev::EnableMouseCapture);
        #[cfg(not(windows))]
        let _ = crossterm::execute!(out, ev::EnableBracketedPaste);
    } else {
        let _ = crossterm::execute!(out, ev::DisableMouseCapture);
        #[cfg(not(windows))]
        let _ = crossterm::execute!(out, ev::DisableBracketedPaste);
    }
}
