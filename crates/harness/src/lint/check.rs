//! Checks one body against the public-record contract and words what it finds.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::repr::quoted;
use super::rules::Rule;

pub const DEFAULT_PR_SECTIONS: [&str; 6] = [
    "Summary",
    "Why",
    "What changed",
    "How to verify",
    "Evidence",
    "Scope and limits",
];
pub const DEFAULT_ISSUE_SECTIONS: [&str; 2] = ["Summary", "Ask"];

static TRACE_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<details>\s*<summary>[^<]*harness trace[^<]*</summary>.*?</details>")
        .unwrap_or_else(|error| panic!("TRACE_BLOCK: {error}"))
});
static HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^#{1,3}\s+(.+?)\s*$").unwrap_or_else(|error| panic!("HEADING: {error}"))
});

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Kind {
    Pr,
    Issue,
    Comment,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pr => "pr",
            Self::Issue => "issue",
            Self::Comment => "comment",
        }
    }
}

/// One hit: the rule, where it is (line 0 for a missing section), what matched.
pub struct Finding {
    pub rule_id: String,
    pub line: usize,
    pub matched: String,
    pub hint: String,
    pub blocking: bool,
}

/// One body's totals in an audit table.
pub struct AuditRow {
    number: u64,
    title: String,
    blocking: usize,
    warnings: usize,
    /// The most frequent blocking rules, at most three, with their counts.
    top: Vec<(String, usize)>,
}

pub struct Lint {
    pr_sections: Vec<String>,
    issue_sections: Vec<String>,
    blocking: Vec<Rule>,
    warnings: Vec<Rule>,
}

impl Lint {
    pub fn new(pr_sections: Vec<String>, issue_sections: Vec<String>, rules: Vec<Rule>) -> Self {
        let (blocking, warnings) = rules.into_iter().partition(|rule| rule.blocking);
        Self {
            pr_sections,
            issue_sections,
            blocking,
            warnings,
        }
    }

    /// Every finding in `body`, blocking ones first, each group in line order.
    pub fn lint(&self, body: &str, kind: Kind) -> Vec<Finding> {
        let mut findings = Vec::new();
        let text = if kind == Kind::Comment {
            // The block keeps its lines, so a finding after it keeps its line number.
            TRACE_BLOCK
                .replace_all(body, |caps: &regex::Captures| {
                    "\n".repeat(caps[0].matches('\n').count())
                })
                .into_owned()
        } else {
            if let Some(trace) = TRACE_BLOCK.find(body) {
                findings.push(Finding {
                    rule_id: "trace-in-body".to_owned(),
                    line: 1 + body[..trace.start()].matches('\n').count(),
                    matched: "<details><summary>Harness trace</summary>".to_owned(),
                    hint: "a Harness trace belongs in a comment, never in an issue or PR body"
                        .to_owned(),
                    blocking: true,
                });
            }
            body.to_owned()
        };
        let lines: Vec<&str> = text.split('\n').collect();
        let fenced = fenced_lines(&lines);
        let mut seen_warnings: HashSet<(String, String)> = HashSet::new();
        for (index, line) in lines.iter().enumerate() {
            let in_fence = fenced.contains(&index);
            for rule in &self.blocking {
                if in_fence && rule.prose {
                    continue;
                }
                match rule.pattern.find_iter(line) {
                    Ok(hits) => findings.extend(
                        hits.into_iter()
                            .map(|(start, end)| Finding::hit(rule, index + 1, &line[start..end])),
                    ),
                    Err(error) => findings.push(Finding::unchecked(rule, index + 1, &error)),
                }
            }
            if in_fence {
                continue;
            }
            for rule in &self.warnings {
                let hits = match rule.pattern.find_iter(line) {
                    Ok(hits) => hits,
                    Err(error) => {
                        findings.push(Finding::unchecked(rule, index + 1, &error));
                        continue;
                    }
                };
                for (start, end) in hits {
                    let matched = &line[start..end];
                    if seen_warnings.insert((rule.id.clone(), matched.to_owned())) {
                        findings.push(Finding::hit(rule, index + 1, matched));
                    }
                }
            }
        }
        // Ticks post with the operator's token, so a comment of theirs that opens with `Operator`
        // would be read back to a later tick as the operator's ruling.
        if let Some(index) = lines.iter().position(|line| !line.trim().is_empty())
            && lines[index].trim_start().starts_with("Operator")
        {
            findings.push(Finding {
                rule_id: "operator-voice".to_owned(),
                line: index + 1,
                matched: "Operator".to_owned(),
                hint:
                    "a body that opens with `Operator` reads as the operator's ruling: open with \
                           what happened"
                        .to_owned(),
                blocking: true,
            });
        }
        match kind {
            Kind::Pr => findings.extend(missing_sections(&text, &self.pr_sections)),
            Kind::Issue => findings.extend(missing_sections(&text, &self.issue_sections)),
            Kind::Comment => {}
        }
        findings.sort_by_key(|finding| (!finding.blocking, finding.line));
        findings
    }

