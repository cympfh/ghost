//! Small terminal drawing of a white ghost for the startup screen.
//! The README keeps `docs/logo.svg`; this mark is only the ghost.

use std::io::{self, Write};
use std::time::Duration;

use ratatui::text::{Line, Span};

use crate::theme;

/// Keep the mark on screen at least this long when preparation finishes sooner.
pub const SPLASH_MIN: Duration = Duration::from_millis(300);

/// Time still to wait so the splash is visible for [`SPLASH_MIN`].
/// Zero when preparation already took that long.
pub fn splash_hold(elapsed: Duration) -> Duration {
    SPLASH_MIN.saturating_sub(elapsed)
}

/// Rounded head, two eyes, wavy hem. Shown when the terminal has a little room.
const FULL: &[&str] = &[
    "    ▄▄▄▄▄    ",
    "  ▄███████▄  ",
    "  ███•█•███  ",
    "█████████████",
    "█████████████",
    " ▀▀▀ ▀▀▀ ▀▀▀ ",
];

/// Same ghost, fewer cells, for a short terminal.
const SMALL: &[&str] = &[
    "  ▄▄▄▄▄  ",
    " ▄█████▄ ",
    " ██•█•██ ",
    "█████████",
    " ▀▀ ▀ ▀▀ ",
];

const TINY: &str = "█•█•█";

/// Largest ghost that fits. A short row when the terminal is smaller than [`SMALL`].
///
/// The full mark asks for `height >= 10` so a 32×10 splash, which keeps one
/// status row, still uses [`SMALL`].
pub fn render(width: u16, height: u16) -> Vec<Line<'static>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }
    if width >= 13 && height >= 10 {
        return mask_lines(FULL);
    }
    if width >= 9 && height >= 5 {
        return mask_lines(SMALL);
    }
    vec![tiny_line(width)]
}

/// Same mark, as ANSI, for the normal terminal while git may ask for a passphrase.
pub fn print_on_stdout() {
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    let height = height.min(16);
    let mut out = io::stdout();
    let _ = writeln!(out, "{}", ansi_block(width, height));
    let _ = out.flush();
}

pub fn ansi_block(width: u16, height: u16) -> String {
    let mut out = String::new();
    for line in render(width, height) {
        for span in line.spans {
            out.push_str(&span_ansi(&span));
        }
        if theme::colors_enabled() {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
    out
}

fn mask_lines(rows: &[&str]) -> Vec<Line<'static>> {
    rows.iter().copied().map(line_from_mask).collect()
}

fn line_from_mask(row: &str) -> Line<'static> {
    Line::from(row.chars().map(span_for).collect::<Vec<_>>())
}

fn tiny_line(width: u16) -> Line<'static> {
    let n = (width as usize).max(1).min(TINY.chars().count());
    Line::from(TINY.chars().take(n).map(span_for).collect::<Vec<_>>())
}

fn span_for(ch: char) -> Span<'static> {
    match ch {
        '█' => Span::styled("█", theme::logo_ghost()),
        '▄' => Span::styled("▄", theme::logo_ghost()),
        '▀' => Span::styled("▀", theme::logo_ghost()),
        '•' => Span::styled("•", theme::logo_eye()),
        _ => Span::raw(" "),
    }
}

fn span_ansi(span: &Span<'_>) -> String {
    let text = span.content.as_ref();
    if !theme::colors_enabled() {
        return text.to_string();
    }
    match text {
        "█" | "▄" | "▀" => format!("\x1b[37m{text}"),
        "•" => "\x1b[38;2;240;80;51;47m•".into(),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_are_rectangular() {
        for rows in [FULL, SMALL] {
            let width = rows[0].chars().count();
            assert!(rows.iter().all(|row| row.chars().count() == width));
        }
    }

    #[test]
    fn splash_hold_is_only_the_remainder() {
        assert_eq!(splash_hold(Duration::ZERO), Duration::from_millis(300));
        assert_eq!(
            splash_hold(Duration::from_millis(299)),
            Duration::from_millis(1)
        );
        assert_eq!(splash_hold(Duration::from_millis(300)), Duration::ZERO);
        assert_eq!(splash_hold(Duration::from_secs(2)), Duration::ZERO);
    }

    #[test]
    fn small_terminal_uses_the_short_mark() {
        let full = render(80, 24);
        let small = render(20, 8);
        let tiny = render(6, 3);
        assert!(full.len() > small.len());
        assert_eq!(small.len(), SMALL.len());
        assert_eq!(tiny.len(), 1);
        let mut text = String::new();
        for line in &tiny {
            for span in &line.spans {
                text.push_str(span.content.as_ref());
            }
        }
        assert!(text.contains('•'), "{text}");
        assert!(!text.contains('▓'), "{text}");
        assert!(render(0, 10).is_empty());
    }

    #[test]
    fn ghost_has_a_round_head_eyes_and_wavy_hem() {
        let full = render(80, 24);
        let mut text = String::new();
        for line in &full {
            for span in &line.spans {
                text.push_str(span.content.as_ref());
            }
            text.push('\n');
        }
        assert!(text.contains('▄'), "rounded head\n{text}");
        assert!(text.contains('•'), "eyes\n{text}");
        assert!(text.contains('▀'), "wavy hem\n{text}");
        assert!(text.contains("▀▀▀ ▀▀▀ ▀▀▀"), "hem waves\n{text}");
        assert!(!text.contains('▓'), "no diamond\n{text}");
        assert_eq!(full.len(), FULL.len());
    }

    #[test]
    fn ansi_respects_no_color() {
        theme::force_colors_for_test(Some(true));
        let on = ansi_block(80, 24);
        assert!(on.contains("38;2;240;80;51"), "{on}");
        assert!(on.contains('█'));
        assert!(on.contains('•'));
        assert!(!on.contains('▓'), "{on}");
        theme::force_colors_for_test(Some(false));
        let off = ansi_block(80, 24);
        assert!(!off.contains('\u{1b}'), "{off}");
        assert!(off.contains('█'));
        assert!(off.contains('•'));
        assert!(off.contains('▄') && off.contains('▀'), "{off}");
        theme::force_colors_for_test(None);
    }
}
