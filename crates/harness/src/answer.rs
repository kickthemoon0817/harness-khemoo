//! `harness supervisor answer`: what the supervisor asked for, done by the harness.
//!
//! The supervisor runs with no tools: it can only answer, ending its output with one JSON document,
//! `{"directive": <schema v1> | null, "github": [<action>, ...]}`. This reads the last such document
//! of its output, applies the directive through `harness directive`'s own validation, and performs
//! each GitHub action itself, with `gh` as an argv: a comment, a label change or a parked question,
//! only on an open issue of the queue, only with the labels the rails name, every body masked,
//! linted and marked as the supervisor's. Every action and every refusal is a line in the directive
//! log.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use regex::Regex;

use serde::Deserialize;
use serde_json::Value;

use crate::dashboard::files;
use crate::dashboard::redact::redact;
use crate::dashboard::text::invisible;
use crate::directive::{self, Context};
use crate::supervisor::{Paths, now_s};
use crate::usage::Lookup;

/// What is read of the end of the supervisor's output.
const OUTPUT_TAIL: u64 = 64 * 1024;
/// How many lines from the end are tried as the start of the document.
const START_LINES: usize = 400;
pub const MAX_ACTIONS: usize = 5;
const BODY_CHARS: usize = 1500;
const QUESTION_CHARS: usize = 600;
const COMMENT_HEAD: &str =
    "**From the harness's supervisor** (an automated note, not an operator's ruling):";
const PARK_HEAD: &str =
    "**From the harness's supervisor** (an automated question for the operator):";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub directive: Option<Value>,
    #[serde(default)]
    pub github: Vec<Value>,
}

/// One GitHub action, as the supervisor names it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Comment {
        issue: u64,
        body: String,
    },
    Label {
        issue: u64,
        #[serde(default)]
        add: Vec<String>,
        #[serde(default)]
        remove: Vec<String>,
    },
    Park {
        issue: u64,
        question: String,
    },
}

impl Action {
    pub fn issue(&self) -> u64 {
        match self {
            Self::Comment { issue, .. } | Self::Label { issue, .. } | Self::Park { issue, .. } => {
                *issue
            }
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Comment { .. } => "comment",
            Self::Label { .. } => "label",
            Self::Park { .. } => "park",
        }
    }
}

/// The last JSON object of `text` that reads as an answer: a line that starts with `{` begins it,
/// tried from the end, at most [`START_LINES`] of them.
pub fn last_answer(text: &str) -> Option<Result<Answer, String>> {
    let mut starts: Vec<usize> = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with('{') {
            starts.push(offset + (line.len() - line.trim_start().len()));
        }
        offset += line.len();
    }
    starts.iter().rev().take(START_LINES).find_map(|start| {
        let value = serde_json::Deserializer::from_str(&text[*start..])
            .into_iter::<Value>()
            .next()?
            .ok()?;
        let object = value.as_object()?;
        if !(object.contains_key("directive") || object.contains_key("github")) {
            return None;
        }
        Some(serde_json::from_value(value).map_err(|error| error.to_string()))
    })
}

/// What GitHub's Markdown would make of `text` that reaches past the issue: a link, an autolink or
/// an image, which send readers or their browsers elsewhere, or HTML, which hides what it says. The
/// body is parsed as GitHub parses Markdown, extensions and all, so a link defined by reference at
/// any depth (in a block quote, in a list item, its URL escaped) is found where it is used.
fn markup(text: &str) -> Option<&'static str> {
    use pulldown_cmark::{Event, Options, Parser, Tag};
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM;
    Parser::new_ext(text, options).find_map(|event| match event {
        Event::Start(Tag::Link { .. }) => Some("a link"),
        Event::Start(Tag::Image { .. }) => Some("an image"),
        Event::Start(Tag::HtmlBlock) | Event::Html(_) | Event::InlineHtml(_) => Some("HTML"),
        _ => None,
    })
}