    pub fn audit_row(&self, number: u64, title: String, body: &str, kind: Kind) -> AuditRow {
        let findings = self.lint(body, kind);
        let mut counts: Vec<(String, usize)> = Vec::new();
        for finding in findings.iter().filter(|finding| finding.blocking) {
            match counts.iter_mut().find(|(id, _)| *id == finding.rule_id) {
                Some((_, count)) => *count += 1,
                None => counts.push((finding.rule_id.clone(), 1)),
            }
        }
        let blocking = counts.iter().map(|(_, count)| count).sum();
        counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        counts.truncate(3);
        AuditRow {
            number,
            title,
            blocking,
            warnings: findings.iter().filter(|finding| !finding.blocking).count(),
            top: counts,
        }
    }

    /// Runs the built-in samples, printing a PASS or FAIL line each; true when all pass.
    pub fn self_test(&self) -> bool {
        let mut passed = true;
        for (name, ok, detail) in self.self_test_cases() {
            passed &= ok;
            if ok {
                println!("PASS {name}");
            } else {
                println!("FAIL {name} :: {detail}");
            }
        }
        passed
    }

    fn self_test_cases(&self) -> Vec<(String, bool, String)> {
        let mut cases = Vec::new();
        let bad = self.lint(SELF_TEST_BAD_BODY, Kind::Pr);
        let fired: HashSet<&str> = bad
            .iter()
            .filter(|finding| finding.blocking)
            .map(|finding| finding.rule_id.as_str())
            .collect();
        for rule in &self.blocking {
            cases.push((
                format!("{} fires on the bad body", rule.id),
                fired.contains(rule.id.as_str()),
                format(&bad, Kind::Pr),
            ));
        }
        cases.push((
            "a body without the sections is blocked".to_owned(),
            fired.contains("section"),
            format(&bad, Kind::Pr),
        ));
        let good = self.lint(SELF_TEST_GOOD_PR, Kind::Pr);
        cases.push((
            "a PR body written to the contract has no blocking finding".to_owned(),
            !good.iter().any(|finding| finding.blocking),
            format(&good, Kind::Pr),
        ));
        cases.push((
            "the PR body's label in prose is warned, not blocked".to_owned(),
            good.iter()
                .any(|finding| finding.rule_id == "label-mention"),
            format(&good, Kind::Pr),
        ));
        let comment = self.lint(SELF_TEST_GOOD_COMMENT, Kind::Comment);
        cases.push((
            "a comment with its protocol line inside the trace block is clean".to_owned(),
            !comment.iter().any(|finding| finding.blocking),
            format(&comment, Kind::Comment),
        ));
        let old_style = self.lint(SELF_TEST_OLD_STYLE_COMMENT, Kind::Comment);
        let old_fired: HashSet<&str> = old_style
            .iter()
            .filter(|finding| finding.blocking)
            .map(|finding| finding.rule_id.as_str())
            .collect();
        cases.push((
            "the old claim line in the open is blocked for its pid, host and budget".to_owned(),
            ["process-id", "host-address", "tick-aside"]
                .iter()
                .all(|id| old_fired.contains(id)),
            format(&old_style, Kind::Comment),
        ));
        let trace_in_body = self.lint(
            &format!("{SELF_TEST_GOOD_PR}\n<details><summary>Harness trace</summary>x</details>"),
            Kind::Pr,
        );
        cases.push((
            "a trace block in a body is blocked".to_owned(),
            trace_in_body
                .iter()
                .any(|finding| finding.rule_id == "trace-in-body"),
            format(&trace_in_body, Kind::Pr),
        ));
        cases
    }
}

