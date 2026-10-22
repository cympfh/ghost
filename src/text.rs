use std::time::Duration;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Drop CSI and OSC sequences so agent output can be drawn as plain text.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for d in chars.by_ref() {
                        if d.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(d) = chars.next() {
                        if d == '\u{7}' {
                            break;
                        }
                        if d == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if c != '\r' {
            out.push(c);
        }
    }
    out
}

pub fn format_countdown(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

pub fn short_sha(sha: &str) -> &str {
    let n = sha.len().min(7);
    &sha[..n]
}

/// Wrap a single logical line into terminal columns, accounting for wide glyphs.
pub fn wrap_cols(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if used + w > width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(ch);
        used += w;
    }
    if lines.is_empty() || !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

pub fn truncate_cols(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if used + w > width - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

pub fn pad_cols(text: &str, width: usize) -> String {
    let t = truncate_cols(text, width);
    let w = UnicodeWidthStr::width(t.as_str());
    if w >= width {
        t
    } else {
        format!("{t}{}", " ".repeat(width - w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_color_and_cr() {
        let s = strip_ansi("\u{1b}[31mred\u{1b}[0m\r\n");
        assert_eq!(s, "red\n");
    }

    #[test]
    fn countdown_formats() {
        assert_eq!(format_countdown(Duration::from_secs(5)), "00:05");
        assert_eq!(format_countdown(Duration::from_secs(90)), "01:30");
        assert_eq!(format_countdown(Duration::from_secs(3661)), "1:01:01");
    }

    #[test]
    fn wraps_wide_characters() {
        let lines = wrap_cols("あいう", 4);
        assert_eq!(lines, vec!["あい".to_string(), "う".to_string()]);
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate_cols("abcdef", 4), "abc…");
        assert_eq!(short_sha("abcdef1234"), "abcdef1");
    }
}