/// What a body the supervisor posts must not hold besides [`markup`], each looked for as plain text
/// anywhere in the body, whatever the parser makes of it and whatever surrounds it: GitHub finds a
/// mention, a bare link or a cross-reference inside emphasis, a table cell or a task item, where a
/// word boundary would not. Any `@` (a mention, an address, a ref at a commit), `www.` and `://` (a
/// bare link), a cross-reference to another repository or a commit (`owner/repo#1`,
/// `owner/repo/issues/1`, `/` and a SHA), a link reference's definition (`]:`), a character
/// reference (`&#64;`, `&commat;`, the character GitHub renders), an escaped `/` or `:`, which hides
/// a URL from these checks, and a closing keyword, which closes whatever it names once its PR
/// merges. A plain `#N` holds none of them.
static UNSAFE: LazyLock<[(Regex, &str); 12]> = LazyLock::new(|| {
    let pattern = |text: &str| Regex::new(text).unwrap_or_else(|error| panic!("{text}: {error}"));
    [
        (pattern(r"@"), "an @"),
        (pattern(r"(?i)www\."), "a URL"),
        (pattern(r"://"), "a URL"),
        (pattern(r"(?m)(^|[\s(<:=])//"), "a URL"),
        (
            pattern(
                r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(?:[#@]|/(?i:issues|pull|discussions|commit)/)|(?i)/[0-9a-f]{7,40}",
            ),
            "a cross-reference",
        ),
        (
            pattern(r"&#|&[A-Za-z][A-Za-z0-9]*;"),
            "a character reference",
        ),
        (pattern(r"\]:"), "a link reference"),
        (pattern(r"\\[/:]"), "an escaped URL"),
        (pattern(r"!\["), "an image"),
        (pattern(r"\]\("), "a link"),
        (pattern(r"<[A-Za-z/!?]"), "HTML or an autolink"),
        (
            pattern(
                r"(?i)(^|[^a-z0-9])(close[sd]?|fix(e[sd])?|resolve[sd]?)[\s:]*([a-z0-9_.-]+/[a-z0-9_.-]+)?#[0-9]",
            ),
            "a closing keyword",
        ),
    ]
});

/// The labels a `label` action may add or remove: the configured take-first and take-last labels,
/// which set the order work is taken in. The operator's label goes on only through `park`, with
/// a question, and never comes off by the supervisor's hand: it is the human's hold. The lane and
/// long-budget labels are the operator's, and the sign-off, ready, claim and queue labels the
/// harness's own, so none of them is the supervisor's, even when a priority label is set to one.
pub fn allowed_labels(env: Lookup) -> BTreeSet<String> {
    let label = |key: &str, default: &str| env(key).unwrap_or_else(|| default.to_owned());
    let protected = [
        label("OPERATOR_LABEL", "ai:operator"),
        label("LANE_LABEL", ""),
        label("LONG_LABEL", "ai:long"),
        label("SIGNOFF_LABEL", "ai:signoff"),
        label("READY_LABEL", "ai:ready"),
        label("WIP_LABEL", "ai:wip"),
        label("ISSUE_LABEL", "ai"),
    ];
    ["FIRST_LABEL", "LOW_LABEL"]
        .iter()
        .filter_map(|key| env(key))
        .filter(|name| !name.trim().is_empty() && !protected.contains(name))
        .collect()
}

/// A body for the record: at most `limit` characters, lines of plain text, nothing the secret
/// masks would cover.
fn body(field: &str, text: &str, limit: usize) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(format!("{field}: empty"));
    }
    if text.chars().count() > limit {
        return Err(format!("{field}: more than {limit} characters"));
    }
    if text
        .chars()
        .any(|c| (c.is_control() && c != '\n') || invisible(c))
    {
        return Err(format!("{field}: holds a control or invisible character"));
    }
    if redact(text) != text {
        return Err(format!("{field}: looks like it holds a secret"));
    }
    if let Some(what) = markup(text) {
        return Err(format!("{field}: holds {what}"));
    }
    if let Some((_, what)) = UNSAFE.iter().find(|(pattern, _)| pattern.is_match(text)) {
        return Err(format!("{field}: holds {what}"));
    }
    Ok(text.to_owned())
}

/// What one action does on GitHub, checked against the rails before anything is asked of GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub issue: u64,
    pub comment: Option<String>,
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