impl Finding {
    fn hit(rule: &Rule, line: usize, matched: &str) -> Self {
        Self {
            rule_id: rule.id.clone(),
            line,
            matched: matched.to_owned(),
            hint: rule.hint.clone(),
            blocking: rule.blocking,
        }
    }

    /// A line the rule could not be checked on blocks, whatever the rule, so nothing is passed
    /// unchecked.
    fn unchecked(rule: &Rule, line: usize, error: &str) -> Self {
        Self {
            rule_id: rule.id.clone(),
            line,
            matched: String::new(),
            hint: format!(
                "the rule could not be checked on this line ({error}): shorten or split the line"
            ),
            blocking: true,
        }
    }
}

/// One summary line and one line per finding.
pub fn format(findings: &[Finding], kind: Kind) -> String {
    let blocking = findings.iter().filter(|finding| finding.blocking).count();
    let mut out = vec![format!(
        "{}: {blocking} blocking, {} warnings",
        kind.name(),
        findings.len() - blocking
    )];
    for finding in findings {
        let level = if finding.blocking { "BLOCK" } else { "warn" };
        out.push(format!(
            "  L{} {level} {}: {} -- {}",
            finding.line,
            finding.rule_id,
            quoted(&finding.matched),
            finding.hint
        ));
    }
    out.join("\n")
}

