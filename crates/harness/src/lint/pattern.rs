//! Lint patterns, written in the syntax of the `regex` crate plus the look-around an installation's
//! rules have always used: each top-level alternative may open with look-behinds `(?<!…)` / `(?<=…)`
//! of a fixed width and close with look-aheads `(?!…)` / `(?=…)`.
//!
//! The engine has no look-around, so each such alternative is a plain regex whose match is then
//! checked with anchored searches over the full line, which keep `\b` and the like seeing the text
//! around them. A match the checks reject is not retried at a shorter length from the same start;
//! the core rules are written so no such retry could succeed. Look-around anywhere else, and
//! backreferences, do not compile.

use regex_automata::meta::Regex;
use regex_automata::{Anchored, Input};
use regex_syntax::hir::{Hir, HirKind};

/// A compiled lint pattern.
pub struct Pattern {
    alternatives: Vec<Alternative>,
}

struct Alternative {
    body: Regex,
    behind: Vec<Behind>,
    ahead: Vec<Ahead>,
}

struct Behind {
    negative: bool,
    /// In characters: the look-behind starts exactly this far before the match.
    width: usize,
    regex: Regex,
}

struct Ahead {
    negative: bool,
    regex: Regex,
}

impl Pattern {
    /// Compiles `source`; the error says why it cannot be used.
    pub fn new(source: &str) -> Result<Self, String> {
        let (flags, rest) = split_flags(source);
        let parts = top_level_alternatives(rest);
        let guarded = parts
            .as_ref()
            .is_some_and(|parts| parts.iter().any(|part| has_look_around(part)));
        let alternatives = match parts {
            Some(parts) if guarded => parts
                .into_iter()
                .map(|part| Alternative::new(flags, part))
                .collect::<Result<_, _>>()?,
            _ => vec![Alternative {
                body: compile(source)?,
                behind: Vec::new(),
                ahead: Vec::new(),
            }],
        };
        Ok(Self { alternatives })
    }

    /// Every match in `line`, left to right and not overlapping, as byte ranges.
    pub fn find_iter(&self, line: &str) -> Vec<(usize, usize)> {
        // Each alternative's first match at or after some earlier position: still its first at or
        // after the current one while it starts there or later.
        let mut cached: Vec<Option<Option<(usize, usize)>>> = vec![None; self.alternatives.len()];
        let mut found = Vec::new();
        let mut pos = 0;
        while pos <= line.len() {
            let mut best: Option<(usize, usize)> = None;
            for (alternative, cache) in self.alternatives.iter().zip(cached.iter_mut()) {
                let candidate = match *cache {
                    Some(Some(hit)) if hit.0 >= pos => Some(hit),
                    Some(None) => None,
                    _ => {
                        let hit = alternative.first_from(line, pos);
                        *cache = Some(hit);
                        hit
                    }
                };
                // Alternatives are tried in order at each start, so a tie goes to the earlier one.
                if let Some(hit) = candidate
                    && best.is_none_or(|(start, _)| hit.0 < start)
                {
                    best = Some(hit);
                }
            }
            let Some((start, end)) = best else { break };
            found.push((start, end));
            pos = if end > start {
                end
            } else {
                match next_char(line, end) {
                    Some(next) => next,
                    None => break,
                }
            };
        }
        found
    }
}

impl Alternative {
    fn new(flags: &str, mut part: &str) -> Result<Self, String> {
        let mut behind = Vec::new();
        while let Some(negative) = look_behind_start(part) {
            let close = matching_close(part, 0).ok_or("unbalanced parenthesis")?;
            let inner = &part[4..close];
            let width = fixed_width(flags, inner)?;
            behind.push(Behind {
                negative,
                width,
                regex: compile(&format!("{flags}(?:{inner})"))?,
            });
            part = &part[close + 1..];
        }
        let mut ahead = Vec::new();
        while let Some((open, negative)) = trailing_look_ahead(part) {
            let inner = &part[open + 3..part.len() - 1];
            ahead.push(Ahead {
                negative,
                regex: compile(&format!("{flags}(?:{inner})"))?,
            });
            part = &part[..open];
        }
        Ok(Self {
            body: compile(&format!("{flags}(?:{part})"))?,
            behind,
            ahead,
        })
    }

    /// The first match at or after `pos` that its look-around allows.
    fn first_from(&self, line: &str, mut pos: usize) -> Option<(usize, usize)> {
        loop {
            let hit = self.body.search(&Input::new(line).span(pos..line.len()))?;
            if self.allows(line, hit.start(), hit.end()) {
                return Some((hit.start(), hit.end()));
            }
            pos = next_char(line, hit.start())?;
        }
    }

