//! Lint patterns, written in Python's `re` syntax and run by `fancy-regex`.
//!
//! The two engines disagree on what a word character is: Rust's `\w` and `\b` also count
//! combining marks, connector punctuation and the zero-width joiners, so `x\u{200d}/tmp/a` would
//! pass a rule that Python blocks. A pattern is therefore translated before it compiles: `\w` and
//! `\W` become explicit classes of letters, digits and the underscore, and `\b` and `\B` become
//! look-around on that class. The bracket-class operators only Rust knows (`--`, `&&`, `~~`) are
//! refused, and a `[` inside a class is a literal, as in Python.

use fancy_regex::{Regex, RegexBuilder};

/// Python's word characters: what `str.isalnum()` accepts, and the underscore.
const WORD: &str = r"\p{L}\p{N}_";
/// The class of everything else.
const NOT_WORD: &str = r"[^\p{L}\p{N}_]";

/// How many backtracking steps one search may take. A body that needs more is not passed: the
/// search's failure is reported as a blocking finding.
const BACKTRACK_LIMIT: usize = 1_000_000;

/// A compiled lint pattern.
pub struct Pattern {
    regex: Regex,
}

impl Pattern {
    /// Compiles `source`; the error says why it cannot be used.
    pub fn new(source: &str) -> Result<Self, String> {
        let translated = translate(source)?;
        let regex = RegexBuilder::new(&translated)
            .backtrack_limit(BACKTRACK_LIMIT)
            .build()
            .map_err(|error| reason(&error))?;
        Ok(Self { regex })
    }

    /// Every match in `line`, left to right and not overlapping, as byte ranges; an error when the
    /// search could not finish.
    pub fn find_iter(&self, line: &str) -> Result<Vec<(usize, usize)>, String> {
        self.regex
            .find_iter(line)
            .map(|hit| {
                hit.map(|hit| (hit.start(), hit.end()))
                    .map_err(|error| error.to_string())
            })
            .collect()
    }
}

/// Why a pattern did not compile. For a part the engine hands to `regex`, its error only names
/// the pattern; the parser's own error says what is wrong, on the line that starts `error: `.
fn reason(error: &fancy_regex::Error) -> String {
    if let fancy_regex::Error::CompileError(fancy_regex::CompileError::InnerError(inner)) = error {
        let text = inner
            .syntax_error()
            .map_or_else(|| inner.to_string(), ToString::to_string);
        return text
            .lines()
            .find_map(|line| line.strip_prefix("error: "))
            .map_or_else(|| text.replace('\n', " "), str::to_owned);
    }
    error.to_string()
}

/// `source` with Python's meaning made explicit for `fancy-regex`.
fn translate(source: &str) -> Result<String, String> {
    let wordy = format!("[{WORD}]");
    let boundary = format!("(?:(?<={wordy})(?!{wordy})|(?<!{wordy})(?={wordy}))");
    let inside = format!("(?:(?<={wordy})(?={wordy})|(?<!{wordy})(?!{wordy}))");
    let mut out = String::with_capacity(source.len() * 2);
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('w') => out.push_str(&wordy),
                Some('W') => out.push_str(NOT_WORD),
                Some('b') => out.push_str(&boundary),
                Some('B') => out.push_str(&inside),
                Some('Z') => out.push_str(r"\z"),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => return Err("a trailing backslash".to_owned()),
            },
            '[' => translate_class(&mut chars, &mut out)?,
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// One bracket class, its opening `[` already read, by Python's rules.
fn translate_class(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    out: &mut String,
) -> Result<(), String> {
    out.push('[');
    if chars.peek() == Some(&'^') {
        chars.next();
        out.push('^');
    }
    // A `]` first in the class is a member.
    if chars.peek() == Some(&']') {
        chars.next();
        out.push_str(r"\]");
    }
    let mut previous: Option<char> = None;
    while let Some(c) = chars.next() {
        match c {
            ']' => {
                out.push(']');
                return Ok(());
            }
            '\\' => {
                match chars.next() {
                    Some('w') => out.push_str(WORD),
                    Some('W') => out.push_str(NOT_WORD),
                    // Inside a class Python's `\b` is the backspace.
                    Some('b') => out.push_str(r"\x08"),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => return Err("a trailing backslash".to_owned()),
                }
                previous = None;
                continue;
            }
            '[' => out.push_str(r"\["),
            '-' | '&' | '~' if previous == Some(c) => {
                return Err(format!(
                    "`{c}{c}` in a bracket class is a set operation here, not two characters: escape them (\\{c}\\{c})"
                ));
            }
            _ => out.push(c),
        }
        previous = Some(c);
    }
    Err("an unterminated character class".to_owned())
}

