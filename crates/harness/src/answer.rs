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
pub fn last_answer(text: &str) -> Option<Answer> {
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
        serde_json::from_value(value).ok()
    })
}

/// The labels the supervisor may set or remove: the configured take-first, take-last, lane and
/// operator labels, those set.
pub fn allowed_labels(env: Lookup) -> BTreeSet<String> {
    ["FIRST_LABEL", "LOW_LABEL", "LANE_LABEL"]
        .iter()
        .filter_map(|key| env(key))
        .chain(std::iter::once(
            env("OPERATOR_LABEL").unwrap_or_else(|| "ai:operator".to_owned()),
        ))
        .filter(|label| !label.trim().is_empty())
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

fn perform(paths: &Paths, repo: &Repo, plan: &Plan, queue_label: &str) -> Result<(), String> {
    in_queue(repo, plan.issue, queue_label)?;
    if let Some(text) = &plan.comment {
        lint(paths, text)?;
    }
    for name in &plan.add {
        repo.api(
            &["-X", "POST", "-f", &format!("labels[]={name}")],
            &format!("issues/{}/labels", plan.issue),
        )?;
    }
    for name in &plan.remove {
        repo.api(
            &["-X", "DELETE"],
            &format!("issues/{}/labels/{}", plan.issue, path_segment(name)),
        )?;
    }
    if let Some(text) = &plan.comment {
        repo.api(
            &["-X", "POST", "-f", &format!("body={text}")],
            &format!("issues/{}/comments", plan.issue),
        )?;
    }
    Ok(())
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
    let Some(answer) = last_answer(&text) else {
        directive::log(
            paths,
            now,
            &["answer: none found in the supervisor's output; nothing applied".to_owned()],
        )?;
        return Ok(1);
    };
    let mut failed = false;
    match &answer.directive {
        None => directive::log(
            paths,
            now,
            &["answer: no directive; the one in force stays".to_owned()],
        )?,
        Some(value) => {
            let context = Context::from_env(env, paths, now)?;
            if directive::apply(
                paths,
                value.to_string().as_bytes(),
                &context,
                &directive::live_ticks(paths),
            )
            .is_err()
            {
                failed = true;
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
        return Ok(u8::from(failed));
    }
    let allowed = allowed_labels(env);
    let operator = env("OPERATOR_LABEL").unwrap_or_else(|| "ai:operator".to_owned());
    let queue_label = env("ISSUE_LABEL").unwrap_or_else(|| "ai".to_owned());
    let repo = env("GH_REPO").as_deref().and_then(Repo::parse);
    for value in &answer.github {
        let line = match serde_json::from_value::<Action>(value.clone()) {
            Err(error) => format!("github: refused an action: {error}"),
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
                    Some(repo) => match perform(paths, repo, &plan, &queue_label) {
                        Ok(()) => describe(&action, &plan),
                        Err(why) => format!(
                            "github: refused {} on #{}: {why}",
                            action.name(),
                            plan.issue
                        ),
                    },
                },
            },
        };
        directive::log(paths, now, &[line])?;
    }
    Ok(u8::from(failed))
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
        ["ai:first", "ai:low", "ai:operator"]
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
        let answer = last_answer(text).unwrap();
        assert_eq!(answer.directive, Some(json!({"version": 1})));
        assert_eq!(answer.github.len(), 1);
        assert!(last_answer("no json here\n{not json\n").is_none());
        assert!(last_answer("{\"other\": 1}\n").is_none());
        // A key the schema does not have makes the document no answer.
        assert!(last_answer("{\"directive\": null, \"shell\": \"rm -rf /\"}\n").is_none());
    }

    #[test]
    fn actions_name_their_kind_and_nothing_else() {
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
        for label in ["ai:ready", "ai:signoff", "ai:wip", "ai", "ai:long"] {
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
    fn the_allowed_labels_are_the_configured_ones() {
        let env = |key: &str| match key {
            "FIRST_LABEL" => Some("ai:first".to_owned()),
            "OPERATOR_LABEL" => Some("ai:op".to_owned()),
            _ => None,
        };
        assert_eq!(
            allowed_labels(&env).into_iter().collect::<Vec<_>>(),
            ["ai:first", "ai:op"]
        );
    }

    #[test]
    fn a_label_is_encoded_for_its_path() {
        assert_eq!(path_segment("ai:first"), "ai%3Afirst");
        assert_eq!(path_segment("a b/c"), "a%20b%2Fc");
    }
}