pub fn plan(
    action: &Action,
    allowed: &BTreeSet<String>,
    operator_label: &str,
) -> Result<Plan, String> {
    let issue = action.issue();
    if issue == 0 {
        return Err("0 is not an issue number".to_owned());
    }
    let label = |list: &[String]| -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for name in list {
            if !allowed.contains(name) {
                return Err(format!("the label {name} is not the supervisor's to set"));
            }
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        Ok(out)
    };
    match action {
        Action::Comment { body: text, .. } => Ok(Plan {
            issue,
            comment: Some(format!(
                "{COMMENT_HEAD}\n\n{}",
                body("body", text, BODY_CHARS)?
            )),
            add: Vec::new(),
            remove: Vec::new(),
        }),
        Action::Label { add, remove, .. } => {
            let (add, remove) = (label(add)?, label(remove)?);
            if add.is_empty() && remove.is_empty() {
                return Err("no label to add or remove".to_owned());
            }
            if let Some(both) = add.iter().find(|name| remove.contains(name)) {
                return Err(format!("{both} is both added and removed"));
            }
            Ok(Plan {
                issue,
                comment: None,
                add,
                remove,
            })
        }
        Action::Park { question, .. } => Ok(Plan {
            issue,
            comment: Some(format!(
                "{PARK_HEAD}\n\n{}",
                body("question", question, QUESTION_CHARS)?
            )),
            add: vec![operator_label.to_owned()],
            remove: Vec::new(),
        }),
    }
}

/// A label as one segment of a REST path.
pub fn path_segment(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// `gh api` for one repository, `[HOST/]OWNER/NAME`.
struct Repo {
    host: Option<String>,
    path: String,
}

impl Repo {
    fn parse(text: &str) -> Option<Self> {
        match text.split('/').collect::<Vec<_>>()[..] {
            [host, owner, name] => Some(Self {
                host: Some(host.to_owned()),
                path: format!("{owner}/{name}"),
            }),
            [owner, name] if !owner.is_empty() && !name.is_empty() => Some(Self {
                host: None,
                path: format!("{owner}/{name}"),
            }),
            _ => None,
        }
    }

    fn api(&self, extra: &[&str], endpoint: &str) -> Result<Vec<u8>, String> {
        let mut args = vec!["api"];
        if let Some(host) = &self.host {
            args.extend(["--hostname", host]);
        }
        args.extend(extra);
        let endpoint = format!("repos/{}/{endpoint}", self.path);
        args.push(&endpoint);
        let output = crate::gh::run(&args, crate::gh::Stderr::Capture)
            .map_err(|error| format!("gh: {error}"))?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(format!(
                "gh exit {}: {}",
                output.status,
                output.stderr.lines().last().unwrap_or_default()
            ))
        }
    }
}

/// Whether issue `issue` is open, an issue and not a PR, and carries the queue's label.
fn in_queue(repo: &Repo, issue: u64, queue_label: &str) -> Result<(), String> {
    let doc: Value = serde_json::from_slice(&repo.api(&[], &format!("issues/{issue}"))?)
        .map_err(|_| "GitHub's answer does not read".to_owned())?;
    if doc.get("pull_request").is_some() {
        return Err("it is a pull request".to_owned());
    }
    if doc["state"] != "open" {
        return Err("it is not open".to_owned());
    }
    let labelled = doc["labels"]
        .as_array()
        .is_some_and(|labels| labels.iter().any(|label| label["name"] == queue_label));
    if labelled {
        Ok(())
    } else {
        Err(format!("it does not carry {queue_label}"))
    }
}

/// Runs this binary's own `lint --kind comment` over `text`: the same rules a tick's post meets.
fn lint(paths: &Paths, text: &str) -> Result<(), String> {
    let file = paths.file("comment.md");
    files::write_replacing(&file, text).map_err(|error| format!("{}: {error}", file.display()))?;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let status = Command::new(exe)
        .args(["lint", "--kind", "comment"])
        .arg(&file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| error.to_string())?;
    let _ = std::fs::remove_file(&file);
    if status.success() {
        Ok(())
    } else {
        Err("the lint blocks it".to_owned())
    }
}

