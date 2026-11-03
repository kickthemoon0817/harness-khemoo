#!/usr/bin/env python3
"""Lint what a tick is about to post to GitHub against prompts/public-record.md.

The public record (issue bodies, PR bodies, comments) is read by people who have the repository and
nothing else. The lint blocks what such a reader cannot use: paths on this host, host names and
addresses, process ids, GPU slots and leases, harness file names, tick-to-tick asides. It warns on
shorthand a body may leave undefined. A comment's first line (the claim protocol) keeps its pid, and a
`<details><summary>Harness trace</summary>` block in a comment is exempt: that is where the harness's own
bookkeeping goes.

    public_record_lint.py --kind pr BODY.md          lint a file; exit 1 on a blocking hit
    public_record_lint.py --kind comment -           lint a comment from stdin
    public_record_lint.py --pr 2138                  lint a PR body fetched with gh
    public_record_lint.py --issue 2137               lint an issue body fetched with gh
    public_record_lint.py --audit prs --last 20      one line per body over the last 20 PRs
    public_record_lint.py --self-test                the built-in samples

GH_REPO (or --repo) names the repository for --pr, --issue and --audit. PUBLIC_RECORD_PR_SECTIONS and
PUBLIC_RECORD_ISSUE_SECTIONS (comma-separated heading names) override the sections a body must carry.
Exit 0 clean (warnings allowed), 1 blocking findings, 2 usage or fetch error.
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
from typing import Sequence


@dataclass(frozen=True)
class Rule:
    """One pattern the lint looks for, with the hint printed beside a hit."""

    rule_id: str
    pattern: re.Pattern[str]
    hint: str
    blocking: bool


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


class PublicRecordLint:
    """Checks a body against the public-record contract and reports what it finds."""

    KINDS: tuple[str, ...] = ("pr", "issue", "comment")
    DEFAULT_PR_SECTIONS: tuple[str, ...] = ("Summary", "Why", "What changed", "How to verify", "Evidence",
                                            "Scope and limits")
    DEFAULT_ISSUE_SECTIONS: tuple[str, ...] = ("Summary", "Ask")
    TRACE_BLOCK: re.Pattern[str] = re.compile(
        r"<details>\s*<summary>[^<]*harness trace[^<]*</summary>.*?</details>", re.IGNORECASE | re.DOTALL)
    PROTOCOL_LINE: re.Pattern[str] = re.compile(r"^harness tick\b")
    PROTOCOL_EXEMPT_RULES: frozenset[str] = frozenset({"process-id", "tick-aside"})
    HEADING: re.Pattern[str] = re.compile(r"^#{1,3}\s+(.+?)\s*$", re.MULTILINE)

    BLOCKING_RULES: tuple[Rule, ...] = (
        Rule("host-path", re.compile(r"(?<![\w./-])(?:/home/\w+|~/|\$HOME\b|/mnt/\w+)"),
             "a path on this host: name the path inside the repository at a commit, or say the file is not in "
             "the repository", True),
        Rule("scratch-path",
             re.compile(r"\b(?:tmp_workspace|ai-worktrees|claude-issue-harness|manure-harness)\b|\bartifacts/manure-"),
             "the harness's scratch tree: cite docs/manure-plan/evidence/<packet>/ at the commit that holds it",
             True),
        Rule("host-address", re.compile(r"\b\d{1,3}(?:\.\d{1,3}){3}\b|\bssh://|\bDOCKER_HOST\b|\(host \S+\)"),
             "a host or an address: say what the host did ('the second GPU host'), never which", True),
        Rule("process-id", re.compile(r"\bpid \d+\b"),
             "a process id belongs in a comment's Harness trace block", True),
        Rule("gpu-slot",
             re.compile(r"\bslots? ?\d\b|\bresource\.lease|\bworv-iter(?:-\d+)?\b|\b(?:local|remote) "
                        r"(?:lane|card|slot|host)\b|\bwhole[- ]card\b|\bcard alone\b|\blease\b"),
             "GPU bookkeeping: state the measurement condition instead ('with no other process on the GPU')",
             True),
        Rule("harness-file",
             re.compile(r"\b(?:lease|merge-lock|claim|tick-pid|run_queue|slow_set|usage_ok|perf_control|"
                        r"nucleus_ok|tick|evidence_trim|worktree_gc|manure_mirror|remote-env)\.(?:sh|py)\b|"
                        r"\bstate/(?:locks|logs|paused)\b|\bbin/remote-docker\b"),
             "a harness file: name the repository command that does the same", True),
        Rule("tick-aside",
             re.compile(r"\(mine\)|\bbudget \d+ ?min\b|\bharness tick\b|\bresume recipe\b|"
                        r"\b(?:next|this|my|a later) tick\b|\bmerge (?:slot|lock)\b|\d{2}:\d{2}Z slot\b"),
             "a tick-to-tick aside: the reader is not the next tick", True),
        Rule("cubin-hash", re.compile(r"\bcubin [0-9a-f]{8,}\b"),
             "a kernel binary hash: name the kernel source commit instead", True),
        Rule("container-tmp", re.compile(r"/tmp/(?!kit\.log\b)"),
             "a path in a container or on this host", True),
        Rule("operator-stamp",
             re.compile(r"\b[Oo]perator(?:'s)?,? ?\(?\d{2}:\d{2}Z|\b\d{2}:\d{2}Z (?:ruling|sign-off|post|comment|"
                        r"answer)\b|\(\d{2}:\d{2}Z\)"),
             "a timestamp in place of a decision: write what was decided and link the comment", True),
    )
    WARNING_RULES: tuple[Rule, ...] = (
        Rule("shorthand-scenario", re.compile(r"\bRS[1-7]\b"),
             "define on first use, for example 'RS4, the scoop, carry and dump cycle' (prompts/glossary.md)",
             False),
        Rule("shorthand-class", re.compile(r"\bclass [AB]\b"),
             "say what the class means: A changes no engine behaviour, B does and needs a sign-off", False),
        Rule("shorthand-parent", re.compile(r"\bparent-[0-9a-f]{7,}\b"),
             "name the base commit and say it is the unchanged tree the change is compared against", False),
        Rule("shorthand-suite", re.compile(r"\b(?:fast|slow|whole) set\b"),
             "name the command: tools/dev/iter.sh build --test [--fast] worv.env.manure", False),
        Rule("shorthand-section", re.compile(r"§\s?[A-Z]\.\d+|§\s?\d+[a-z]?\b"),
             "cite the document and the section's title", False),
        Rule("shorthand-rule", re.compile(r"\b(?:P|V|D|T)\d{1,2}[a-z]?\b|\bbar [123]\b"),
             "name the rule the first time: 'P1 (the engine names no body role)'", False),
        Rule("shorthand-run", re.compile(r"\blockstep\b|\bidentity (?:arm|proof|run)s?\b|\b(?:change|parent) arms?\b"),
             "say what the run is: a deterministic replay whose state documents must match byte for byte",
             False),
        Rule("label-mention", re.compile(r"\bai:(?:ready|wip|signoff|operator|plan|fable|long|group)\b"),
             "a harness label in prose: the labels show on the issue; say the state in words", False),
    )

    def __init__(self, repo: str | None, pr_sections: Sequence[str], issue_sections: Sequence[str]) -> None:
        self._repo: str | None = repo
        self._pr_sections: tuple[str, ...] = tuple(pr_sections)
        self._issue_sections: tuple[str, ...] = tuple(issue_sections)

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
        seen_warnings: set[tuple[str, str]] = set()
        for line_index, line in enumerate(lines):
            protocol_line: bool = kind == "comment" and line_index == 0 and bool(self.PROTOCOL_LINE.match(line))
            for rule in self.BLOCKING_RULES:
                if protocol_line and rule.rule_id in self.PROTOCOL_EXEMPT_RULES:
                    continue
                for match in rule.pattern.finditer(line):
                    findings.append(Finding(rule.rule_id, line_index + 1, match.group(0), rule.hint, True))
            for rule in self.WARNING_RULES:
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
            "Packet: `~/tmp_workspace/artifacts/manure-plan/evidence/2026-11-03-x/INDEX.md` on 10.50.2.178.",
            "Lease: slot 1 (`worv-iter`), pid 2421513, budget 120 min, cubin b034f353dfa4, `lease.sh acquire`.",
            "Operator (05:31Z) accepted it; land in the 06:00Z slot; log at /tmp/mg/run.log. Then `ai:ready` (mine).",
        ])
        good_pr: str = "\n".join([
            "## Summary", "The return walk keeps a candidate's borne verdict while its inputs repeat. Fixes #2137.",
            "## Why", "Each exchange read every column again; 57 ms of an 84 ms stage on RS6, the pushed-crate scenario.",
            "## What changed", "- `ManureZone::_markBorneByStillSteel` keeps the verdict per row.",
            "## How to verify", "```", "tools/dev/iter.sh build --test --fast worv.env.manure", "```",
            "## Evidence", "Packet `docs/manure-plan/evidence/2026-11-03-2137-walk-hold-at-rest/` at 249643293.",
            "## Scope and limits", "Compile-verified on the second GPU host only.",
        ])
        good_comment: str = "\n".join([
            "harness tick 2026-11-03T04:30:40Z pid 2421513 claiming, budget 120 min",
            "State: the walk hold is built; the identity runs are queued.",
            "<details><summary>Harness trace</summary>", "",
            "host khemoo, slot 1, worktree /home/khemoo/tmp_workspace/ai-worktrees/mpm-2137, lease.sh acquire 2137",
            "</details>",
        ])
        passed: bool = True

        def check(name: str, condition: bool, detail: str) -> None:
            nonlocal passed
            passed = passed and condition
            print(f"{'PASS' if condition else 'FAIL'} {name}" + ("" if condition else f" :: {detail}"))

        bad_findings: tuple[Finding, ...] = self.lint(bad_body, "pr")
        fired: set[str] = {finding.rule_id for finding in bad_findings if finding.blocking}
        for rule in self.BLOCKING_RULES:
            check(f"{rule.rule_id} fires on the bad body", rule.rule_id in fired, self.format(bad_findings, "pr"))
        check("a body without the sections is blocked", "section" in fired, self.format(bad_findings, "pr"))
        good_findings: tuple[Finding, ...] = self.lint(good_pr, "pr")
        check("a PR body written to the contract has no blocking finding",
              not any(finding.blocking for finding in good_findings), self.format(good_findings, "pr"))
        check("the PR body's shorthand is warned, not blocked",
              any(finding.rule_id == "shorthand-scenario" for finding in good_findings), self.format(good_findings, "pr"))
        comment_findings: tuple[Finding, ...] = self.lint(good_comment, "comment")
        check("a comment's protocol line and trace block are exempt",
              not any(finding.blocking for finding in comment_findings), self.format(comment_findings, "comment"))
        trace_in_body: tuple[Finding, ...] = self.lint(good_pr + "\n<details><summary>Harness trace</summary>x</details>",
                                                      "pr")
        check("a trace block in a body is blocked", any(finding.rule_id == "trace-in-body" for finding in trace_in_body),
              self.format(trace_in_body, "pr"))
        return passed

    def _missing_sections(self, text: str, required: Sequence[str]) -> list[Finding]:
        present: list[str] = [heading.lower() for heading in self.HEADING.findall(text)]
        missing: list[Finding] = []
        for name in required:
            if not any(heading.startswith(name.lower()) for heading in present):
                missing.append(Finding("section", 0, f"## {name}", "a required section is missing", True))
        return missing

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
        lint: PublicRecordLint = PublicRecordLint(args.repo, pr_sections, issue_sections)
        kind: str
        body: str
        try:
            if args.self_test:
                return 0 if lint.self_test() else 1
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
