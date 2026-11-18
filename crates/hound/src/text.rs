//! Making text from transcripts and other machines safe to show on a
//! terminal: one character set, used by the command line and the popup.

/// Characters that must not reach a terminal as they are: control characters
/// (C0, DEL, C1, line breaks and tabs included) and invisible format or
/// bidirectional controls, which could send escape sequences or reorder what
/// is shown.
pub fn is_unprintable(c: char) -> bool {
    c.is_control() || is_format_control(c)
}

/// `s` made safe to print on one terminal line: line breaks and tabs become
/// spaces, and other [unprintable](is_unprintable) characters become U+FFFD.
pub fn printable(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            c if is_unprintable(c) => '\u{FFFD}',
            c => c,
        })
        .collect()
}

/// Invisible formatting characters: soft hyphen, bidirectional marks,
/// embeddings, overrides and isolates, zero-width characters and the BOM.
fn is_format_control(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_neutralises_controls() {
        assert_eq!(printable("a\nb\r\tc"), "a b  c");
        assert_eq!(
            printable("x\u{1b}]0;t\u{7}y\u{9b}z\u{202e}w\u{200b}v\u{7f}"),
            "x\u{FFFD}]0;t\u{FFFD}y\u{FFFD}z\u{FFFD}w\u{FFFD}v\u{FFFD}"
        );
        assert_eq!(printable("caf\u{e9} \u{2026} ok"), "caf\u{e9} \u{2026} ok");
    }

    #[test]
    fn unprintable_set() {
        for c in [
            '\n', '\u{1b}', '\u{7f}', '\u{9b}', '\u{202e}', '\u{2066}', '\u{200d}', '\u{feff}',
        ] {
            assert!(is_unprintable(c), "U+{:04X}", c as u32);
        }
        for c in ['a', ' ', '\u{e9}', '\u{2026}', '漢', '🦀', '\u{1F3FD}'] {
            assert!(!is_unprintable(c), "U+{:04X}", c as u32);
        }
    }
}
