#!/usr/bin/env python3
"""Lint what a tick is about to post to GitHub against prompts/public-record.md.

The public record (issue bodies, PR bodies, comments) is read by people who have the repository and
nothing else. The lint blocks what such a reader cannot use: paths on this host, host names and
addresses, process ids, harness file names, tick-to-tick asides. It warns on harness labels in prose. A
`<details><summary>Harness trace</summary>` block in a comment is exempt: that is where the harness's own
bookkeeping goes, the claim protocol line (`harness tick … pid N claiming`) included, because the launcher
finds that line anywhere in a comment. Inside a closed fenced code block only the prose rules (tick
asides, an installation's rules marked prose, the warnings) are skipped, so quoted output does not block
on harness vocabulary; every rule that identifies the machine still applies there.

    public_record_lint.py --kind pr BODY.md          lint a file; exit 1 on a blocking hit
    public_record_lint.py --kind comment -           lint a comment from stdin
    public_record_lint.py --pr 2138                  lint a PR body fetched with gh
    public_record_lint.py --issue 2137               lint an issue body fetched with gh
    public_record_lint.py --audit prs --last 20      one line per body over the last 20 PRs
    public_record_lint.py --self-test                the built-in samples, core rules only

GH_REPO (or --repo) names the repository for --pr, --issue and --audit. PUBLIC_RECORD_PR_SECTIONS and
PUBLIC_RECORD_ISSUE_SECTIONS (comma-separated heading names) override the sections a body must carry.
The harness labels the warning names come from WIP_LABEL, READY_LABEL, SIGNOFF_LABEL, OPERATOR_LABEL,
LONG_LABEL, LANE_LABEL, FIRST_LABEL and LOW_LABEL.

Any path under WORKTREE_ROOT, HARNESS_HOME or HARNESS_STATE, each when set, blocks.

The installation's rules load from PUBLIC_RECORD_RULES, else from $HARNESS_INSTANCE_DIR/lint_rules.json
(HARNESS_INSTANCE_DIR defaults to the checkout's instance/); an instance dir without that file is named
on stderr on every run. The file is a JSON object:

    {"version_words": ["SDK"],            words a dotted quad may follow as a version, not an address
     "labels": ["ai:plan"],               more harness labels to warn on
     "rules": [{"id": "...", "pattern": "...", "hint": "...",
                "blocking": true,         false makes it a warning
                "prose": false}]}         true skips it inside a closed fence

A rule whose id is a core rule's replaces that rule. Exit 0 clean (warnings allowed), 1 blocking
findings, 2 usage, rules-file or fetch error.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass
from typing import Any, Sequence

_OCTET: str = r"(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)"
_IPV4: str = rf"{_OCTET}(?:\.{_OCTET}){{3}}"
DEFAULT_VERSION_WORDS: tuple[str, ...] = ("v", "version")
# The label settings tick.sh and claimable.sh read, with their defaults.
LABEL_SETTINGS: tuple[tuple[str, str], ...] = (
    ("WIP_LABEL", "ai:wip"), ("READY_LABEL", "ai:ready"), ("SIGNOFF_LABEL", "ai:signoff"),
    ("OPERATOR_LABEL", "ai:operator"), ("LONG_LABEL", "ai:long"), ("LANE_LABEL", ""),
    ("FIRST_LABEL", "ai:first"), ("LOW_LABEL", "ai:low"))
DEFAULT_LABELS: tuple[str, ...] = tuple(default for _, default in LABEL_SETTINGS if default)


def host_address_pattern(version_words: Sequence[str]) -> str:
    """Returns the address pattern, letting a bare dotted quad pass right after a version word.

    A dotted quad is also a version number (`version 1.2.3.4`), so a bare one passes only right after a
    version word; with a port it is an address whatever precedes it. A word joined by a hyphen
    (`v-1.2.3.4`) or with no space (`v1.2.3.4`) already passes: the quad must not follow a word
    character, a dot or a hyphen.
    """
    prefix: str = ""
    for word in version_words:
        head: str = f"[{word[0].upper()}{word[0].lower()}]" if word[0].isalpha() else re.escape(word[0])
        prefix += rf"(?<!\b{head}{re.escape(word[1:])} )"
    return rf"(?<![\w.]){_IPV4}:\d{{1,5}}\b|(?<![\w.-]){prefix}{_IPV4}(?![\w-]|\.\d|:\d)"


@dataclass(frozen=True)
class Rule:
    """One pattern the lint looks for, with the hint printed beside a hit.

    A prose rule is skipped inside a closed fence, so quoted output may carry its words.
    """

    rule_id: str
    pattern: re.Pattern[str]
    hint: str
    blocking: bool
    prose: bool = False


def core_rules(version_words: Sequence[str], labels: Sequence[str],
               harness_roots: Sequence[str] = ()) -> tuple[Rule, ...]:
    """Returns the rules every installation gets, blocking ones first.

    Args:
        version_words: Words a dotted quad may follow as a version.
        labels: Harness labels to warn on in prose.
        harness_roots: The worktree root, the harness checkout and its state; any path under one blocks.
    """
    rules: list[Rule] = []
    roots: list[str] = [root.rstrip("/") for root in harness_roots if root.rstrip("/")]
    if roots:
        alternatives: str = "|".join(re.escape(root) for root in sorted(set(roots), key=len, reverse=True))
        rules.append(Rule("harness-path", re.compile(rf"(?<![\w.-])(?:{alternatives})(?![\w.-])"),
                          "a path in the harness's own trees: name the path inside the repository at a commit",
                          True))
    rules += [
        Rule("host-path", re.compile(r"(?<![\w./-])(?:/home/\w+|~/|\$HOME\b|/mnt/\w+)"),
             "a path on this host: name the path inside the repository at a commit, or say the file is not in "
             "the repository", True),
        Rule("host-address", re.compile(rf"{host_address_pattern(version_words)}|\bssh://|\bDOCKER_HOST\b|\(host \S+\)"),
             "a host or an address: say what the host did ('the second test host'), never which", True),
        Rule("process-id", re.compile(r"\bpid \d+\b"),
             "a process id belongs in a comment's Harness trace block", True),
        Rule("harness-file",
             re.compile(r"\b(?:claim|claimable|merge-lock|tick-pid|tick|usage_ok|usage-lib|pr_sweep|worktree_gc|"
                        r"instance-lib|public_record_lint|dashboard)\.(?:sh|py)\b|\bstate/(?:locks|logs|paused)\b"),
             "a harness file: name the repository command that does the same", True),
        Rule("tick-aside",
             re.compile(r"\(mine\)|\bbudget \d+ ?min\b|\bharness tick\b|\bresume recipe\b|"
                        r"\b(?:next|this|my|a later) tick\b|\bmerge (?:slot|lock)\b|\d{2}:\d{2}Z slot\b"),
             "a tick-to-tick aside: the reader is not the next tick", True, prose=True),
        Rule("container-tmp", re.compile(r"(?<![\w./-])/tmp/"),
             "a path in a container or on this host", True),
        Rule("operator-stamp",
             re.compile(r"\b[Oo]perator(?:'s)?,? ?\(?\d{2}:\d{2}Z|\b\d{2}:\d{2}Z (?:ruling|sign-off|post|comment|"
                        r"answer)\b|\(\d{2}:\d{2}Z\)"),
             "a timestamp in place of a decision: write what was decided and link the comment", True),
    ]
    if labels:
        alternatives: str = "|".join(re.escape(label) for label in sorted(set(labels), key=len, reverse=True))
        rules.append(Rule("label-mention", re.compile(rf"(?<![\w:-])(?:{alternatives})(?![\w:-])"),
                          "a harness label in prose: the labels show on the issue; say the state in words", False))
    return tuple(rules)


@dataclass(frozen=True)
class Finding:
    """One hit: the rule, where it is, what matched."""

    rule_id: str
    line_number: int
    matched: str
    hint: str
    blocking: bool


@dataclass(frozen=True)
class AuditRow:
    """One body's totals in an audit table."""

    number: int
    title: str
    blocking: int
    warnings: int
    top_rules: str