/// Why an action failed, and whether its comment had already been posted.
struct Failure {
    posted: bool,
    why: String,
}

/// Performs `plan`, its comment first: a park's label goes on only once its question is posted,
/// so no issue waits on the operator for a question nobody can read.
fn perform(paths: &Paths, repo: &Repo, plan: &Plan, queue_label: &str) -> Result<(), Failure> {
    let before = |why: String| Failure { posted: false, why };
    in_queue(repo, plan.issue, queue_label).map_err(before)?;
    let mut posted = false;
    if let Some(text) = &plan.comment {
        lint(paths, text).map_err(before)?;
        repo.api(
            &["-X", "POST", "-f", &format!("body={text}")],
            &format!("issues/{}/comments", plan.issue),
        )
        .map_err(before)?;
        posted = true;
    }
    let after = |why: String| Failure { posted, why };
    for name in &plan.add {
        repo.api(
            &["-X", "POST", "-f", &format!("labels[]={name}")],
            &format!("issues/{}/labels", plan.issue),
        )
        .map_err(after)?;
    }
    for name in &plan.remove {
        repo.api(
            &["-X", "DELETE"],
            &format!("issues/{}/labels/{}", plan.issue, path_segment(name)),
        )
        .map_err(after)?;
    }
    Ok(())
}

/// What is read of the end of the directive log to count the day's parks: a day of runs, every
/// one of them parking, logs far less.
const PARK_LOG_TAIL: u64 = 64 * 1024;

/// The issues parked in the 24 hours before `now`, counted from the end of the directive log at
/// `path`. No log is no park; a log that cannot be read is an error, so the cap fails closed.
pub fn parks_today(path: &Path, now: u64) -> Result<usize, String> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.to_string()),
    };
    let size = file
        .seek(SeekFrom::End(0))
        .map_err(|error| error.to_string())?;
    let from = size.saturating_sub(PARK_LOG_TAIL);
    file.seek(SeekFrom::Start(from))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(PARK_LOG_TAIL)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    // A tail cut mid-line starts at the next whole one.
    let whole = if from > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    Ok(recent_parks(whole, now))
}

/// The issues parked in the 24 hours before `now`, as the directive log `log` records them.
pub fn recent_parks(log: &str, now: u64) -> usize {
    log.lines()
        .filter(|line| line.contains(" github: parked #"))
        .filter_map(|line| crate::supervisor::epoch_of(line.split_whitespace().next()?))
        .filter(|at| at.saturating_add(86_400) > now)
        .count()
}

fn read_output(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let size = file
        .seek(SeekFrom::End(0))
        .map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(size.saturating_sub(OUTPUT_TAIL)))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(OUTPUT_TAIL)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Applies the answer in `output`; exits 1 when there is none or its directive is refused, so the
