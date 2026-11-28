//! `harness lint`: checks what a tick is about to post to GitHub against prompts/public-record.md.
//!
//! The public record (issue bodies, PR bodies, comments) is read by people who have the repository
//! and nothing else. The lint blocks what such a reader cannot use: paths on this host, host names
//! and addresses, process ids, harness file names, tick-to-tick asides. It warns on harness labels
//! in prose. A `<details><summary>Harness trace</summary>` block in a comment is exempt: that is
//! where the harness's own bookkeeping goes, the claim protocol line (`harness tick … pid N
//! claiming`) included, because the launcher finds that line anywhere in a comment. Inside a closed
//! fenced code block only the prose rules (tick asides, an installation's rules marked prose, the
//! warnings) are skipped, so quoted output does not block on harness vocabulary; every rule that
//! identifies the machine still applies there.

mod check;
mod pattern;
mod repr;
mod rules;

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Deserialize;

use crate::gh;
use crate::home::{env_nonempty, harness_home};
use check::{AuditRow, Kind, Lint};
use rules::{LABEL_SETTINGS, compose_rules, core_rules, default_labels, load_rules_file};

const AFTER_HELP: &str = "\
Examples:
  harness lint --kind pr BODY.md          lint a file; exit 1 on a blocking hit
  harness lint --kind comment -           lint a comment from stdin
  harness lint --pr 2138                  lint a PR body fetched with gh
  harness lint --issue 2137               lint an issue body fetched with gh
  harness lint --audit prs --last 20      one line per body over the last 20 PRs
  harness lint --self-test                the built-in samples, core rules only

GH_REPO (or --repo) names the repository for --pr, --issue and --audit. PUBLIC_RECORD_PR_SECTIONS and
PUBLIC_RECORD_ISSUE_SECTIONS (comma-separated heading names) override the sections a body must carry.
The harness labels the warning names come from WIP_LABEL, READY_LABEL, SIGNOFF_LABEL, OPERATOR_LABEL,
LONG_LABEL, LANE_LABEL, FIRST_LABEL and LOW_LABEL. Any path under WORKTREE_ROOT, HARNESS_HOME or
HARNESS_STATE, each when set, blocks.