class RulesFileError(ValueError):
    """The installation's rules file cannot be used as written."""


def load_rules_file(path: str) -> tuple[tuple[str, ...], tuple[str, ...], tuple[Rule, ...]]:
    """Reads an installation's lint_rules.json.

    Args:
        path: The file, laid out as the module docstring says.

    Returns:
        The extra version words, the extra labels, and the rules.

    Raises:
        RulesFileError: The file is missing, is not JSON, or does not have that layout.
    """
    try:
        with open(path, encoding="utf-8") as handle:
            data: Any = json.load(handle)
    except (OSError, json.JSONDecodeError) as error:
        raise RulesFileError(f"{path}: {error}") from error
    if not isinstance(data, dict) or set(data) - {"version_words", "labels", "rules"}:
        raise RulesFileError(f"{path}: expected an object with only version_words, labels and rules")

    def strings(key: str) -> tuple[str, ...]:
        value: Any = data.get(key, [])
        if not isinstance(value, list) or not all(isinstance(item, str) and item for item in value):
            raise RulesFileError(f"{path}: {key} must be a list of non-empty strings")
        return tuple(value)

    raw_rules: Any = data.get("rules", [])
    if not isinstance(raw_rules, list):
        raise RulesFileError(f"{path}: rules must be a list")
    rules: list[Rule] = []
    for index, raw in enumerate(raw_rules):
        where: str = f"{path}: rules[{index}]"
        if not isinstance(raw, dict) or set(raw) - {"id", "pattern", "hint", "blocking", "prose"}:
            raise RulesFileError(f"{where}: expected an object with id, pattern, hint, and optional blocking, prose")
        if not all(isinstance(raw.get(key), str) and raw[key] for key in ("id", "pattern", "hint")):
            raise RulesFileError(f"{where}: id, pattern and hint must be non-empty strings")
        if not all(isinstance(raw.get(key, False), bool) for key in ("blocking", "prose")):
            raise RulesFileError(f"{where}: blocking and prose must be true or false")
        try:
            pattern: re.Pattern[str] = re.compile(raw["pattern"])
        except re.error as error:
            raise RulesFileError(f"{where} ({raw['id']}): bad pattern: {error}") from error
        rules.append(Rule(raw["id"], pattern, raw["hint"], raw.get("blocking", True), raw.get("prose", False)))
    return strings("version_words"), strings("labels"), tuple(rules)