/// run counts as failed. A refused GitHub action is logged and the rest go on.
pub fn run(env: Lookup, paths: &Paths, output: &Path) -> Result<u8, String> {
    paths.prepare()?;
    let now = now_s();
    let text = read_output(output)?;
    let answer = match last_answer(&text) {
        None => {
            directive::log(
                paths,
                now,
                &["answer: none found in the supervisor's output; nothing applied".to_owned()],
            )?;
            return Ok(1);
        }
        Some(Err(error)) => {
            directive::log(
                paths,
                now,
                &[format!("answer refused: {error}; nothing applied")],
            )?;
            return Ok(1);
        }
        Some(Ok(answer)) => answer,
    };
    match &answer.directive {
        None => directive::log(
            paths,
            now,
            &["answer: no directive; the one in force stays".to_owned()],
        )?,
        Some(value) => {
            let context = Context::from_env(env, paths, now)?;
            // One answer is one decision: its actions may rest on its directive, so they go with it.
            if directive::apply(
                paths,
                value.to_string().as_bytes(),
                &context,
                &directive::live_ticks(paths),
            )
            .is_err()
            {
                directive::log(
                    paths,
                    now,
                    &[
                        "answer refused: its directive was refused; no GitHub action taken"
                            .to_owned(),
                    ],
                )?;
                return Ok(1);
            }
        }
    }
    if answer.github.len() > MAX_ACTIONS {
        directive::log(
            paths,
            now,
            &[format!(
                "github: refused all {} actions: more than {MAX_ACTIONS}",
                answer.github.len()
            )],
        )?;
        return Ok(0);
    }
    let max_parks: usize = env("SUPERVISOR_MAX_PARKS_PER_DAY")
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(2);
    let mut parks = parks_today(&paths.directive_log(), now);
    let allowed = allowed_labels(env);
    let operator = env("OPERATOR_LABEL").unwrap_or_else(|| "ai:operator".to_owned());
    let queue_label = env("ISSUE_LABEL").unwrap_or_else(|| "ai".to_owned());
    let repo = env("GH_REPO").as_deref().and_then(Repo::parse);
    for value in &answer.github {
        let line = match serde_json::from_value::<Action>(value.clone()) {
            Err(error) => format!("github: refused an action: {error}"),
            Ok(Action::Park { issue, .. }) if parks.as_ref().is_err() => format!(
                "github: refused park on #{issue}: the day's parks cannot be counted ({})",
                parks.as_ref().err().map_or("", String::as_str)
            ),
            Ok(Action::Park { issue, .. })
                if parks.as_ref().is_ok_and(|count| *count >= max_parks) =>
            {
                format!(
                    "github: refused park on #{issue}: {} issues parked in 24 h already (SUPERVISOR_MAX_PARKS_PER_DAY)",
                    parks.as_ref().map_or(0, |count| *count)
                )
            }
            Ok(action) => match plan(&action, &allowed, &operator) {
                Err(why) => format!(
                    "github: refused {} on #{}: {why}",
                    action.name(),
                    action.issue()
                ),
                Ok(plan) => match &repo {
                    None => format!(
                        "github: refused {} on #{}: no GH_REPO",
                        action.name(),
                        plan.issue
                    ),
                    Some(repo) => {
                        let done = perform(paths, repo, &plan, &queue_label);
                        // A posted question counts toward the cap whatever became of its label,
                        // so the same question is not asked again.
                        let posted = done
                            .as_ref()
                            .map_or_else(|failure| failure.posted, |()| true);
                        if posted
                            && matches!(action, Action::Park { .. })
                            && let Ok(count) = parks.as_mut()
                        {
                            *count += 1;
                        }
                        match done {
                            Ok(()) => describe(&action, &plan),
                            Err(Failure { posted: true, why })
                                if matches!(action, Action::Park { .. }) =>
                            {
                                format!(
                                    "github: parked #{} for the operator, but its label failed: {why}",
                                    plan.issue
                                )
                            }
                            Err(Failure { posted: true, why }) => format!(
                                "github: commented on #{}, but its labels failed: {why}",
                                plan.issue
                            ),
                            Err(Failure { why, .. }) => format!(
                                "github: refused {} on #{}: {why}",
                                action.name(),
                                plan.issue
                            ),
                        }
                    }
                },
            },
        };
        directive::log(paths, now, &[line])?;
    }
    Ok(0)
}