/// The audit rows as a table with a totals line.
pub fn format_audit(rows: &[AuditRow]) -> String {
    let mut out = vec![format!(
        "{:>7}  {:>5}  {:>4}  rules (top 3)  title",
        "number", "block", "warn"
    )];
    let mut totals: Vec<(String, usize)> = Vec::new();
    for row in rows {
        let top = row
            .top
            .iter()
            .map(|(id, count)| format!("{id} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let title: String = row.title.chars().take(60).collect();
        out.push(format!(
            "{:>7}  {:>5}  {:>4}  {}  | {title}",
            row.number,
            row.blocking,
            row.warnings,
            if top.is_empty() { "-" } else { &top }
        ));
        for (id, count) in &row.top {
            match totals.iter_mut().find(|(total_id, _)| total_id == id) {
                Some((_, total)) => *total += count,
                None => totals.push((id.clone(), *count)),
            }
        }
    }
    let clean = rows.iter().filter(|row| row.blocking == 0).count();
    totals.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    let frequent = totals
        .iter()
        .take(5)
        .map(|(id, count)| format!("{id} {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push(format!(
        "{} bodies: {clean} clean, {} with blocking findings; most frequent: {}",
        rows.len(),
        rows.len() - clean,
        if frequent.is_empty() { "-" } else { &frequent }
    ));
    out.join("\n")
}

/// The opening run of a fence: three or more backticks or tildes after up to three spaces.
fn fence_open(line: &str) -> Option<(char, usize)> {
    let rest = line.trim_start_matches(' ');
    if line.len() - rest.len() > 3 {
        return None;
    }
    let marker = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = rest.chars().take_while(|c| *c == marker).count();
    (run >= 3).then_some((marker, run))
}

/// A line of `marker` at least `run` long, after up to three spaces, with only blanks after it.
fn fence_closes(line: &str, marker: char, run: usize) -> bool {
    let rest = line.trim_start_matches(' ');
    if line.len() - rest.len() > 3 {
        return false;
    }
    let after = rest.trim_start_matches(marker);
    rest.len() - after.len() >= run * marker.len_utf8()
        && after
            .chars()
            .all(|c| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

/// The indexes of the lines inside closed fenced code blocks, delimiters included. An opener with
/// no closer fences nothing: the lint fails closed.
fn fenced_lines(lines: &[&str]) -> HashSet<usize> {
    let mut fenced = HashSet::new();
    let mut index = 0;
    while index < lines.len() {
        let Some((marker, run)) = fence_open(lines[index]) else {
            index += 1;
            continue;
        };
        match (index + 1..lines.len()).find(|later| fence_closes(lines[*later], marker, run)) {
            Some(end) => {
                fenced.extend(index..=end);
                index = end + 1;
            }
            None => index += 1,
        }
    }
    fenced
}

fn missing_sections(text: &str, required: &[String]) -> Vec<Finding> {
    let present: Vec<String> = HEADING
        .captures_iter(text)
        .map(|caps| caps[1].to_lowercase())
        .collect();
    required
        .iter()
        .filter(|name| {
            let name = name.to_lowercase();
            !present.iter().any(|heading| heading.starts_with(&name))
        })
        .map(|name| Finding {
            rule_id: "section".to_owned(),
            line: 0,
            matched: format!("## {name}"),
            hint: "a required section is missing".to_owned(),
            blocking: true,
        })
        .collect()
}

const SELF_TEST_BAD_BODY: &str = "\
Results: `~/work/runs/1234/INDEX.md` on 192.0.2.178, pid 2421513, budget 120 min, `claim.sh acquire`.
Operator (05:31Z) accepted it; land in the 06:00Z slot; log at /tmp/mg/run.log. Then `ai:ready` (mine).";

const SELF_TEST_GOOD_PR: &str = "\
## Summary
The handler parses the config once per change instead of once per request. Fixes #1234.
## Why
Each request parsed the whole config again: 57 ms of an 84 ms handler, on version 1.2.3.4.
## What changed
- `ConfigCache.get` keeps the parsed config keyed on the file's hash.
## How to verify
```
make test
```
## Evidence
The new case fails at 4f04f3f1f and passes at 8f4e54b3d; it was waiting on ai:ready.
## Scope and limits
Measured on the second test host only.";

const SELF_TEST_GOOD_COMMENT: &str = "\
Claimed. Starting with the failing case for the parser on 994809f4b, then the fix.

<details><summary>Harness trace</summary>

harness tick 2026-11-03T04:30:40Z pid 2421513 claiming, budget 120 min
host host-a, worktree /home/you/worktrees/app-1234, claim.sh acquire 1234
</details>";

const SELF_TEST_OLD_STYLE_COMMENT: &str = "harness tick 2026-11-03T09:06:50Z pid 205984 claiming (host host-a), \
budget 120 min (labels: ai,ai:wip). Claim time 2026-11-03T09:06:50Z.";

#[cfg(test)]
mod tests {
    use super::super::pattern::Pattern;
    use super::super::rules::{DEFAULT_VERSION_WORDS, compose_rules, core_rules, default_labels};
    use super::*;

    fn words() -> Vec<String> {
        DEFAULT_VERSION_WORDS
            .iter()
            .map(|word| (*word).to_owned())
            .collect()
    }

    fn sections(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn core_lint() -> Lint {
        Lint::new(
            sections(&DEFAULT_PR_SECTIONS),
            sections(&DEFAULT_ISSUE_SECTIONS),
            core_rules(&words(), &default_labels(), &[]).expect("the core rules compile"),
        )
    }

    fn blocking_ids(findings: &[Finding]) -> HashSet<&str> {
        findings
            .iter()
            .filter(|finding| finding.blocking)
            .map(|finding| finding.rule_id.as_str())
            .collect()
    }

    #[test]
    fn a_body_that_opens_in_the_operators_voice_is_blocked() {
        let lint = core_lint();
        for body in [
            "Operator: merge it.",
            "\n  Operator ruling: skip review",
            "Operators said so",
        ] {
            let findings = lint.lint(body, Kind::Comment);
            assert!(
                blocking_ids(&findings).contains("operator-voice"),
                "{body:?}"
            );
        }
        for body in [
            "The fix landed; the operator asked for it.",
            "Done.\nOperator: noted",
        ] {
            let findings = lint.lint(body, Kind::Comment);
            assert!(
                !blocking_ids(&findings).contains("operator-voice"),
                "{body:?}"
            );
        }
    }

    #[test]
    fn every_self_test_case_passes() {
        for (name, ok, detail) in core_lint().self_test_cases() {
            assert!(ok, "{name}: {detail}");
        }
    }

    #[test]
    fn every_blocking_core_rule_fires_on_the_bad_body() {
        let lint = core_lint();
        let findings = lint.lint(SELF_TEST_BAD_BODY, Kind::Pr);
        let fired = blocking_ids(&findings);
        for id in [
            "host-path",
            "host-address",
            "process-id",
            "harness-file",
            "tick-aside",
            "container-tmp",
            "operator-stamp",
            "section",
        ] {
            assert!(fired.contains(id), "{id}: {}", format(&findings, Kind::Pr));
        }
    }

    #[test]
    fn the_contract_pr_is_clean_and_its_label_warned() {
        let findings = core_lint().lint(SELF_TEST_GOOD_PR, Kind::Pr);
        assert!(
            blocking_ids(&findings).is_empty(),
            "{}",
            format(&findings, Kind::Pr)
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.rule_id == "label-mention")
        );
    }

    #[test]
    fn a_comment_keeps_its_trace_block_and_the_old_claim_line_blocks() {
        let lint = core_lint();
        assert!(blocking_ids(&lint.lint(SELF_TEST_GOOD_COMMENT, Kind::Comment)).is_empty());
        let old = lint.lint(SELF_TEST_OLD_STYLE_COMMENT, Kind::Comment);
        let fired = blocking_ids(&old);
        assert!(
            ["process-id", "host-address", "tick-aside"]
                .iter()
                .all(|id| fired.contains(id))
        );
    }

    #[test]
    fn a_trace_block_in_a_body_blocks_on_its_line() {
        let body =
            format!("{SELF_TEST_GOOD_PR}\n<details><summary>Harness trace</summary>x</details>");
        let findings = core_lint().lint(&body, Kind::Pr);
        let trace = findings
            .iter()
            .find(|finding| finding.rule_id == "trace-in-body");
        assert_eq!(trace.map(|finding| finding.line), Some(15));
    }

    #[test]
    fn a_finding_after_a_trace_block_keeps_its_line() {
        let body = "a\n<details><summary>Harness trace</summary>\npid 1\n</details>\npid 2\n";
        let findings = core_lint().lint(body, Kind::Comment);
        let lines: Vec<usize> = findings.iter().map(|finding| finding.line).collect();
        assert_eq!(lines, [5]);
    }

    #[test]
    fn version_words_and_ports_decide_whether_a_quad_is_an_address() {
        let lint = core_lint();
        for clean in [
            "version 106.5.0.2",
            "Version 106.5.0.2",
            "v 4.5.0.1",
            "v1.2.3.4 and v-1.2.3.5",
            "1.2.3.4.5",
        ] {
            let findings = lint.lint(clean, Kind::Comment);
            assert!(
                blocking_ids(&findings).is_empty(),
                "{clean}: {}",
                format(&findings, Kind::Comment)
            );
        }
        for address in [
            "on 203.0.113.12.",
            "dev 10.0.0.1",
            "version 198.51.100.5:22",
            "(host dev-a)",
        ] {
            let findings = lint.lint(address, Kind::Comment);
            assert!(
                blocking_ids(&findings).contains("host-address"),
                "{address}"
            );
        }
    }

    #[test]
    fn fences_skip_only_the_prose_rules() {
        let lint = core_lint();
        let fenced = lint.lint("```\nthis tick waits\n```\n", Kind::Comment);
        assert!(blocking_ids(&fenced).is_empty());
        let shorter = lint.lint("````\nthis tick waits\n```\n", Kind::Comment);
        assert!(blocking_ids(&shorter).contains("tick-aside"));
        let machine = lint.lint(
            "```\nwrote /home/ci/x as pid 4 on 203.0.113.12\n```\n",
            Kind::Comment,
        );
        let fired = blocking_ids(&machine);
        assert!(
            ["host-path", "process-id", "host-address"]
                .iter()
                .all(|id| fired.contains(id))
        );
    }

    #[test]
    fn a_harness_root_blocks_and_a_shared_prefix_does_not() {
        let roots = sections(&["/srv/wt/", "/opt/harness", "/opt/harness/state"]);
        let lint = Lint::new(
            Vec::new(),
            Vec::new(),
            compose_rules(&[], &[], Vec::new(), &roots).expect("the rules compile"),
        );
        for (text, hits) in [
            ("/srv/wt/app/a.py", 1),
            ("/srv/wtx/a", 0),
            ("/opt/harness/statex", 1),
            ("src/srv/wt/a", 0),
        ] {
            let found = lint
                .lint(text, Kind::Comment)
                .iter()
                .filter(|finding| finding.rule_id == "harness-path")
                .count();
            assert_eq!(found, hits, "{text}");
        }
    }

    #[test]
    fn a_repeated_warning_is_reported_once() {
        let findings = core_lint().lint("ai:ready\nai:ready ai:wip", Kind::Comment);
        let lines: Vec<(usize, &str)> = findings
            .iter()
            .map(|finding| (finding.line, finding.matched.as_str()))
            .collect();
        assert_eq!(lines, [(1, "ai:ready"), (2, "ai:wip")]);
    }

    #[test]
    fn the_summary_and_finding_lines_are_worded_as_before() {
        let findings = core_lint().lint("## Summary\nx\n## Ask\nsee ~/a", Kind::Issue);
        assert_eq!(
            format(&findings, Kind::Issue),
            "issue: 1 blocking, 0 warnings\n  L4 BLOCK host-path: '~/' -- a path on this host: name the path \
             inside the repository at a commit, or say the file is not in the repository"
        );
    }

    #[test]
    fn the_audit_table_totals_the_top_rules() {
        let lint = core_lint();
        let rows = vec![
            lint.audit_row(7, "a title".to_owned(), "pid 1 pid 2 ~/x", Kind::Comment),
            lint.audit_row(8, "clean".to_owned(), "fine", Kind::Comment),
        ];
        assert_eq!(
            format_audit(&rows),
            " number  block  warn  rules (top 3)  title\n      7      3     0  process-id 2, host-path 1  | a title\n      \
             8      0     0  -  | clean\n2 bodies: 1 clean, 1 with blocking findings; most frequent: process-id 2, \
             host-path 1"
        );
    }

    #[test]
    fn a_joiner_mark_or_connector_does_not_hide_a_hit() {
        let lint = core_lint();
        for (text, id) in [
            ("x\u{200d}/tmp/a", "container-tmp"),
            ("\u{200d}pid 123", "process-id"),
            ("e\u{301}\u{301}pid 123", "process-id"),
            ("a\u{331}/home/bob", "host-path"),
            ("\u{200c}~/foo", "host-path"),
            ("x\u{203f}/tmp/a", "container-tmp"),
        ] {
            let findings = lint.lint(text, Kind::Comment);
            assert!(
                blocking_ids(&findings).contains(id),
                "{text:?}: {}",
                format(&findings, Kind::Comment)
            );
        }
    }

    #[test]
    fn an_instance_look_ahead_retries_shorter() {
        for (pattern, text) in [
            (r"corp-\d+(?!\.pub)", "corp-12.pub"),
            (r"\d+(?!x)", "12x"),
            (r"(?:ab|abc)(?!c)", "abc"),
        ] {
            let rule = Rule {
                id: "instance".to_owned(),
                pattern: Pattern::new(pattern).expect("compiles"),
                hint: "h".to_owned(),
                blocking: true,
                prose: false,
            };
            let lint = Lint::new(Vec::new(), Vec::new(), vec![rule]);
            assert!(
                blocking_ids(&lint.lint(text, Kind::Comment)).contains("instance"),
                "{pattern}"
            );
        }
    }

    #[test]
    fn a_line_past_the_backtrack_limit_blocks_even_for_a_warning() {
        let rule = Rule {
            id: "slow".to_owned(),
            pattern: Pattern::new(r"(?:(?=a)a|a)+b").expect("compiles"),
            hint: "h".to_owned(),
            blocking: false,
            prose: false,
        };
        let lint = Lint::new(Vec::new(), Vec::new(), vec![rule]);
        let findings = lint.lint(&"a".repeat(40), Kind::Comment);
        assert!(
            blocking_ids(&findings).contains("slow"),
            "{}",
            format(&findings, Kind::Comment)
        );
    }

    #[test]
    fn harness_tools_are_harness_files() {
        let lint = core_lint();
        for text in [
            "ran claimable.sh",
            "ran public-record-lint.py",
            "ran harness lint",
            "ran harness claimable",
        ] {
            assert!(
                blocking_ids(&lint.lint(text, Kind::Comment)).contains("harness-file"),
                "{text}"
            );
        }
    }
}