The installation's rules load from PUBLIC_RECORD_RULES, else from $HARNESS_INSTANCE_DIR/lint_rules.json
(HARNESS_INSTANCE_DIR defaults to the checkout's instance/); an instance dir without that file is named
on stderr on every run. The file is a JSON object:

    {\"version_words\": [\"SDK\"],            words a dotted quad may follow as a version, not an address
     \"labels\": [\"ai:plan\"],               more harness labels to warn on
     \"rules\": [{\"id\": \"...\", \"pattern\": \"...\", \"hint\": \"...\",
                \"blocking\": true,         false makes it a warning
                \"prose\": false}]}         true skips it inside a closed fence

A pattern is a regex in Python's re syntax, \\w and \\b meaning what they mean there. A bracket class may
not hold --, && or ~~ unescaped. A rule whose id is a core rule's replaces that rule.
Exit 0 clean (warnings allowed), 1 blocking findings, 2 usage, rules-file or fetch error.";

#[derive(clap::Args)]
#[command(after_help = AFTER_HELP)]
pub struct Args {
    /// A file holding the body, or - for stdin.
    body: Option<PathBuf>,
    #[arg(long, value_enum)]
    kind: Option<Kind>,
    /// Lint this PR's body, fetched with gh.
    #[arg(long)]
    pr: Option<u64>,
    /// Lint this issue's body, fetched with gh.
    #[arg(long)]
    issue: Option<u64>,
    /// One line per body over the last N.
    #[arg(long, value_enum)]
    audit: Option<Audit>,
    /// How many bodies --audit reads.
    #[arg(long, default_value_t = 20)]
    last: u32,
    /// --audit prs: only PRs into this branch.
    #[arg(long)]
    base: Option<String>,
    /// --audit issues: only issues with this label.
    #[arg(long)]
    label: Option<String>,
    #[arg(long, help = "OWNER/REPO (default: $GH_REPO)")]
    repo: Option<String>,
    /// Run the built-in samples.
    #[arg(long)]
    self_test: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Audit {
    Prs,
    Issues,
}

/// The largest body read: GitHub caps a body at 65536 characters.
const BODY_LIMIT: u64 = 1 << 20;

pub fn run(args: &Args) -> ExitCode {
    match lint_main(args) {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("harness lint: {message}");
            ExitCode::from(2)
        }
    }
}

fn env_string(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
}

fn sections(name: &str, default: &[&str]) -> Vec<String> {
    let set: Vec<String> = env_string(name)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    if set.is_empty() {
        default.iter().map(|part| (*part).to_owned()).collect()
    } else {
        set
    }
}

fn lint_main(args: &Args) -> Result<u8, String> {
    let repo = args
        .repo
        .clone()
        .or_else(|| env_string("GH_REPO"))
        .filter(|repo| !repo.is_empty());
    let pr_sections = sections("PUBLIC_RECORD_PR_SECTIONS", &check::DEFAULT_PR_SECTIONS);
    let issue_sections = sections(
        "PUBLIC_RECORD_ISSUE_SECTIONS",
        &check::DEFAULT_ISSUE_SECTIONS,
    );
    if args.self_test {
        let words: Vec<String> = rules::DEFAULT_VERSION_WORDS
            .iter()
            .map(|word| (*word).to_owned())
            .collect();
        let core = core_rules(&words, &default_labels(), &[])?;
        let lint = Lint::new(pr_sections, issue_sections, core);
        return Ok(u8::from(!lint.self_test()));
    }
    let mut labels: Vec<String> = LABEL_SETTINGS
        .iter()
        .map(|(name, default)| env_string(name).unwrap_or_else(|| (*default).to_owned()))
        .filter(|label| !label.is_empty())
        .collect();
    let mut version_words = Vec::new();
    let mut installation = Vec::new();
    let (rules_file, instance) = rules_path();
    // An instance without its rules is likely a missed copy, and the lint then passes what the
    // project's rules would block, so say so on every run.
    if rules_file.is_none() {
        match &instance {
            Some(dir) if dir.is_dir() => eprintln!(
                "harness lint: no lint_rules.json in {}; only the core rules applied",
                dir.display()
            ),
            Some(_) => {}
            None => eprintln!(
                "harness lint: no instance dir found (set HARNESS_INSTANCE_DIR or HARNESS_HOME); only the core rules applied"
            ),
        }
    }
    if let Some(path) = rules_file {
        let file = load_rules_file(&path)?;
        version_words = file.version_words;
        labels.extend(file.labels);
        installation = file.rules;
    }
    let roots: Vec<String> = ["WORKTREE_ROOT", "HARNESS_HOME", "HARNESS_STATE"]
        .iter()
        .map(|name| env_string(name).unwrap_or_default())
        .collect();
    let lint = Lint::new(
        pr_sections,
        issue_sections,
        compose_rules(&version_words, &labels, installation, &roots)?,
    );

    if let Some(audit) = args.audit {
        let rows = audit_rows(&lint, audit, args, repo.as_deref())?;
        println!("{}", check::format_audit(&rows));
        return Ok(0);
    }
    let (kind, body) = if let Some((kind, number)) = args
        .pr
        .map(|number| (Kind::Pr, number))
        .or_else(|| args.issue.map(|number| (Kind::Issue, number)))
    {
        let (title, body) = fetch_body(kind, number, repo.as_deref())?;
        println!("{} #{number}: {title}", kind.name());
        (kind, body)
    } else {
        let (Some(path), Some(kind)) = (&args.body, args.kind) else {
            eprintln!("harness lint: a body file (or -) and --kind are required");
            return Ok(2);
        };
        let text = if path == Path::new("-") {
            read_limited(io::stdin().lock(), BODY_LIMIT)
                .map_err(|error| format!("stdin: {error}"))?
        } else {
            read_bounded(path, BODY_LIMIT)
                .map_err(|error| format!("{}: {error}", path.display()))?
        };
        // A body read as text has its line ends made \n, as a post's are.
        (kind, text.replace("\r\n", "\n").replace('\r', "\n"))
    };
    let findings = lint.lint(&body, kind);
    println!("{}", check::format(&findings, kind));
    Ok(u8::from(findings.iter().any(|finding| finding.blocking)))
}

/// The installation's rules file and the instance dir it was looked for in. The file is
/// `PUBLIC_RECORD_RULES` when set, else the instance's `lint_rules.json` when it exists.
fn rules_path() -> (Option<PathBuf>, Option<PathBuf>) {
    let instance = env_nonempty("HARNESS_INSTANCE_DIR")
        .map(PathBuf::from)
        .or_else(|| harness_home().map(|home| home.join("instance")));
    if let Some(explicit) = env_nonempty("PUBLIC_RECORD_RULES") {
        return (Some(PathBuf::from(explicit)), instance);
    }
    let candidate = instance
        .as_ref()
        .map(|dir| dir.join("lint_rules.json"))
        .filter(|file| file.is_file());
    (candidate, instance)
}

fn read_limited(reader: impl Read, limit: u64) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(format!("larger than {limit} bytes")));
    }
    String::from_utf8(bytes).map_err(|_| io::Error::other("not UTF-8"))
}

