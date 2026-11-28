//! A matched string as the lint prints it: quoted, with every character a reader could not see
//! (controls, bidi controls, invisible and non-space blanks) written as an escape, so a hit that
//! hides in such characters is still shown.

use std::fmt::Write as _;

/// `text` quoted as a Python literal: single quotes unless it holds one and no double quote.
pub fn quoted(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if c == quote => {
                out.push('\\');
                out.push(c);
            }
            _ if printable(c) => out.push(c),
            _ => {
                let code = u32::from(c);
                // Writing to a String cannot fail.
                let _ = match code {
                    0..=0xff => write!(out, "\\x{code:02x}"),
                    0x100..=0xffff => write!(out, "\\u{code:04x}"),
                    _ => write!(out, "\\U{code:08x}"),
                };
            }
        }
    }
    out.push(quote);
    out
}

/// False for the controls (Cc), the format characters (Cf, the bidi controls among them), the
/// separators other than the space (Zs, Zl, Zp), the private-use planes (Co) and the
/// noncharacters.
fn printable(c: char) -> bool {
    let code = u32::from(c);
    !(matches!(code,
        0x00..=0x1f | 0x7f..=0xa0 | 0xad
        | 0x600..=0x605 | 0x61c | 0x6dd | 0x70f | 0x890..=0x891 | 0x8e2
        | 0x1680 | 0x180e
        | 0x2000..=0x200f | 0x2028..=0x202f | 0x205f..=0x206f
        | 0x3000 | 0xe000..=0xf8ff | 0xfdd0..=0xfdef | 0xfeff | 0xfff9..=0xfffb
        | 0x1_10bd | 0x1_10cd | 0x1_3430..=0x1_343f | 0x1_bca0..=0x1_bca3 | 0x1_d173..=0x1_d17a
        | 0xe_0001 | 0xe_0020..=0xe_007f | 0xf_0000..=0x10_ffff)
        || code & 0xfffe == 0xfffe)
}

#[cfg(test)]
mod tests {
    use super::quoted;

    #[test]
    fn plain_text_is_single_quoted() {
        assert_eq!(quoted("## Summary"), "'## Summary'");
    }

    #[test]
    fn a_single_quote_switches_to_double_quotes() {
        assert_eq!(quoted("it's"), "\"it's\"");
        assert_eq!(quoted("it's \"x\""), "'it\\'s \"x\"'");
    }

    #[test]
    fn invisible_and_bidi_characters_are_escaped() {
        assert_eq!(
            quoted("a\u{202e}b\u{200b}c\u{feff}"),
            "'a\\u202eb\\u200bc\\ufeff'"
        );
        assert_eq!(quoted("\u{7}\u{a0}\\"), "'\\x07\\xa0\\\\'");
        assert_eq!(quoted("\u{e0041}"), "'\\U000e0041'");
        assert_eq!(quoted("é ü 東"), "'é ü 東'");
    }
}