class PublicRecordLint:
    """Checks a body against the public-record contract and reports what it finds."""

    KINDS: tuple[str, ...] = ("pr", "issue", "comment")
    DEFAULT_PR_SECTIONS: tuple[str, ...] = ("Summary", "Why", "What changed", "How to verify", "Evidence",
                                            "Scope and limits")
    DEFAULT_ISSUE_SECTIONS: tuple[str, ...] = ("Summary", "Ask")
    TRACE_BLOCK: re.Pattern[str] = re.compile(
        r"<details>\s*<summary>[^<]*harness trace[^<]*</summary>.*?</details>", re.IGNORECASE | re.DOTALL)
    HEADING: re.Pattern[str] = re.compile(r"^#{1,3}\s+(.+?)\s*$", re.MULTILINE)
    FENCE_OPEN: re.Pattern[str] = re.compile(r"^ {0,3}(`{3,}|~{3,})")

    def __init__(self, repo: str | None, pr_sections: Sequence[str], issue_sections: Sequence[str],
                 rules: Sequence[Rule]) -> None:
        self._repo: str | None = repo
        self._pr_sections: tuple[str, ...] = tuple(pr_sections)
        self._issue_sections: tuple[str, ...] = tuple(issue_sections)
        self._blocking: tuple[Rule, ...] = tuple(rule for rule in rules if rule.blocking)
        self._warnings: tuple[Rule, ...] = tuple(rule for rule in rules if not rule.blocking)

    @staticmethod
    def compose_rules(version_words: Sequence[str], labels: Sequence[str], installation_rules: Sequence[Rule],
                      harness_roots: Sequence[str] = ()) -> tuple[Rule, ...]:
        """Returns the core rules with the installation's added, one with a core rule's id replacing it."""
        replacing: dict[str, Rule] = {rule.rule_id: rule for rule in installation_rules}
        core: tuple[Rule, ...] = core_rules((*DEFAULT_VERSION_WORDS, *version_words), (*labels,), harness_roots)
        core_ids: set[str] = {rule.rule_id for rule in core}
        return (tuple(replacing.get(rule.rule_id, rule) for rule in core)
                + tuple(rule for rule in installation_rules if rule.rule_id not in core_ids))

    def lint(self, body: str, kind: str) -> tuple[Finding, ...]:
        """Lints one body.

        Args:
            body: The text as it would be posted.
            kind: One of KINDS.

        Returns:
            Every finding, blocking ones first, in line order.
        """
        if kind not in self.KINDS:
            raise ValueError(f"kind must be one of {self.KINDS}, not {kind!r}")
        findings: list[Finding] = []
        text: str = body
        trace_match: re.Match[str] | None = self.TRACE_BLOCK.search(body)
        if kind == "comment":
            text = self.TRACE_BLOCK.sub(lambda match: "\n" * match.group(0).count("\n"), body)
        elif trace_match is not None:
            findings.append(Finding("trace-in-body", 1 + body[:trace_match.start()].count("\n"),
                                    "<details><summary>Harness trace</summary>",
                                    "a Harness trace belongs in a comment, never in an issue or PR body", True))
        lines: list[str] = text.split("\n")
        fenced: set[int] = self._fenced_lines(lines)
        seen_warnings: set[tuple[str, str]] = set()
        for line_index, line in enumerate(lines):
            if line_index in fenced:
                for rule in self._blocking:
                    if not rule.prose:
                        for match in rule.pattern.finditer(line):
                            findings.append(Finding(rule.rule_id, line_index + 1, match.group(0), rule.hint, True))
                continue
            for rule in self._blocking:
                for match in rule.pattern.finditer(line):
                    findings.append(Finding(rule.rule_id, line_index + 1, match.group(0), rule.hint, True))
            for rule in self._warnings:
                for match in rule.pattern.finditer(line):
                    key: tuple[str, str] = (rule.rule_id, match.group(0))
                    if key in seen_warnings:
                        continue
                    seen_warnings.add(key)
                    findings.append(Finding(rule.rule_id, line_index + 1, match.group(0), rule.hint, False))
        if kind in ("pr", "issue"):
            findings.extend(self._missing_sections(text, self._pr_sections if kind == "pr" else self._issue_sections))
        findings.sort(key=lambda finding: (not finding.blocking, finding.line_number))
        return tuple(findings)

    def format(self, findings: Sequence[Finding], kind: str) -> str:
        """Renders findings as one summary line and one line per finding."""
        blocking: int = sum(1 for finding in findings if finding.blocking)
        warnings: int = len(findings) - blocking
        out: list[str] = [f"{kind}: {blocking} blocking, {warnings} warnings"]
        for finding in findings:
            level: str = "BLOCK" if finding.blocking else "warn"
            out.append(f"  L{finding.line_number} {level} {finding.rule_id}: {finding.matched!r} -- {finding.hint}")
        return "\n".join(out)

    def fetch_body(self, kind: str, number: int) -> tuple[str, str]:
        """Fetches a PR's or an issue's title and body with gh.

        Args:
            kind: "pr" or "issue".
            number: Its number in the repository.

        Returns:
            The title and the body.
        """
        command: list[str] = ["gh", kind, "view", str(number), "--json", "title,body"]
        if self._repo:
            command += ["--repo", self._repo]
        result: subprocess.CompletedProcess[str] = subprocess.run(command, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            raise RuntimeError(f"gh failed: {result.stderr.strip()}")
        payload: dict[str, str] = json.loads(result.stdout)
        return payload.get("title", ""), payload.get("body", "") or ""

    def audit(self, what: str, last: int, base: str | None, label: str | None) -> tuple[AuditRow, ...]:
        """Lints the last N PR or issue bodies and returns one row each.

        Args:
            what: "prs" or "issues".
            last: How many, newest first.
            base: For PRs, restrict to this base branch.
            label: For issues, restrict to this label.

        Returns:
            One row per body, newest first.
        """
        kind: str = "pr" if what == "prs" else "issue"
        command: list[str] = ["gh", kind, "list", "--state", "all", "--limit", str(last), "--json", "number,title,body"]
        if self._repo:
            command += ["--repo", self._repo]
        if base and kind == "pr":
            command += ["--base", base]
        if label and kind == "issue":
            command += ["--label", label]
        result: subprocess.CompletedProcess[str] = subprocess.run(command, capture_output=True, text=True, check=False)
        if result.returncode != 0:
            raise RuntimeError(f"gh failed: {result.stderr.strip()}")
        rows: list[AuditRow] = []
        for item in json.loads(result.stdout):
            findings: tuple[Finding, ...] = self.lint(item.get("body") or "", kind)
            counts: Counter[str] = Counter(finding.rule_id for finding in findings if finding.blocking)
            top: str = ", ".join(f"{rule_id} {count}" for rule_id, count in counts.most_common(3))
            rows.append(AuditRow(item["number"], item["title"], sum(counts.values()),
                                 sum(1 for finding in findings if not finding.blocking), top))
        return tuple(rows)

    @staticmethod
    def format_audit(rows: Sequence[AuditRow]) -> str:
        """Renders audit rows as a table with a totals line."""
        out: list[str] = [f"{'number':>7}  {'block':>5}  {'warn':>4}  rules (top 3)  title"]
        totals: Counter[str] = Counter()
        for row in rows:
            out.append(f"{row.number:>7}  {row.blocking:>5}  {row.warnings:>4}  {row.top_rules or '-'}  | {row.title[:60]}")
            for part in filter(None, row.top_rules.split(", ")):
                rule_id, count = part.rsplit(" ", 1)
                totals[rule_id] += int(count)
        clean: int = sum(1 for row in rows if row.blocking == 0)
        out.append(f"{len(rows)} bodies: {clean} clean, {len(rows) - clean} with blocking findings; "
                   f"most frequent: {', '.join(f'{r} {c}' for r, c in totals.most_common(5)) or '-'}")
        return "\n".join(out)

    def self_test(self) -> bool:
        """Runs the built-in samples and prints PASS/FAIL lines.

        Returns:
            True when every sample behaves as the contract says.
        """
        bad_body: str = "\n".join([
            "Results: `~/work/runs/1234/INDEX.md` on 192.0.2.178, pid 2421513, budget 120 min, `claim.sh acquire`.",
            "Operator (05:31Z) accepted it; land in the 06:00Z slot; log at /tmp/mg/run.log. Then `ai:ready` (mine).",
        ])
        good_pr: str = "\n".join([
            "## Summary", "The handler parses the config once per change instead of once per request. Fixes #1234.",
            "## Why", "Each request parsed the whole config again: 57 ms of an 84 ms handler, on version 1.2.3.4.",
            "## What changed", "- `ConfigCache.get` keeps the parsed config keyed on the file's hash.",
            "## How to verify", "```", "make test", "```",
            "## Evidence", "The new case fails at 4f04f3f1f and passes at 8f4e54b3d; it was waiting on ai:ready.",
            "## Scope and limits", "Measured on the second test host only.",
        ])
        good_comment: str = "\n".join([
            "Claimed. Starting with the failing case for the parser on 994809f4b, then the fix.",
            "", "<details><summary>Harness trace</summary>", "",
            "harness tick 2026-11-03T04:30:40Z pid 2421513 claiming, budget 120 min",
            "host host-a, worktree /home/you/worktrees/app-1234, claim.sh acquire 1234",
            "</details>",
        ])
        old_style_comment: str = ("harness tick 2026-11-03T09:06:50Z pid 205984 claiming (host host-a), budget 120 min "
                                  "(labels: ai,ai:wip). Claim time 2026-11-03T09:06:50Z.")
        passed: bool = True

        def check(name: str, condition: bool, detail: str) -> None:
            nonlocal passed
            passed = passed and condition
            print(f"{'PASS' if condition else 'FAIL'} {name}" + ("" if condition else f" :: {detail}"))

        bad_findings: tuple[Finding, ...] = self.lint(bad_body, "pr")
        fired: set[str] = {finding.rule_id for finding in bad_findings if finding.blocking}
        for rule in self._blocking:
            check(f"{rule.rule_id} fires on the bad body", rule.rule_id in fired, self.format(bad_findings, "pr"))
        check("a body without the sections is blocked", "section" in fired, self.format(bad_findings, "pr"))
        good_findings: tuple[Finding, ...] = self.lint(good_pr, "pr")
        check("a PR body written to the contract has no blocking finding",
              not any(finding.blocking for finding in good_findings), self.format(good_findings, "pr"))
        check("the PR body's label in prose is warned, not blocked",
              any(finding.rule_id == "label-mention" for finding in good_findings), self.format(good_findings, "pr"))
        comment_findings: tuple[Finding, ...] = self.lint(good_comment, "comment")
        check("a comment with its protocol line inside the trace block is clean",
              not any(finding.blocking for finding in comment_findings), self.format(comment_findings, "comment"))
        old_style_findings: tuple[Finding, ...] = self.lint(old_style_comment, "comment")
        check("the old claim line in the open is blocked for its pid, host and budget",
              {"process-id", "host-address", "tick-aside"} <= {f.rule_id for f in old_style_findings if f.blocking},
              self.format(old_style_findings, "comment"))
        trace_in_body: tuple[Finding, ...] = self.lint(good_pr + "\n<details><summary>Harness trace</summary>x</details>",
                                                      "pr")
        check("a trace block in a body is blocked", any(finding.rule_id == "trace-in-body" for finding in trace_in_body),
              self.format(trace_in_body, "pr"))
        return passed

    def _fenced_lines(self, lines: Sequence[str]) -> set[int]:
        """Returns the indexes of lines inside closed fenced code blocks, delimiters included.

        A fence closes on a line of the opener's character, at least as long, indented up to three
        spaces. An opener with no closer fences nothing: the lint fails closed.
        """
        fenced: set[int] = set()
        index: int = 0
        while index < len(lines):
            opener: re.Match[str] | None = self.FENCE_OPEN.match(lines[index])
            if opener is None:
                index += 1
                continue
            marker: str = opener.group(1)
            closer: re.Pattern[str] = re.compile(rf"^ {{0,3}}{re.escape(marker[0])}{{{len(marker)},}}\s*$")
            end: int | None = next((later for later in range(index + 1, len(lines)) if closer.match(lines[later])),
                                   None)
            if end is None:
                index += 1
                continue
            fenced.update(range(index, end + 1))
            index = end + 1
        return fenced

    def _missing_sections(self, text: str, required: Sequence[str]) -> list[Finding]:
        present: list[str] = [heading.lower() for heading in self.HEADING.findall(text)]
        missing: list[Finding] = []
        for name in required:
            if not any(heading.startswith(name.lower()) for heading in present):
                missing.append(Finding("section", 0, f"## {name}", "a required section is missing", True))
        return missing

    @staticmethod
    def rules_path() -> tuple[str | None, str]:
        """Returns the installation's rules file and the instance dir it was looked for in.

        The file is PUBLIC_RECORD_RULES when set, else the instance's lint_rules.json when it exists.
        """
        home: str = os.environ.get("HARNESS_HOME") or os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        instance: str = os.environ.get("HARNESS_INSTANCE_DIR") or os.path.join(home, "instance")
        explicit: str = os.environ.get("PUBLIC_RECORD_RULES", "")
        if explicit:
            return explicit, instance
        candidate: str = os.path.join(instance, "lint_rules.json")
        return (candidate if os.path.isfile(candidate) else None), instance

    @staticmethod
    def main(argv: Sequence[str]) -> int:
        """Parses the command line and runs one of the modes.

        Args:
            argv: The arguments after the program name.

        Returns:
            The exit code.
        """
        parser: argparse.ArgumentParser = argparse.ArgumentParser(description=__doc__,
                                                                  formatter_class=argparse.RawDescriptionHelpFormatter)
        parser.add_argument("body", nargs="?", help="a file holding the body, or - for stdin")
        parser.add_argument("--kind", choices=PublicRecordLint.KINDS)
        parser.add_argument("--pr", type=int, help="lint this PR's body, fetched with gh")
        parser.add_argument("--issue", type=int, help="lint this issue's body, fetched with gh")
        parser.add_argument("--audit", choices=("prs", "issues"), help="one line per body over the last N")
        parser.add_argument("--last", type=int, default=20, help="how many bodies --audit reads (default 20)")
        parser.add_argument("--base", help="--audit prs: only PRs into this branch")
        parser.add_argument("--label", help="--audit issues: only issues with this label")
        parser.add_argument("--repo", default=os.environ.get("GH_REPO"), help="OWNER/REPO (default: $GH_REPO)")
        parser.add_argument("--self-test", action="store_true", help="run the built-in samples")
        args: argparse.Namespace = parser.parse_args(argv)
        pr_sections: tuple[str, ...] = tuple(
            part.strip() for part in os.environ.get("PUBLIC_RECORD_PR_SECTIONS", "").split(",") if part.strip()
        ) or PublicRecordLint.DEFAULT_PR_SECTIONS
        issue_sections: tuple[str, ...] = tuple(
            part.strip() for part in os.environ.get("PUBLIC_RECORD_ISSUE_SECTIONS", "").split(",") if part.strip()
        ) or PublicRecordLint.DEFAULT_ISSUE_SECTIONS
        if args.self_test:
            core: tuple[Rule, ...] = core_rules(DEFAULT_VERSION_WORDS, DEFAULT_LABELS)
            return 0 if PublicRecordLint(args.repo, pr_sections, issue_sections, core).self_test() else 1
        labels: list[str] = [label for name, default in LABEL_SETTINGS
                             if (label := os.environ.get(name, default))]
        version_words: tuple[str, ...] = ()
        installation_rules: tuple[Rule, ...] = ()
        rules_file, instance = PublicRecordLint.rules_path()
        if rules_file is None and os.path.isdir(instance):
            # An instance without its rules is likely a missed copy, and the lint then passes what
            # the project's rules would block, so say so on every run.
            print(f"public_record_lint: no lint_rules.json in {instance}; only the core rules applied",
                  file=sys.stderr)
        if rules_file is not None:
            try:
                version_words, extra_labels, installation_rules = load_rules_file(rules_file)
            except RulesFileError as error:
                print(f"public_record_lint: {error}", file=sys.stderr)
                return 2
            labels += extra_labels
        lint: PublicRecordLint = PublicRecordLint(
            args.repo, pr_sections, issue_sections,
            PublicRecordLint.compose_rules(version_words, labels, installation_rules,
                                           [os.environ.get(name, "") for name in
                                            ("WORKTREE_ROOT", "HARNESS_HOME", "HARNESS_STATE")]))
        kind: str
        body: str
        try:
            if args.audit:
                print(PublicRecordLint.format_audit(lint.audit(args.audit, args.last, args.base, args.label)))
                return 0
            if args.pr is not None or args.issue is not None:
                kind = "pr" if args.pr is not None else "issue"
                number: int = args.pr if args.pr is not None else args.issue
                title, body = lint.fetch_body(kind, number)
                print(f"{kind} #{number}: {title}")
            else:
                if not args.body or not args.kind:
                    parser.error("a body file (or -) and --kind are required")
                kind = args.kind
                body = sys.stdin.read() if args.body == "-" else open(args.body, encoding="utf-8").read()
        except (RuntimeError, OSError, json.JSONDecodeError) as error:
            print(f"public_record_lint: {error}", file=sys.stderr)
            return 2
        findings: tuple[Finding, ...] = lint.lint(body, kind)
        print(lint.format(findings, kind))
        return 1 if any(finding.blocking for finding in findings) else 0


if __name__ == "__main__":
    sys.exit(PublicRecordLint.main(sys.argv[1:]))
