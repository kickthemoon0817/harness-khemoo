//! Text helpers shared by the parsers, the collector and the page: line splitting, numbers read
//! from untrusted text, UTC times, and the number formats the page prints.

use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

/// Splits text into lines at every line boundary: LF, CRLF, CR, VT, FF, the file, group and
/// record separators, NEL and the Unicode line and paragraph separators.
pub fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        let boundary = matches!(
            character,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !boundary {
            continue;
        }
        lines.push(&text[start..index]);
        let mut end = index + character.len_utf8();
        if character == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
            chars.next();
            end += 1;
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// A word character: a letter, a number or `_`. The `regex` crate's `\w` also takes combining
/// marks, connector punctuation and the zero-width joiners, which must not hide a secret from a
/// pattern that starts at a word boundary.
pub fn is_word_char(character: char) -> bool {
    static WORD: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^[\p{L}\p{N}_]$").expect("a constant pattern compiles")
    });
    let mut buffer = [0u8; 4];
    WORD.is_match(character.encode_utf8(&mut buffer))
}

/// The SHA-256 of some bytes, in lower-case hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            write!(hex, "{byte:02x}").ok();
            hex
        })
}

/// The last path component, empty after a trailing slash.
pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A count, pid, issue or epoch of at most twelve ASCII digits; anything else is no number, so a
/// huge digit string or a non-ASCII digit never becomes one.
pub fn optional_int(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.len() > 12
        || !trimmed.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    trimmed.parse().ok()
}

/// A decimal number; `-` and any other non-number read as none.
pub fn optional_float(text: &str) -> Option<f64> {
    text.trim().parse().ok()
}

/// ISO-8601 UTC to the second.
pub fn iso(epoch: f64) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let seconds = epoch.floor() as i64;
    let moment = OffsetDateTime::from_unix_timestamp(seconds).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute(),
        moment.second()
    )
}

/// The UTC moment of an epoch, for the page's clock.
pub fn utc(epoch: f64) -> OffsetDateTime {
    #[allow(clippy::cast_possible_truncation)]
    let seconds = epoch.floor() as i64;
    OffsetDateTime::from_unix_timestamp(seconds).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// Reads an ISO-8601 time: `YYYY-MM-DD`, optionally `T` or a space and `HH:MM[:SS[.ffffff]]`, then
/// `Z` or an offset; a time without an offset is UTC.
pub fn parse_iso(text: &str) -> Option<f64> {
    let text = text.trim_end();
    let (date_text, rest) = (text.get(..10)?, &text[10..]);
    let date = parse_date(date_text)?;
    if rest.is_empty() {
        return Some(seconds_of(
            PrimitiveDateTime::new(date, Time::MIDNIGHT),
            UtcOffset::UTC,
        ));
    }
    let mut rest_chars = rest.chars();
    let separator = rest_chars.next()?;
    if separator != 'T' && separator != ' ' {
        return None;
    }
    let rest = rest_chars.as_str();
    let offset_start = rest.find(['Z', '+', '-']).unwrap_or(rest.len());
    let (time_text, offset_text) = rest.split_at(offset_start);
    let (time, fraction) = parse_time(time_text)?;
    let offset = parse_offset(offset_text)?;
    Some(seconds_of(PrimitiveDateTime::new(date, time), offset) + fraction)
}

fn seconds_of(moment: PrimitiveDateTime, offset: UtcOffset) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let seconds = moment.assume_offset(offset).unix_timestamp() as f64;
    seconds
}

fn digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn parse_date(text: &str) -> Option<Date> {
    let bytes = text.as_bytes();
    if bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return None;
    }
    let year = i32::try_from(digits(text.get(0..4)?)?).ok()?;
    let month = Month::try_from(u8::try_from(digits(text.get(5..7)?)?).ok()?).ok()?;
    let day = u8::try_from(digits(text.get(8..10)?)?).ok()?;
    Date::from_calendar_date(year, month, day).ok()
}