fn describe(action: &Action, plan: &Plan) -> String {
    match action {
        Action::Comment { .. } => format!("github: commented on #{}", plan.issue),
        Action::Park { .. } => format!("github: parked #{} for the operator", plan.issue),
        Action::Label { .. } => {
            let signed: Vec<String> = plan
                .add
                .iter()
                .map(|name| format!("+{name}"))
                .chain(plan.remove.iter().map(|name| format!("-{name}")))
                .collect();
            format!("github: labelled #{} {}", plan.issue, signed.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn allowed() -> BTreeSet<String> {
        ["ai:first", "ai:low"]
            .iter()
            .map(|label| (*label).to_owned())
            .collect()
    }

    fn action(value: Value) -> Action {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn the_last_answer_of_the_output_is_read() {
        let text = "I looked.\n{\"directive\": null, \"github\": []}\nThen I changed my mind:\n```json\n{\n  \"directive\": {\"version\": 1},\n  \"github\": [{\"comment\": {\"issue\": 7, \"body\": \"x\"}}]\n}\n```\n";
        let answer = last_answer(text).unwrap().unwrap();
        assert_eq!(answer.directive, Some(json!({"version": 1})));
        assert_eq!(answer.github.len(), 1);
        assert!(last_answer("no json here\n{not json\n").is_none());
        assert!(last_answer("{\"other\": 1}\n").is_none());
        // A key the schema does not have refuses the answer, and says which.
        let refused = last_answer("{\"directive\": null, \"shell\": \"rm -rf /\"}\n").unwrap();
        assert!(refused.is_err_and(|why| why.contains("unknown field `shell`")));
        assert!(
            last_answer("{\"directive\": null, \"github\": {\"comment\": 1}}\n")
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn a_body_reaches_no_one_past_its_issue() {
        for (body, what) in [
            ("ping @owner-a about it", "an @"),
            ("@example-org/team please look", "an @"),
            // Emphasis, a table cell or a task item hides no mention, bare link or cross-reference.
            ("_@octocat_", "an @"),
            ("__@octocat__", "an @"),
            ("_@github/docs_", "an @"),
            ("_www.example.org/login_", "a URL"),
            ("| a |\n|---|\n| _www.example.org/login_ |", "a URL"),
            ("- [ ] _www.example.org/login_", "a URL"),
            ("_WWW.example.org_", "a URL"),
            ("a scheme_://x", "a URL"),
            ("see octocat/Hello-World#1", "a cross-reference"),
            ("_see octocat/Hello-World#1_", "a cross-reference"),
            ("see octocat/Spoon-Knife/issues/1", "a cross-reference"),
            ("see octocat/Spoon-Knife/pull/2", "a cross-reference"),
            ("read _o/r/Discussions/3_", "a cross-reference"),
            ("at commit /0123abc", "a cross-reference"),
            ("the commit _/0123ABCdef_", "a cross-reference"),
            ("_fixes #3_", "a closing keyword"),
            ("see https://example.com/x", "a URL"),
            ("see ftp://192.0.2.1/x", "a URL"),
            ("see www.example.com", "a URL"),
            ("the [docs](relative/path)", "a link"),
            ("![tracker](x)", "an image"),
            ("<img src=x>", "HTML"),
            ("<b>bold</b>", "HTML"),
            ("<!-- hidden -->", "HTML"),
            (
                "write to dev-a.example.com's owner, ops@example.com",
                "an @",
            ),
            ("Fixes #3", "a closing keyword"),
            ("this closes example-org/other#12", "a cross-reference"),
            ("Resolved: #4", "a closing keyword"),
            ("thanks &#64;octocat", "a character reference"),
            ("thanks &commat;octocat", "a character reference"),
            ("mail a.b&#64;example.org", "a character reference"),
            ("read &#x40;team", "a character reference"),
            ("see [here][1]\n\n[1]: //example.org/login", "a link"),
            ("see [docs]\n   [docs]: relative", "a link"),
            ("off to //example.org/login", "a URL"),
            ("link(//example.org)", "a URL"),
            ("go to=//example.org", "a URL"),
            // A reference defined in a block quote, in a list item, nested two deep, its URL escaped:
            // each renders as a link where it is used.
            (
                "click [here][1]\n\n> [1]: https:\\/\\/example.org/login",
                "a link",
            ),
            ("click [here][1]\n\n> [1]: /\\/example.org/login", "a link"),
            ("click [here][1]\n\n- [1]: /\\/example.org/login", "a link"),
            (
                "click [here][1]\n\n> - [1]: /\\/example.org/login",
                "a link",
            ),
            ("click [here]\n\n> [here]: relative/page", "a link"),
            ("an <https://example.org> autolink", "a link"),
            ("a <span>tag</span>", "HTML"),
            ("<div>\nblock\n</div>", "HTML"),
            ("![alt](pic.png)", "an image"),
            // The same, with no use to find: the text checks still refuse the definition.
            ("> - [1]: /\\/example.org/login", "a link reference"),
            ("a path a\\/b", "an escaped URL"),
            ("time 10\\:30", "an escaped URL"),
        ] {
            let refused = plan_of(json!({"comment": {"issue": 7, "body": body}}));
            assert!(
                refused.as_ref().is_err_and(|why| why.contains(what)),
                "{body:?}: {refused:?}"
            );
            assert!(
                plan_of(json!({"park": {"issue": 7, "question": body}})).is_err(),
                "{body:?}"
            );
        }
        // Plain references and words stay.
        for body in [
            "Waits on #3; a prefix #4 and a fixed point stay.",
            "a < b and 2 > 1",
            "email the team",
            "A & B; a path a/b//c and [a] list",
            "Waits on #3: run `cargo test --workspace` and `tests/run.sh`, then see #12.",
            "The _emphasis_ stays: read src/main.rs and `src/lib.rs`, then #7 and **#8**.",
            "- a list item\n- another, waiting on #5\n\n> a quote\n\n    an indented code block",
            "rate 3/4 and 1//2",
        ] {
            assert!(
                plan_of(json!({"comment": {"issue": 7, "body": body}})).is_ok(),
                "{body:?}"
            );
        }
    }

    #[test]
    fn a_long_log_still_counts_its_last_parks_and_an_unreadable_one_counts_none() {
        let dir = crate::dashboard::testkit::TempDir::new();
        let now = crate::supervisor::epoch_of("2026-11-28T12:00:00Z").unwrap();
        let log = dir.path().join("directive.log");
        let old =
            "2026-11-20T00:00:00Z apply: an old directive with a long reason line\n".repeat(30_000);
        let recent = "2026-11-28T10:00:00Z github: parked #4 for the operator\n2026-11-28T11:00:00Z github: parked #5 for the operator\n";
        std::fs::write(&log, format!("{old}{recent}")).unwrap();
        assert!(std::fs::metadata(&log).unwrap().len() > 1 << 20);
        assert_eq!(parks_today(&log, now), Ok(2));
        assert_eq!(parks_today(&dir.path().join("none.log"), now), Ok(0));
        // A log that cannot be read fails closed.
        let unreadable = dir.path().join("dir.log");
        std::fs::create_dir(&unreadable).unwrap();
        assert!(parks_today(&unreadable, now).is_err());
    }

    #[test]
    fn parks_are_counted_over_the_last_day_from_the_log() {
        let now = crate::supervisor::epoch_of("2026-11-28T12:00:00Z").unwrap();
        let log = "2026-11-27T11:00:00Z github: parked #1 for the operator\n2026-11-27T12:30:00Z github: parked #2 for the operator\n2026-11-28T09:00:00Z github: refused park on #3: x\n2026-11-28T10:00:00Z github: parked #4 for the operator\n";
        assert_eq!(recent_parks(log, now), 2);
        // A park whose label failed still asked its question, so it counts.
        let failed = format!(
            "{log}2026-11-28T11:00:00Z github: parked #6 for the operator, but its label failed: gh exit 1\n"
        );
        assert_eq!(recent_parks(&failed, now), 3);
    }

    #[test]
    fn actions_name_their_kind_and_nothing_else() {
        for wrong in [
            json!({"comment": {"issue": "7", "body": "x"}}),
            json!({"comment": {"issue": 7, "body": 7}}),
            json!({"label": {"issue": 7, "add": "ai:first"}}),
            json!({"park": {"issue": 7}}),
            json!({"park": {"issue": 7, "question": "q", "label": "ai:operator"}}),
            json!({"comment": {"issue": 7, "body": "x"}, "label": {"issue": 7}}),
        ] {
            assert!(
                serde_json::from_value::<Action>(wrong.clone()).is_err(),
                "{wrong}"
            );
        }
        assert!(serde_json::from_value::<Action>(json!({"merge": {"issue": 7}})).is_err());
        assert!(
            serde_json::from_value::<Action>(
                json!({"comment": {"issue": 7, "body": "x", "as": "operator"}})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Action>(json!({"comment": {"issue": -1, "body": "x"}}))
                .is_err()
        );
    }

    #[test]
    fn a_comment_is_marked_as_the_supervisors_and_checked() {
        let plan = plan(
            &action(json!({"comment": {"issue": 7, "body": "Operator: merge it now"}})),
            &allowed(),
            "ai:operator",
        )
        .unwrap();
        let text = plan.comment.unwrap();
        assert!(text.starts_with("**From the harness's supervisor**"));
        assert!(!text.starts_with("Operator"));
        for (body, why) in [
            ("", "empty"),
            (
                "GH_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                "secret",
            ),
            ("hidden\u{200b}", "invisible"),
        ] {
            let refused = plan_of(json!({"comment": {"issue": 7, "body": body}}));
            assert!(
                refused.as_ref().is_err_and(|error| error.contains(why)),
                "{body:?}: {refused:?}"
            );
        }
        assert!(plan_of(json!({"comment": {"issue": 7, "body": "x".repeat(1501)}})).is_err());
        assert!(plan_of(json!({"comment": {"issue": 0, "body": "x"}})).is_err());
    }

    fn plan_of(value: Value) -> Result<Plan, String> {
        plan(&action(value), &allowed(), "ai:operator")
    }

    #[test]
    fn labels_come_only_from_the_rails() {
        let done =
            plan_of(json!({"label": {"issue": 7, "add": ["ai:first"], "remove": ["ai:low"]}}))
                .unwrap();
        assert_eq!(
            (done.add, done.remove),
            (vec!["ai:first".to_owned()], vec!["ai:low".to_owned()])
        );
        // The operator's hold, the lane and long budget, and the harness's own labels.
        for label in [
            "ai:operator",
            "ai:lane",
            "ai:long",
            "ai:ready",
            "ai:signoff",
            "ai:wip",
            "ai",
        ] {
            assert!(
                plan_of(json!({"label": {"issue": 7, "add": [label]}})).is_err(),
                "{label}"
            );
            assert!(
                plan_of(json!({"label": {"issue": 7, "remove": [label]}})).is_err(),
                "{label}"
            );
        }
        assert!(plan_of(json!({"label": {"issue": 7}})).is_err());
        assert!(
            plan_of(json!({"label": {"issue": 7, "add": ["ai:first"], "remove": ["ai:first"]}}))
                .is_err()
        );
    }

    #[test]
    fn a_park_adds_the_operator_label_and_asks() {
        let done =
            plan_of(json!({"park": {"issue": 9, "question": "Should #9 wait for the new API?"}}))
                .unwrap();
        assert_eq!(done.add, ["ai:operator"]);
        assert!(
            done.comment
                .unwrap()
                .ends_with("Should #9 wait for the new API?")
        );
    }

    #[test]
    fn only_the_priority_labels_are_the_supervisors() {
        let env = |key: &str| match key {
            "FIRST_LABEL" => Some("ai:first".to_owned()),
            "LOW_LABEL" => Some("ai:low".to_owned()),
            "LANE_LABEL" => Some("ai:lane".to_owned()),
            "OPERATOR_LABEL" => Some("ai:op".to_owned()),
            _ => None,
        };
        assert_eq!(
            allowed_labels(&env).into_iter().collect::<Vec<_>>(),
            ["ai:first", "ai:low"]
        );
        // A priority label set to a protected one is not the supervisor's either.
        let clash = |key: &str| match key {
            "FIRST_LABEL" => Some("ai:operator".to_owned()),
            "LOW_LABEL" => Some("ai:long".to_owned()),
            _ => None,
        };
        assert!(allowed_labels(&clash).is_empty());
    }

    #[test]
    fn the_operators_hold_goes_on_only_by_park_and_never_comes_off() {
        assert!(
            plan_of(json!({"label": {"issue": 7, "remove": ["ai:operator"]}}))
                .is_err_and(|why| why.contains("ai:operator"))
        );
        assert!(plan_of(json!({"label": {"issue": 7, "add": ["ai:operator"]}})).is_err());
        assert_eq!(
            plan_of(json!({"park": {"issue": 7, "question": "Is this in scope?"}}))
                .unwrap()
                .add,
            ["ai:operator"]
        );
        assert!(
            plan_of(json!({"park": {"issue": 7, "question": "q"}}))
                .unwrap()
                .remove
                .is_empty()
        );
    }

    #[test]
    fn a_label_is_encoded_for_its_path() {
        assert_eq!(path_segment("ai:first"), "ai%3Afirst");
        assert_eq!(path_segment("a b/c"), "a%20b%2Fc");
    }
}