/// The file's text, refused past `limit` bytes or when it is not UTF-8.
fn read_bounded(path: &Path, limit: u64) -> io::Result<String> {
    read_limited(File::open(path)?, limit)
}

fn gh_json(args: &[&str]) -> Result<Vec<u8>, String> {
    let output = gh::run(args, gh::Stderr::Capture).map_err(|error| format!("gh: {error}"))?;
    if !output.status.success() {
        return Err(format!("gh failed: {}", output.stderr));
    }
    Ok(output.stdout)
}

fn fetch_body(kind: Kind, number: u64, repo: Option<&str>) -> Result<(String, String), String> {
    #[derive(Deserialize)]
    struct View {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: Option<String>,
    }
    let number = number.to_string();
    let mut args = vec![kind.name(), "view", &number, "--json", "title,body"];
    if let Some(repo) = repo {
        args.extend(["--repo", repo]);
    }
    let view: View =
        serde_json::from_slice(&gh_json(&args)?).map_err(|error| format!("gh output: {error}"))?;
    Ok((
        view.title.unwrap_or_default(),
        view.body.unwrap_or_default(),
    ))
}

fn audit_rows(
    lint: &Lint,
    audit: Audit,
    args: &Args,
    repo: Option<&str>,
) -> Result<Vec<AuditRow>, String> {
    #[derive(Deserialize)]
    struct Item {
        number: u64,
        title: String,
        #[serde(default)]
        body: Option<String>,
    }
    let kind = match audit {
        Audit::Prs => Kind::Pr,
        Audit::Issues => Kind::Issue,
    };
    let last = args.last.to_string();
    let mut gh_args = vec![
        kind.name(),
        "list",
        "--state",
        "all",
        "--limit",
        &last,
        "--json",
        "number,title,body",
    ];
    if let Some(repo) = repo {
        gh_args.extend(["--repo", repo]);
    }
    match (kind, &args.base, &args.label) {
        (Kind::Pr, Some(base), _) if !base.is_empty() => gh_args.extend(["--base", base]),
        (Kind::Issue, _, Some(label)) if !label.is_empty() => gh_args.extend(["--label", label]),
        _ => {}
    }
    let items: Vec<Item> = serde_json::from_slice(&gh_json(&gh_args)?)
        .map_err(|error| format!("gh output: {error}"))?;
    Ok(items
        .into_iter()
        .map(|item| {
            lint.audit_row(
                item.number,
                item.title,
                &item.body.unwrap_or_default(),
                kind,
            )
        })
        .collect())
}
