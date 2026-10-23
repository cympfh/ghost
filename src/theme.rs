//! Every color the TUI paints. Call sites use these helpers so `NO_COLOR` can
//! flatten them and so bright black (`Color::DarkGray`) never appears.

use std::cell::Cell;

use ratatui::style::{Color, Modifier, Style};

thread_local! {
    static FORCE_COLOR: Cell<Option<bool>> = const { Cell::new(None) };
}

/// `NO_COLOR` disables color only when it is set and non-empty.
/// Unset and empty leave color on (https://no-color.org/).
pub fn colors_enabled_from(value: Option<&str>) -> bool {
    match value {
        None | Some("") => true,
        Some(_) => false,
    }
}

pub fn colors_enabled() -> bool {
    if let Some(forced) = FORCE_COLOR.with(|c| c.get()) {
        return forced;
    }
    match std::env::var("NO_COLOR") {
        Err(std::env::VarError::NotPresent) => true,
        // Present but not Unicode still counts as set.
        Err(_) => false,
        Ok(value) => colors_enabled_from(Some(value.as_str())),
    }
}

#[cfg(test)]
pub fn force_colors_for_test(enabled: Option<bool>) {
    FORCE_COLOR.with(|c| c.set(enabled));
}

fn paint(color: Color) -> Style {
    if colors_enabled() {
        Style::default().fg(color)
    } else {
        Style::default()
    }
}

fn paint_bold(color: Color) -> Style {
    let style = paint(color);
    if colors_enabled() {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

fn paint_bg(fg: Color, bg: Color) -> Style {
    if colors_enabled() {
        Style::default().fg(fg).bg(bg)
    } else {
        Style::default()
    }
}

fn paint_bg_bold(fg: Color, bg: Color) -> Style {
    let style = paint_bg(fg, bg);
    if colors_enabled() {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// ANSI 7. Readable on a black terminal. Not `Color::DarkGray` (ANSI 8).
const SECONDARY: Color = Color::Gray;

pub fn prompt() -> Style {
    paint_bold(Color::Green)
}

pub fn title() -> Style {
    paint_bold(Color::White)
}

pub fn text() -> Style {
    paint(Color::White)
}

pub fn secondary() -> Style {
    paint(SECONDARY)
}

pub fn accent() -> Style {
    paint(Color::Cyan)
}

pub fn accent_bold() -> Style {
    paint_bold(Color::Cyan)
}

pub fn warn() -> Style {
    paint(Color::Yellow)
}

pub fn warn_bold() -> Style {
    paint_bold(Color::Yellow)
}

pub fn error() -> Style {
    paint(Color::Red)
}

pub fn ok() -> Style {
    paint(Color::Green)
}

pub fn review_choice() -> Style {
    paint(Color::Magenta)
}

pub fn code_choice() -> Style {
    paint(Color::Green)
}

pub fn selected() -> Style {
    paint_bg_bold(Color::Black, Color::White)
}

pub fn cursor() -> Style {
    paint_bg_bold(Color::Black, Color::Yellow)
}

pub fn cursor_block() -> Style {
    paint_bg(Color::Black, Color::Yellow)
}

pub fn status_bar() -> Style {
    paint_bg_bold(Color::Black, Color::White)
}

pub fn border_fatal() -> Style {
    paint(Color::Red)
}

pub fn border_input() -> Style {
    paint(Color::Yellow)
}

pub fn border_running() -> Style {
    paint(Color::Cyan)
}

pub fn border_confirm() -> Style {
    paint(Color::Yellow)
}

pub fn border_idle() -> Style {
    paint(Color::White)
}

pub fn border_choose() -> Style {
    paint(Color::Cyan)
}

pub fn badge(text: &str) -> Style {
    if text.contains('✓') || text.contains("approved") {
        ok()
    } else if text.contains('✗') || text.contains("changes") {
        error()
    } else if text.contains('…') || text.contains("review") {
        warn()
    } else {
        secondary()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_is_set_and_non_empty_only() {
        assert!(colors_enabled_from(None));
        assert!(colors_enabled_from(Some("")));
        assert!(!colors_enabled_from(Some("1")));
        assert!(!colors_enabled_from(Some("0")));
    }

    #[test]
    fn secondary_is_gray_and_no_color_drops_fg_and_bg() {
        force_colors_for_test(Some(true));
        let on = secondary();
        assert_eq!(on.fg, Some(Color::Gray));
        assert_ne!(on.fg, Some(Color::DarkGray));
        assert!(!on.add_modifier.contains(Modifier::DIM));
        let bar = status_bar();
        assert_eq!(bar.fg, Some(Color::Black));
        assert_eq!(bar.bg, Some(Color::White));
        assert!(bar.add_modifier.contains(Modifier::BOLD));
        assert!(!bar.add_modifier.contains(Modifier::DIM));

        force_colors_for_test(Some(false));
        let off = secondary();
        assert_eq!(off.fg, None);
        assert_eq!(off.bg, None);
        assert!(!off.add_modifier.contains(Modifier::DIM));
        assert!(!off.add_modifier.contains(Modifier::BOLD));
        let plain = status_bar();
        assert_eq!(plain.fg, None);
        assert_eq!(plain.bg, None);
        force_colors_for_test(None);
    }
}
