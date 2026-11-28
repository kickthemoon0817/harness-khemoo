//! The rules: the core every installation gets, and an installation's `lint_rules.json`.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::Value;

use super::pattern::Pattern;

const OCTET: &str = r"(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)";

/// Words a dotted quad may follow as a version, not an address.
pub const DEFAULT_VERSION_WORDS: [&str; 2] = ["v", "version"];

/// The label settings the launcher and the queue count read, with their defaults.
pub const LABEL_SETTINGS: [(&str, &str); 8] = [
    ("WIP_LABEL", "ai:wip"),
    ("READY_LABEL", "ai:ready"),
    ("SIGNOFF_LABEL", "ai:signoff"),
    ("OPERATOR_LABEL", "ai:operator"),
    ("LONG_LABEL", "ai:long"),
    ("LANE_LABEL", ""),
    ("FIRST_LABEL", "ai:first"),
    ("LOW_LABEL", "ai:low"),
];

/// The labels the warning names when no setting overrides them.
pub fn default_labels() -> Vec<String> {
    LABEL_SETTINGS
        .iter()
        .filter(|(_, default)| !default.is_empty())
        .map(|(_, default)| (*default).to_owned())
        .collect()
}

/// One pattern the lint looks for, with the hint printed beside a hit. A prose rule is skipped
/// inside a closed fence, so quoted output may carry its words.
pub struct Rule {
    pub id: String,
    pub pattern: Pattern,
    pub hint: String,
    pub blocking: bool,
    pub prose: bool,
}

impl Rule {
    fn core(
        id: &str,
        pattern: &str,
        hint: &str,
        blocking: bool,
        prose: bool,
    ) -> Result<Self, String> {
        // The strings spliced into a core pattern come from the settings; one can still make the
        // pattern too large to compile.
        let pattern = Pattern::new(pattern).map_err(|error| format!("core rule {id}: {error}"))?;
        Ok(Self {
            id: id.to_owned(),
            pattern,
            hint: hint.to_owned(),
            blocking,
            prose,
        })
    }
}

/// Each string once, escaped, longest first, as alternatives of one group.
fn alternation(items: &[String]) -> String {
    let mut unique: Vec<&str> = Vec::new();
    for item in items {
        if !unique.contains(&item.as_str()) {
            unique.push(item);
        }
    }
    unique.sort_by_key(|item| std::cmp::Reverse(item.chars().count()));
    let escaped: Vec<_> = unique.into_iter().map(fancy_regex::escape).collect();
    format!("(?:{})", escaped.join("|"))
}

/// A bare dotted quad passes right after a version word (`version 1.2.3.4`); with a port it is an
/// address whatever precedes it. A word joined by a hyphen (`v-1.2.3.4`) or with no space
/// (`v1.2.3.4`) already passes: the quad must not follow a word character, a dot or a hyphen.
fn host_address_pattern(version_words: &[String]) -> String {
    let ipv4 = format!(r"{OCTET}(?:\.{OCTET}){{3}}");
    let mut prefix = String::new();
    for word in version_words {
        let mut chars = word.chars();
        let Some(first) = chars.next() else { continue };
        let head = if first.is_alphabetic() {
            let cased: String = first.to_uppercase().chain(first.to_lowercase()).collect();
            format!("[{}]", fancy_regex::escape(&cased))
        } else {
            fancy_regex::escape(&first.to_string()).into_owned()
        };
        // Writing to a String cannot fail.
        let _ = write!(
            prefix,
            r"(?<!\b{head}{} )",
            fancy_regex::escape(chars.as_str())
        );
    }
    format!(r"(?<![\w.]){ipv4}:\d{{1,5}}\b|(?<![\w.-]){prefix}{ipv4}(?![\w-]|\.\d|:\d)")
}

/// The rules every installation gets, blocking ones first. `harness_roots` are the worktree root,
/// the harness checkout and its state: any path under one blocks.
pub fn core_rules(
    version_words: &[String],
    labels: &[String],
    harness_roots: &[String],
) -> Result<Vec<Rule>, String> {
    let mut rules = Vec::new();
    let roots: Vec<String> = harness_roots
        .iter()
        .map(|root| root.trim_end_matches('/').to_owned())
        .filter(|root| !root.is_empty())
        .collect();
    if !roots.is_empty() {
        rules.push(Rule::core(
            "harness-path",
            &format!(r"(?<![\w.-]){}(?![\w.-])", alternation(&roots)),
            "a path in the harness's own trees: name the path inside the repository at a commit",
            true,
            false,
        )?);
    }
    rules.push(Rule::core(
        "host-path",
        r"(?<![\w./-])(?:/home/\w+|~/|\$HOME\b|/mnt/\w+)",
        "a path on this host: name the path inside the repository at a commit, or say the file is not in \
         the repository",
        true,
        false,
    )?);
    rules.push(Rule::core(
        "host-address",
        &format!(
            r"{}|\bssh://|\bDOCKER_HOST\b|\(host \S+\)",
            host_address_pattern(version_words)
        ),
        "a host or an address: say what the host did ('the second test host'), never which",
        true,
        false,
    )?);
    rules.push(Rule::core(
        "process-id",
        r"\bpid \d+\b",
        "a process id belongs in a comment's Harness trace block",
        true,
        false,
    )?);
    rules.push(Rule::core(
        "harness-file",
        r"\b(?:claim|claimable|merge-lock|tick-pid|tick|usage-ok|pr-sweep|worktree-gc|public-record-lint|dashboard)\.(?:sh|py)\b|\blib/instance\.sh\b|\bstate/(?:locks|logs|paused)\b|\bharness (?:lint|claimable)\b",
        "a harness file: name the repository command that does the same",
        true,
        false,
    )?);
    rules.push(Rule::core(
        "tick-aside",
        r"\(mine\)|\bbudget \d+ ?min\b|\bharness tick\b|\bresume recipe\b|\b(?:next|this|my|a later) tick\b|\bmerge (?:slot|lock)\b|\d{2}:\d{2}Z slot\b",
        "a tick-to-tick aside: the reader is not the next tick",
        true,
        true,
    )?);
    rules.push(Rule::core(
        "container-tmp",
        r"(?<![\w./-])/tmp/",
        "a path in a container or on this host",
        true,
        false,
    )?);
    rules.push(Rule::core(
        "operator-stamp",
        r"\b[Oo]perator(?:'s)?,? ?\(?\d{2}:\d{2}Z|\b\d{2}:\d{2}Z (?:ruling|sign-off|post|comment|answer)\b|\(\d{2}:\d{2}Z\)",
        "a timestamp in place of a decision: write what was decided and link the comment",
        true,
        false,
    )?);
    if !labels.is_empty() {
        rules.push(Rule::core(
            "label-mention",
            &format!(r"(?<![\w:-]){}(?![\w:-])", alternation(labels)),
            "a harness label in prose: the labels show on the issue; say the state in words",
            false,
            false,
        )?);
    }
    Ok(rules)
}

