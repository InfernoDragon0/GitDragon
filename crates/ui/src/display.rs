//! Text from repositories and hosts, made safe to draw: direction controls and other invisible
//! control characters become visible `<U+XXXX>` escapes.

use std::borrow::Cow;

/// Is `c` shown as an escape? `multiline` keeps `\n` and `\r`; `\t` is always kept.
fn needs_escape(c: char, multiline: bool) -> bool {
    match c {
        '\t' => false,
        '\n' | '\r' | '\u{2028}' | '\u{2029}' => !multiline,
        '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => true,
        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => true,
        '\u{200e}' | '\u{200f}' | '\u{61c}' => true,
        _ => false,
    }
}

fn escape(text: &str, multiline: bool) -> Cow<'_, str> {
    if !text.chars().any(|c| needs_escape(c, multiline)) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if needs_escape(c, multiline) {
            out.push_str(&format!("<U+{:04X}>", c as u32));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Text that may span lines (bodies, comments): line breaks and tabs stay, other control and
/// direction characters become `<U+XXXX>`. Borrows when nothing changes.
pub fn sanitize_display(text: &str) -> Cow<'_, str> {
    escape(text, true)
}

/// Single-line text (names, titles, paths): like [`sanitize_display`], and line breaks are
/// escaped too.
pub fn sanitize_line(text: &str) -> Cow<'_, str> {
    escape(text, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_borrowed() {
        assert!(matches!(
            sanitize_display("a\tb\nc é 日本"),
            Cow::Borrowed(_)
        ));
        assert!(matches!(sanitize_line("a\tb é"), Cow::Borrowed(_)));
    }

    #[test]
    fn direction_controls_are_escaped() {
        assert_eq!(sanitize_display("a\u{202e}b"), "a<U+202E>b");
        assert_eq!(sanitize_display("\u{2066}x\u{2069}"), "<U+2066>x<U+2069>");
        assert_eq!(
            sanitize_display("\u{200e}\u{200f}\u{61c}"),
            "<U+200E><U+200F><U+061C>"
        );
        assert_eq!(sanitize_display("\u{202a}\u{202d}"), "<U+202A><U+202D>");
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(sanitize_display("a\u{1b}[0m"), "a<U+001B>[0m");
        assert_eq!(sanitize_display("\0x\u{7f}"), "<U+0000>x<U+007F>");
        assert_eq!(sanitize_display("\u{80}\u{9f}"), "<U+0080><U+009F>");
        assert_eq!(sanitize_display("\u{a0}"), "\u{a0}");
    }

    #[test]
    fn line_breaks_depend_on_the_variant() {
        assert_eq!(sanitize_display("a\r\nb\tc"), "a\r\nb\tc");
        assert_eq!(sanitize_line("a\r\nb\tc"), "a<U+000D><U+000A>b\tc");
        assert_eq!(sanitize_line("a\u{2028}b"), "a<U+2028>b");
    }
}