    fn allows(&self, line: &str, start: usize, end: usize) -> bool {
        self.behind.iter().all(|look| {
            let matched = back_chars(line, start, look.width).is_some_and(|from| {
                look.regex
                    .search(&Input::new(line).span(from..start).anchored(Anchored::Yes))
                    .is_some_and(|hit| hit.end() == start)
            });
            matched != look.negative
        }) && self.ahead.iter().all(|look| {
            let matched = look.regex.is_match(
                Input::new(line)
                    .span(end..line.len())
                    .anchored(Anchored::Yes),
            );
            matched != look.negative
        })
    }
}

fn compile(pattern: &str) -> Result<Regex, String> {
    Regex::new(pattern).map_err(|error| {
        // The parser's message spans lines (the pattern, a caret, the reason); keep the reason.
        let text = error.to_string();
        text.lines()
            .find_map(|line| line.strip_prefix("error: "))
            .map_or_else(|| text.replace('\n', " "), str::to_owned)
    })
}

/// A leading group of inline flags, `(?i)`, applies to every alternative once they are split.
fn split_flags(source: &str) -> (&str, &str) {
    if let Some(rest) = source.strip_prefix("(?")
        && let Some(close) = rest.find(')')
        && close > 0
        && rest[..close].bytes().all(|b| b.is_ascii_alphabetic())
    {
        return source.split_at(close + 3);
    }
    ("", source)
}

fn has_look_around(part: &str) -> bool {
    look_behind_start(part).is_some() || trailing_look_ahead(part).is_some()
}

fn look_behind_start(part: &str) -> Option<bool> {
    if part.starts_with("(?<!") {
        Some(true)
    } else if part.starts_with("(?<=") {
        Some(false)
    } else {
        None
    }
}

/// The opening index of a look-ahead that ends `part`, and whether it is negative.
fn trailing_look_ahead(part: &str) -> Option<(usize, bool)> {
    let events = structure(part)?;
    let last = part.len().checked_sub(1)?;
    let mut opens = Vec::new();
    for (index, event) in events {
        match event {
            Event::Open => opens.push(index),
            Event::Close => {
                let open = opens.pop()?;
                if index == last {
                    let group = &part[open..];
                    return if group.starts_with("(?!") {
                        Some((open, true))
                    } else if group.starts_with("(?=") {
                        Some((open, false))
                    } else {
                        None
                    };
                }
            }
            Event::Bar => {}
        }
    }
    None
}

fn matching_close(part: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, event) in structure(part)? {
        if index < open {
            continue;
        }
        match event {
            Event::Open => depth += 1,
            Event::Close => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            Event::Bar => {}
        }
    }
    None
}

/// The pattern split on its top-level `|`, or `None` when its parentheses do not balance.
fn top_level_alternatives(pattern: &str) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut from = 0;
    for (index, event) in structure(pattern)? {
        match event {
            Event::Open => depth += 1,
            Event::Close => depth = depth.checked_sub(1)?,
            Event::Bar if depth == 0 => {
                parts.push(&pattern[from..index]);
                from = index + 1;
            }
            Event::Bar => {}
        }
    }
    (depth == 0).then(|| {
        parts.push(&pattern[from..]);
        parts
    })
}

#[derive(Clone, Copy)]
enum Event {
    Open,
    Close,
    Bar,
}

/// The positions of the unescaped `(`, `)` and `|` outside character classes, or `None` when a
/// class is left open.
fn structure(pattern: &str) -> Option<Vec<(usize, Event)>> {
    let bytes = pattern.as_bytes();
    let mut events = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 1,
            b'[' => index = class_end(bytes, index)?,
            b'(' => events.push((index, Event::Open)),
            b')' => events.push((index, Event::Close)),
            b'|' => events.push((index, Event::Bar)),
            _ => {}
        }
        index += 1;
    }
    Some(events)
}

