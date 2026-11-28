//! Reads named keys from the instance's harness.env without sourcing it.
//!
//! The file is never executed or echoed: each line is matched as a plain `KEY=value` assignment,
//! only the requested keys are kept, and values are returned verbatim, unquoted but not
//! expanded. The last assignment of a key wins, as it would when the shell sources the file.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::files::read_text;
use super::text::split_lines;

static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$")
        .expect("a constant pattern compiles")
});
static COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s#").expect("a constant pattern compiles"));

/// The wanted keys the file assigns, each to its unquoted value; a missing or unreadable file
/// yields none.
pub fn read_keys(env_path: &Path, wanted_keys: &[&str]) -> HashMap<String, String> {
    let Ok(text) = read_text(env_path) else {
        return HashMap::new();
    };
    let mut values = HashMap::new();
    for line in split_lines(&text) {
        let Some(found) = ASSIGNMENT.captures(line) else {
            continue;
        };
        let (Some(key), Some(value)) = (found.get(1), found.get(2)) else {
            continue;
        };
        if wanted_keys.contains(&key.as_str()) {
            values.insert(key.as_str().to_owned(), unquote(value.as_str()));
        }
    }
    values
}

/// Replaces `$NAME` and `${NAME}` for each placeholder name, and nothing else: the file is never
/// evaluated, so a value can only name the few things the dashboard already knows.
pub fn expand(value: &str, placeholders: &[(&str, String)]) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    'scan: while let Some(dollar) = rest.find('$') {
        output.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        for (name, replacement) in placeholders {
            if let Some(tail) = after
                .strip_prefix('{')
                .and_then(|inner| inner.strip_prefix(name))
                .and_then(|inner| inner.strip_prefix('}'))
            {
                output.push_str(replacement);
                rest = tail;
                continue 'scan;
            }
            if let Some(tail) = after.strip_prefix(name)
                && !tail
                    .chars()
                    .next()
                    .is_some_and(|next| next.is_ascii_alphanumeric() || next == '_')
            {
                output.push_str(replacement);
                rest = tail;
                continue 'scan;
            }
        }
        output.push('$');
        rest = after;
    }
    output.push_str(rest);
    output
}

/// An assignment's right-hand side as the shell would assign it, without expansion: shell
/// quoting and a trailing comment stripped.
fn unquote(raw_value: &str) -> String {
    let stripped = raw_value.trim();
    if let Some(quote) = stripped
        .chars()
        .next()
        .filter(|first| *first == '\'' || *first == '"')
    {
        let inner = &stripped[1..];
        return inner
            .find(quote)
            .map_or(inner, |closing| &inner[..closing])
            .to_owned();
    }
    match COMMENT.find(stripped) {
        Some(comment) => stripped[..comment.start()].trim_end().to_owned(),
        None => stripped.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::testkit::TempDir;

    #[test]
    fn reads_only_the_wanted_keys_unquoted() {
        let directory = TempDir::new();
        let env_path = directory.path().join("harness.env");
        std::fs::write(
            &env_path,
            "GH_REPO=first/repo\nexport GH_TOKEN='abc 123'\nGH_REPO=owner/name  # comment\nSECRET=zzz\n# GH_TOKEN=commented\n",
        )
        .ok();
        let values = read_keys(&env_path, &["GH_TOKEN", "GH_REPO"]);
        let expected: HashMap<String, String> =
            [("GH_TOKEN", "abc 123"), ("GH_REPO", "owner/name")]
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .into();
        assert_eq!(values, expected);
    }

    #[test]
    fn missing_file_reads_nothing() {
        assert!(read_keys(Path::new("/nonexistent/harness.env"), &["GH_TOKEN"]).is_empty());
    }

    #[test]
    fn only_the_named_placeholders_expand() {
        let placeholders = [("HARNESS_HOME", "/h".to_owned()), ("HOME", "/u".to_owned())];
        assert_eq!(
            expand(
                "$HARNESS_HOME/x ${HOME}/y $HARNESS_HOMEX $PATH $",
                &placeholders
            ),
            "/h/x /u/y $HARNESS_HOMEX $PATH $"
        );
    }
}
