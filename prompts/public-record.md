# The public record — what a tick writes to GitHub

Everything a tick posts to GitHub is read by people who have the repository and nothing else: no
dev host, no harness checkout, no scratch store, no memory of the last tick. An issue body, a PR
body, a comment, a close summary and a commit message have to stand on their own there, today and
in a year. This file is the contract. `"$HARNESS_BIN" lint` checks the mechanical half of it,
and a tick runs the lint on every body before `gh` posts it: a blocking finding means rewrite, not
post. The project's terms are in the installation's `prompts/glossary.md`; define each one the first
time a body uses it, with the glossary's phrase. The installation's `prompts/public-record.md`, when
it exists, adds the project's own rules to these, and its `lint_rules.json` adds them to the lint.

## The one test

Could an engineer who clones the repository at the commit you name, and has never seen this
machine, understand what changed and why, repeat the verification, and judge the evidence, from the
body alone? If a sentence is only true on this host, it does not belong in the body.

## What never appears in a body

| Never | Because | Write instead |
|---|---|---|
| a path on this host: `/home/…`, `~/…`, `/tmp/…`, a worktree | nobody else can open it | the path inside the repository at a named commit (`docs/evidence/1234/INDEX.md` at `249643293`), or "not in the repository" and what it holds |
| a host name, an IP address, `DOCKER_HOST`, `ssh://…` | infrastructure, not the change | what the host did: "the build host", "the second test host" |
| a process id, a device slot, a lane, a container name, a lock, a merge slot | the harness's bookkeeping | the condition it implies, if it matters: "with no other process on the machine" |
| a harness file: `claim.sh`, `merge-lock.sh`, `state/…` | not in the repository | the repository command that does the same: `make test` |
| a worktree name, a tick budget, "(mine)", "the next tick" | tick-to-tick asides | nothing; the labels carry the harness state |
| an operator timestamp in place of the decision ("operator 05:31Z") | the reader cannot open the thread by time | the decision in one sentence, linking the comment |
| a comparison of two readings taken in different sessions | the host drifts between sessions | both readings in one session, or a single reading named as such |

Where those details are still needed (the launcher reads the claim, a successor tick resumes from
the recipe), they go in a **comment**, at its end, under a collapsed block, and nowhere else:

```
<details><summary>Harness trace</summary>

harness tick 2026-11-03T04:30:40Z pid 2421513 claiming, budget 120 min
host <harness-host>, worktree <worktree-root>/app-1234 at 249643293,
resume: run the new case on the fix, then the whole suite
</details>
```

The lint skips that block in a comment and blocks it in an issue or PR body. A `harness tick` or
`pid N` line anywhere else in a comment is a blocking finding. Inside a closed fenced code block
the lint lets quoted output keep the harness's words (tick asides and the installation's prose
rules), but a host path, an address or a pid blocks there as anywhere else.

## A PR body

The title is one sentence that says what the change does, conventional-commit style, no
parentheses. The body has these sections, in this order, with these headings, so a reader finds the
same thing in the same place every time:

```
## Summary
Two to four sentences in plain words: what the change does, whether it changes behaviour, and the
version it lands as. Fixes #<issue>.

## Why
The defect or the ask, as the issue stated it, with the number that made it worth doing
("each request parsed the whole config again: 57 ms of an 84 ms handler").

## What changed
Bullets by file or concept. Name the interface, the constant, the module. Say what did not change
where a reader would expect it to ("no public signature changes").

## How to verify
The commands, from the repository root at the PR's head, and what their output should read: the new
test, the suite, the run. A reader with the toolchain follows these; one without it knows what was run.

## Evidence
The decisive lines, quoted, with the commit each reading was taken at and the condition it was
taken under. Tables for numbers: one row per reading, units in the header, the condition in a
column. Media that stays out of git is named as kept outside the repository.

## Scope and limits
What was not verified and why ("compile-verified only: no test host was free"), what the change
leaves open, and any reading that is a recorded value and not a bar.
```

A PR that only changes docs or tests still carries every section; the ones that do not apply say
so in one line. An installation that requires more sections adds them to `PUBLIC_RECORD_PR_SECTIONS`.

## An issue body

A defect:

```
## Summary
One paragraph: what is wrong, where, how big, and what the fix is expected to be.

## Observed
The reading, with its commit, input and condition. Quote the lines.

## Expected
The rule or the reference value, cited.

## Evidence
The test case that shows it, or the repository path of what does.

## Ask
The steps, each one a thing a PR can do: the failing case first, then the fix, then the proof.

Depends on: #<n>, #<m>
```

A planned step uses `## Context` in place of Observed and Expected (which plan item, which decision,
what is known), and `## Acceptance` after the Ask (what the evidence must show). The `Depends on:`
line keeps that exact form: the launcher reads it.

An issue a script files (a red suite) says what is red, at which commit, the command that reproduces
it, the merges since the last green, and how to bisect, in repository terms. The script's own log
path goes under a Harness trace block in a comment, never in the body.

The lint requires `## Summary` and `## Ask`, the two sections every issue above carries; the
others depend on the kind of issue.

## A comment

What a reader sees is the state in plain words, as a change log a reviewer can follow: what is
done, what is running, what is owed, each with its commit. A claim says what is being started
("Claimed. Starting with the failing case for the parser's five keys."); a pause says what a
successor must do, in repository terms. GitHub shows the time and the author, so the text repeats
neither.

The protocol the launcher parses is one line **inside the Harness trace block** at the end, with
the host, worktree and recipe beside it; the launcher finds it anywhere in the comment:

```
harness tick <UTC timestamp> pid <PID> claiming|resuming, budget <n> min
harness tick pid <PID> checkpoint <n> at <UTC timestamp>
harness tick pid <PID> pausing|withdrawing|releasing
```

Never in the open. "harness tick 2026-11-03T09:06:50Z pid 205984 claiming (host <harness-host>), budget
120 min" as a comment's visible text is the record written for one machine, and the lint blocks it.

A landing comment names the merge commit, the version, and the suites run on the merge result.
A close summary is one or two sentences: what landed, as which commit and version, and where the
evidence is.

## Writing

- One idea per sentence. Plain words over the harness's: "the unchanged tree" rather than "the
  base arm", or the term and its meaning together, the first time.
- Every number has a unit, a commit and a condition, and lives in a table when there are more than
  two. A percentile says over how many samples.
- Quote the decisive output line, not the whole log. A claim with no quoted line is a claim.
- Say what was not done. "Not run" is a finding; silence is not.
- Link an issue, PR or comment by number or URL, never by time.
- No AI attribution anywhere: no `Co-Authored-By`, no session link, no "Generated with".

## Before and after

Before:

> Results in `~/work/runs/1234/INDEX.md`. Measured on slot 1 (pid 2421513, mine), head 4f04f3f1f
> against 8f4e54b3d: handler p50 84 → 27 ms. Next tick runs the full suite.

After:

> ## Evidence
> Both trees were measured in one session with no other process on the machine, run alternately.
>
> | Reading, 1000 requests against the sample config | head 4f04f3f1f | this change 8f4e54b3d |
> |---|---|---|
> | handler time, p50 over 1000 requests (ms) | 84.0 | 27.0 |