#[cfg(test)]
mod tests {
    use super::Pattern;

    fn matches(pattern: &str, line: &str) -> Vec<String> {
        let pattern = Pattern::new(pattern).expect("the test pattern compiles");
        pattern
            .find_iter(line)
            .expect("the search finishes")
            .into_iter()
            .map(|(start, end)| line[start..end].to_owned())
            .collect()
    }

    #[test]
    fn a_plain_pattern_finds_every_match() {
        assert_eq!(matches(r"\bT[1-9]\b", "T1 and T2, not T10"), ["T1", "T2"]);
    }

    #[test]
    fn word_characters_are_pythons() {
        // A combining mark, a joiner or connector punctuation is not a word character, so a word
        // boundary or a look-behind on `\w` still holds next to one.
        for (pattern, line, hit) in [
            (r"(?<![\w./-])/tmp/", "x\u{200d}/tmp/a", "/tmp/"),
            (r"(?<![\w./-])/tmp/", "x\u{203f}/tmp/a", "/tmp/"),
            (r"\bpid \d+\b", "\u{200d}pid 123", "pid 123"),
            (r"\bpid \d+\b", "e\u{301}\u{301}pid 123", "pid 123"),
            (r"(?<![\w./-])/home/\w+", "a\u{331}/home/bob", "/home/bob"),
            (r"(?<![\w./-])~/", "\u{200c}~/foo", "~/"),
        ] {
            assert_eq!(matches(pattern, line), [hit], "{pattern} on {line:?}");
        }
        assert_eq!(matches(r"\w+", "é東9_"), ["é東9_"]);
        assert_eq!(matches(r"\Bb", "ab b"), ["b"]);
    }

    #[test]
    fn a_rejected_look_ahead_retries_shorter() {
        assert_eq!(matches(r"corp-\d+(?!\.pub)", "corp-12.pub"), ["corp-1"]);
        assert_eq!(matches(r"\d+(?!x)", "12x"), ["1"]);
        assert_eq!(matches(r"(?:ab|abc)(?!c)", "abc"), ["abc"]);
        assert_eq!(matches(r"(?:abc|ab)(?!d)", "abcd"), ["ab"]);
    }

    #[test]
    fn look_behind_keeps_its_word_boundary() {
        let pattern = r"(?<!\b[Vv] )\d";
        assert_eq!(matches(pattern, "v 1"), Vec::<String>::new());
        assert_eq!(matches(pattern, "dev 1"), ["1"]);
    }

    #[test]
    fn a_class_reads_as_in_python() {
        assert_eq!(matches(r"[]a]+", "]a]b"), ["]a]"]);
        assert_eq!(matches(r"[[]", "a[b"), ["["]);
        assert_eq!(matches(r"[\w.]+", "a\u{301}.b"), ["a", ".b"]);
    }

    #[test]
    fn unusable_patterns_are_refused_with_a_reason() {
        for (pattern, reason) in [
            ("(", ""),
            (r"[a-z--x]", "escape"),
            (r"[a&&b]", "escape"),
            (r"[a~~b]", "escape"),
            ("[a", "unterminated"),
            ("a\\", "backslash"),
            ("a{2,1}", "repetition"),
        ] {
            let error = Pattern::new(pattern).err().unwrap_or_default();
            assert!(
                !error.is_empty() && error.contains(reason),
                "{pattern}: {error}"
            );
        }
        assert!(Pattern::new(r"[a\-\-b]").is_ok());
    }

    #[test]
    fn a_search_past_the_backtrack_limit_is_an_error() {
        let pattern = Pattern::new(r"(?:(?=a)a|a)+b").expect("compiles");
        assert!(pattern.find_iter(&"a".repeat(40)).is_err());
    }
}