fn parse_time(text: &str) -> Option<(Time, f64)> {
    let (clock, fraction_text) = match text.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (text, None),
    };
    let mut parts = clock.split(':');
    let hour = parse_two(parts.next()?)?;
    let minute = parts.next().map_or(Some(0), parse_two)?;
    let second = parts.next().map_or(Some(0), parse_two)?;
    if parts.next().is_some() {
        return None;
    }
    let fraction = match fraction_text {
        Some(fraction) if fraction.len() <= 6 => {
            let exponent = i32::try_from(fraction.len()).unwrap_or(6);
            f64::from(digits(fraction)?) / 10f64.powi(exponent)
        }
        Some(_) => return None,
        None => 0.0,
    };
    Some((Time::from_hms(hour, minute, second).ok()?, fraction))
}

fn parse_two(text: &str) -> Option<u8> {
    if text.len() != 2 {
        return None;
    }
    u8::try_from(digits(text)?).ok()
}

fn parse_offset(text: &str) -> Option<UtcOffset> {
    if text.is_empty() || text == "Z" {
        return Some(UtcOffset::UTC);
    }
    let sign: i8 = if text.starts_with('-') { -1 } else { 1 };
    let body = text.get(1..)?;
    let (hours, minutes) = match body.split_once(':') {
        Some((hours, minutes)) => (hours, minutes),
        None if body.len() == 4 => body.split_at(2),
        None => (body, "00"),
    };
    let hours = i8::try_from(parse_two(hours)?).ok()?;
    let minutes = i8::try_from(parse_two(minutes)?).ok()?;
    UtcOffset::from_hms(sign * hours, sign * minutes, 0).ok()
}

/// A float as the page writes it: the shortest digits that read back the same, `.0` on a
/// whole number, and an exponent of at least two digits past 1e16 and under 1e-4.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    let text = format!("{value:?}");
    let Some((mantissa, exponent)) = text.split_once('e') else {
        return text;
    };
    let (sign, digits) = match exponent.strip_prefix('-') {
        Some(digits) => ('-', digits),
        None => ('+', exponent),
    };
    format!("{mantissa}e{sign}{digits:0>2}")
}

/// A float in six significant digits, trailing zeros dropped.
pub fn float_g(value: f64) -> String {
    if !value.is_finite() {
        return float_repr(value);
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    let scientific = format!("{value:.5e}");
    let Some((_, exponent_text)) = scientific.split_once('e') else {
        return scientific;
    };
    let exponent: i32 = exponent_text.parse().unwrap_or(0);
    if (-4..6).contains(&exponent) {
        let decimals = usize::try_from(5 - exponent).unwrap_or(0);
        let fixed = format!("{value:.decimals$}");
        if fixed.contains('.') {
            fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            fixed
        }
    } else {
        let (mantissa, _) = scientific.split_once('e').unwrap_or((&scientific, ""));
        let mantissa = if mantissa.contains('.') {
            mantissa.trim_end_matches('0').trim_end_matches('.')
        } else {
            mantissa
        };
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.abs())
    }
}

/// Seconds since an epoch, whole and never negative, or none.
pub fn age(epoch: Option<f64>, now: f64) -> Option<i64> {
    #[allow(clippy::cast_possible_truncation)]
    epoch.map(|epoch| ((now - epoch).max(0.0)) as i64)
}