/// The core rules with an installation's added; one with a core rule's id replaces that rule.
pub fn compose_rules(
    version_words: &[String],
    labels: &[String],
    installation: Vec<Rule>,
    harness_roots: &[String],
) -> Result<Vec<Rule>, String> {
    let words: Vec<String> = DEFAULT_VERSION_WORDS
        .iter()
        .map(|word| (*word).to_owned())
        .chain(version_words.iter().cloned())
        .collect();
    let mut installation: Vec<Option<Rule>> = installation.into_iter().map(Some).collect();
    let mut take = |id: &str| -> Option<Rule> {
        // The last rule of an id wins, as a later key does in a mapping.
        let index = installation
            .iter()
            .rposition(|rule| rule.as_ref().is_some_and(|rule| rule.id == id))?;
        let rule = installation[index].take();
        for other in &mut installation {
            if other.as_ref().is_some_and(|rule| rule.id == id) {
                *other = None;
            }
        }
        rule
    };
    let mut rules: Vec<Rule> = core_rules(&words, labels, harness_roots)?
        .into_iter()
        .map(|core| take(&core.id).unwrap_or(core))
        .collect();
    rules.extend(installation.into_iter().flatten());
    Ok(rules)
}

/// What an installation's rules file adds: version words, labels and rules.
pub struct RulesFile {
    pub version_words: Vec<String>,
    pub labels: Vec<String>,
    pub rules: Vec<Rule>,
}

/// The largest rules file read; a real one is a few kilobytes.
const RULES_FILE_LIMIT: u64 = 1 << 20;

/// Reads an installation's `lint_rules.json`. The error names the file and what is wrong with it.
pub fn load_rules_file(path: &Path) -> Result<RulesFile, String> {
    let shown = path.display();
    let text =
        super::read_bounded(path, RULES_FILE_LIMIT).map_err(|error| format!("{shown}: {error}"))?;
    let data: Value = serde_json::from_str(&text).map_err(|error| format!("{shown}: {error}"))?;
    let Some(object) = data.as_object().filter(|object| {
        object
            .keys()
            .all(|key| ["version_words", "labels", "rules"].contains(&key.as_str()))
    }) else {
        return Err(format!(
            "{shown}: expected an object with only version_words, labels and rules"
        ));
    };
    let strings = |key: &str| -> Result<Vec<String>, String> {
        let error = || format!("{shown}: {key} must be a list of non-empty strings");
        match object.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Value::String(text) if !text.is_empty() => Ok(text.clone()),
                    _ => Err(error()),
                })
                .collect(),
            Some(_) => Err(error()),
        }
    };
    let raw_rules: &[Value] = match object.get("rules") {
        None => &[],
        Some(Value::Array(items)) => items,
        Some(_) => return Err(format!("{shown}: rules must be a list")),
    };
    let mut rules = Vec::new();
    for (index, raw) in raw_rules.iter().enumerate() {
        let place = format!("{shown}: rules[{index}]");
        let Some(raw) = raw.as_object().filter(|raw| {
            raw.keys()
                .all(|key| ["id", "pattern", "hint", "blocking", "prose"].contains(&key.as_str()))
        }) else {
            return Err(format!(
                "{place}: expected an object with id, pattern, hint, and optional blocking, prose"
            ));
        };
        let text = |key: &str| {
            raw.get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        };
        let (Some(id), Some(pattern), Some(hint)) = (text("id"), text("pattern"), text("hint"))
        else {
            return Err(format!(
                "{place}: id, pattern and hint must be non-empty strings"
            ));
        };
        let flag = |key: &str, default: bool| match raw.get(key) {
            None => Some(default),
            Some(value) => value.as_bool(),
        };
        let (Some(blocking), Some(prose)) = (flag("blocking", true), flag("prose", false)) else {
            return Err(format!("{place}: blocking and prose must be true or false"));
        };
        let pattern = Pattern::new(pattern)
            .map_err(|error| format!("{place} ({id}): bad pattern: {error}"))?;
        rules.push(Rule {
            id: id.to_owned(),
            pattern,
            hint: hint.to_owned(),
            blocking,
            prose,
        });
    }
    Ok(RulesFile {
        version_words: strings("version_words")?,
        labels: strings("labels")?,
        rules,
    })
}