/// The index of the `]` closing the class opened at `open`, nested classes included.
fn class_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 1,
            b'[' => {
                depth += 1;
                // A `]` right after the opening bracket (or its `^`) is a member, not the end.
                if bytes.get(index + 1) == Some(&b'^') {
                    index += 1;
                }
                if bytes.get(index + 1) == Some(&b']') {
                    index += 1;
                }
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// A look-behind's width in characters; one that can match more than one width is refused.
fn fixed_width(flags: &str, inner: &str) -> Result<usize, String> {
    let hir = regex_syntax::parse(&format!("{flags}(?:{inner})")).map_err(|error| {
        let text = error.to_string();
        text.lines()
            .find_map(|line| line.strip_prefix("error: "))
            .map_or_else(|| text.replace('\n', " "), str::to_owned)
    })?;
    match width(&hir) {
        (min, Some(max)) if min == max => Ok(min),
        _ => Err("look-behind requires fixed-width pattern".to_owned()),
    }
}

/// The fewest and most characters `hir` can match; `None` for no upper bound.
fn width(hir: &Hir) -> (usize, Option<usize>) {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => (0, Some(0)),
        HirKind::Literal(literal) => {
            let chars =
                std::str::from_utf8(&literal.0).map_or(literal.0.len(), |s| s.chars().count());
            (chars, Some(chars))
        }
        HirKind::Class(_) => (1, Some(1)),
        HirKind::Repetition(repetition) => {
            let (min, max) = width(&repetition.sub);
            let times = usize::try_from(repetition.min).unwrap_or(usize::MAX);
            let most = repetition
                .max
                .map(|m| usize::try_from(m).unwrap_or(usize::MAX));
            let upper = match (max, most) {
                (Some(0), _) => Some(0),
                (Some(max), Some(most)) => max.checked_mul(most),
                _ => None,
            };
            (min.saturating_mul(times), upper)
        }
        HirKind::Capture(capture) => width(&capture.sub),
        HirKind::Concat(subs) => subs
            .iter()
            .map(width)
            .fold((0, Some(0)), |(min, max), (a, b)| {
                (
                    min.saturating_add(a),
                    max.zip(b).and_then(|(x, y)| x.checked_add(y)),
                )
            }),
        HirKind::Alternation(subs) => {
            let widths: Vec<_> = subs.iter().map(width).collect();
            let min = widths.iter().map(|w| w.0).min().unwrap_or(0);
            let max = widths
                .iter()
                .try_fold(0, |most: usize, w| w.1.map(|b| most.max(b)));
            (min, max)
        }
    }
}

fn next_char(line: &str, at: usize) -> Option<usize> {
    line[at..].chars().next().map(|c| at + c.len_utf8())
}

/// The byte index `count` characters before `at`, if the line has that many.
fn back_chars(line: &str, at: usize, count: usize) -> Option<usize> {
    let mut index = at;
    for _ in 0..count {
        index -= line[..index].chars().next_back()?.len_utf8();
    }
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::Pattern;

    fn matches(pattern: &str, line: &str) -> Vec<String> {
        let pattern = Pattern::new(pattern).expect("the test pattern compiles");
        pattern
            .find_iter(line)
            .into_iter()
            .map(|(start, end)| line[start..end].to_owned())
            .collect()
    }

    #[test]
    fn a_plain_pattern_finds_every_match() {
        assert_eq!(matches(r"\bT[1-9]\b", "T1 and T2, not T10"), ["T1", "T2"]);
    }

    #[test]
    fn a_look_behind_sees_the_text_before_the_search_start() {
        assert_eq!(
            matches(r"(?<![\w./-])/tmp/", "/tmp/a x/tmp/b /tmp/c"),
            ["/tmp/", "/tmp/"]
        );
        assert_eq!(matches(r"(?<=a)b", "ab cb ab"), ["b", "b"]);
    }

    #[test]
    fn a_look_behind_keeps_its_word_boundary() {
        let pattern = r"(?<!\b[Vv] )\d";
        assert_eq!(matches(pattern, "v 1"), Vec::<String>::new());
        assert_eq!(matches(pattern, "dev 1"), ["1"]);
    }

    #[test]
    fn a_look_ahead_rejects_what_follows() {
        assert_eq!(
            matches(
                r"(?<![\w./-])/tmp/(?!app\.log\b)",
                "/tmp/app.log /tmp/app.logs"
            ),
            ["/tmp/"]
        );
        assert_eq!(matches(r"a(?=b)", "ac ab"), ["a"]);
    }

    #[test]
    fn a_rejected_alternative_gives_way_to_the_next_at_the_same_start() {
        assert_eq!(
            matches(
                r"(?<![\w.-])/a/b(?![\w.-])|(?<![\w.-])/a(?![\w.-])",
                "/a/bc"
            ),
            ["/a"]
        );
    }

    #[test]
    fn leading_flags_apply_to_each_alternative() {
        assert_eq!(matches(r"(?i)(?<!x)ab|cd", "AB xAB CD"), ["AB", "CD"]);
    }

    #[test]
    fn unsupported_patterns_do_not_compile() {
        for pattern in ["(", r"a(?<!b)c", r"(a)\1", r"(?<!a+)b", r"[a"] {
            assert!(Pattern::new(pattern).is_err(), "{pattern}");
        }
    }

    #[test]
    fn a_class_holding_parentheses_or_bars_does_not_split() {
        assert_eq!(matches(r"(?<!x)[(|)]+", "(|) x("), ["(|)"]);
    }
}