/// The text cut to `limit` characters, an ellipsis standing for the rest.
pub fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(limit.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

/// The last `limit` characters of a text.
pub fn tail_chars(text: &str, limit: usize) -> &str {
    let count = text.chars().count();
    if count <= limit {
        return text;
    }
    let skip = count - limit;
    let start = text
        .char_indices()
        .nth(skip)
        .map_or(text.len(), |(index, _)| index);
    &text[start..]
}

/// The first `limit` characters of a text.
pub fn head_chars(text: &str, limit: usize) -> &str {
    let end = text
        .char_indices()
        .nth(limit)
        .map_or(text.len(), |(index, _)| index);
    &text[..end]
}

/// Bidi controls and the invisible characters that join nothing: they can make text read in
/// another order than it is, or hide a difference between two names. The zero-width joiners,
/// variation selectors and tag characters stay: emoji and scripts such as Persian need them.
fn hides_text(character: char) -> bool {
    matches!(
        character,
        '\u{061C}'
            | '\u{200B}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// Joiners and selectors that change how text looks but not what it says.
pub fn joins_glyphs(character: char) -> bool {
    matches!(character, '\u{200C}' | '\u{200D}' | '\u{FE00}'..='\u{FE0F}' | '\u{E0000}'..='\u{E007F}')
}

/// Text from a state file, a command or GitHub, made safe to show: every control character but
/// tab and line breaks, every bidi control and every invisible character that joins nothing
/// becomes U+FFFD, so no text can carry a terminal escape or read differently from what it is.
pub fn clean(text: &str) -> std::borrow::Cow<'_, str> {
    let replaced = |character: char| {
        (character.is_control() && !matches!(character, '\t' | '\n' | '\r'))
            || hides_text(character)
    };
    if !text.chars().any(replaced) {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(
        text.chars()
            .map(|character| {
                if replaced(character) {
                    '\u{FFFD}'
                } else {
                    character
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_at_every_boundary() {
        assert_eq!(
            split_lines("a\nb\r\nc\rd\u{0c}e\n"),
            ["a", "b", "c", "d", "e"]
        );
        assert_eq!(split_lines(""), Vec::<&str>::new());
        assert_eq!(split_lines("a\n\nb"), ["a", "", "b"]);
    }

    #[test]
    fn iso_times_round_trip() {
        assert_eq!(parse_iso("2026-11-26T01:40:00Z"), Some(1_795_657_200.0));
        assert_eq!(
            parse_iso("2026-11-26T03:40:00+02:00"),
            Some(1_795_657_200.0)
        );
        assert_eq!(parse_iso("2026-11-26T01:40:00"), Some(1_795_657_200.0));
        assert_eq!(parse_iso("2026-11-26"), Some(1_795_651_200.0));
        assert_eq!(parse_iso("2026-02-30T00:00:00Z"), None);
        assert_eq!(parse_iso("soon"), None);
        assert_eq!(iso(1_795_657_200.9), "2026-11-26T01:40:00Z");
    }

    #[test]
    fn numbers_print_in_the_page_formats() {
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1e-5), "1e-05");
        assert_eq!(float_repr(30.0), "30.0");
        assert_eq!(float_repr(0.05), "0.05");
        assert_eq!(float_g(6.0), "6");
        assert_eq!(float_g(0.5), "0.5");
        assert_eq!(float_g(1_234_567.0), "1.23457e+06");
        assert_eq!(optional_int(&"9".repeat(5000)), None);
        assert_eq!(optional_int(" 42 "), Some(42));
        assert_eq!(optional_int("\u{b2}"), None);
    }

    #[test]
    fn clean_keeps_emoji_and_joined_scripts() {
        let title = "\u{26a0}\u{fe0f} Fix the \u{1f469}\u{200d}\u{1f4bb} page; \u{646}\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}";
        assert_eq!(clean(title), title);
        assert_eq!(
            clean("a\u{200b}b\u{feff}c\u{2066}d"),
            "a\u{fffd}b\u{fffd}c\u{fffd}d"
        );
    }

    #[test]
    fn clean_replaces_escapes_and_bidi() {
        assert_eq!(
            clean("a\u{1b}[2Jb\u{202e}c\td"),
            "a\u{fffd}[2Jb\u{fffd}c\td"
        );
        assert!(matches!(clean("plain"), std::borrow::Cow::Borrowed(_)));
    }
}
