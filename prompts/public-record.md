# The public record — what a tick writes to GitHub

Everything a tick posts to GitHub is read by people who have the repository and nothing else: no
dev host, no harness checkout, no evidence store, no memory of the last tick. An issue body, a PR
body, a comment, a close summary and a commit message have to stand on their own there, today and
in a year. This file is the contract. `bin/public_record_lint.py` checks the mechanical half of it,
and a tick runs the lint on every body before `gh` posts it: a blocking finding means rewrite, not
post. The project's terms are in `prompts/glossary.md`; define each one the first time a body uses
it, with the glossary's phrase.

## The one test

Could an engineer who clones the repository at the commit you name, and has never seen this
machine, understand what changed and why, repeat the verification, and judge the evidence, from the
body alone? If a sentence is only true on this host, it does not belong in the body.

## What never appears in a body

| Never | Because | Write instead |
|---|---|---|
| a path on this host: `/home/…`, `~/…`, `tmp_workspace/…`, `ai-worktrees/…`, `artifacts/manure-plan/evidence/…` | nobody else can open it | the path inside the repository at a named commit (`docs/manure-plan/evidence/<packet>/INDEX.md` at `249643293`), or "not in the repository" and what it holds |
| a host name, an IP address, `DOCKER_HOST`, `ssh://…` | infrastructure, not the change | what the host did: "the build host", "the second GPU host" |
| a process id, a GPU slot, a lease, a lane, a container name, a lock, a merge slot | the harness's bookkeeping | the condition it implies, if it matters: "with no other process on the GPU", "two scenario runs shared the GPU" |
| a harness file: `lease.sh`, `run_queue.sh`, `slow_set.sh`, `state/…` | not in the repository | the repository command that does the same: `tools/dev/iter.sh build --test --fast worv.env.manure` |
| a cubin hash, a worktree name, a tick budget, "(mine)", "the next tick" | tick-to-tick asides | nothing; the labels carry the harness state |
| an operator timestamp in place of the decision ("operator 05:31Z") | the reader cannot open the thread by time | the decision in one sentence, linking the comment |
| a comparison of two readings taken in different sessions | the host drifts between sessions | both arms in one session, or a single reading named as such |

Where those details are still needed (a successor tick resumes from them), they go in a **comment**,
at its end, under a collapsed block, and nowhere else:

```
<details><summary>Harness trace</summary>

host khemoo, slot 2, worktree ai-worktrees/mpm-2137 at 249643293, lease 2137 held 04:31Z–05:20Z,
resume: build, then RS1–RS6 identity against parent-8cc9dd2e8
</details>
```

The lint skips that block in a comment and blocks it in an issue or PR body.

## A PR body

The title is one sentence that says what the change does, conventional-commit style, no
parentheses. The body has these sections, in this order, with these headings, so a reader finds the
same thing in the same place every time:

```
## Summary
Two to four sentences in plain words: what the change does for the engine, whether it changes
behaviour (class A: no; class B: yes, and who signs it off), and the version it lands as.
Fixes #<issue>.

## Why
The defect or the ask, as the issue stated it, with the number that made it worth doing
(“each exchange read every column again: 57 ms of an 84 ms stage on RS6, the pushed-crate scenario”).

## What changed
Bullets by file or concept. Name the law, the interface, the constant class. Say what did not
change where a reader would expect it to (“no conservation path changes”).

## How to verify
The commands, from the repository root at the PR's head, and what their output should read:
the fixture (`tools/test/run_doctest_sharded.sh --file '*test_x.cpp' <binary>`), the suite, the
scenario run. A reader with the image and a GPU follows these; one without them knows what was run.

## Evidence
The decisive lines, quoted, with the commit each reading was taken at and the condition it was
taken under. Tables for numbers: one row per reading, units in the header, the condition in a
column. The packet by its repository path, with its INDEX.md committed in this PR; media that stays
out of git is named in the INDEX.md as kept outside the repository.

## Principle checks
What the plan requires of every PR (P1, P4, P9, P10 for the manure plan), each named the first time.

## Scope and limits
What was not verified and why (“compile-verified only: no GPU held”), what the change leaves open,
and any reading that is a recorded value and not a bar.
```

A PR that only changes docs or tests still carries every section; the ones that do not apply say
so in one line.

## An issue body

A defect:

```
## Summary
One paragraph: what is wrong, where, how big, and what the fix is expected to be.

## Observed
The reading, with its commit, scenario, seed and condition. Quote the lines.

## Expected
The law or the reference value, cited.

## Evidence
The packet by repository path, or the test case that shows it.

## Ask
The steps, each one a thing a PR can do: the failing case first, then the fix, then the proof.

Depends on: #<n>, #<m>
Class: A | B
```

A plan step uses `## Context` in place of Observed and Expected (which plan item, which decision,
what is known), and `## Acceptance` after the Ask (what the packet must show). The `Depends on:`
line keeps that exact form: the launcher reads it.

An issue a script files (a red suite) says what is red, at which commit, the command that reproduces
it, the merges since the last green, and how to bisect, in repository terms. The script's own log
path goes under a Harness trace block in a comment, never in the body.

## A comment

The first line is the protocol the launcher parses, and nothing else is on it:

```
harness tick <UTC timestamp> pid <PID> claiming|resuming, budget <n> min
harness tick pid <PID> checkpoint <n> at <UTC timestamp>
harness tick pid <PID> pausing|withdrawing|releasing: <one line>
```

Then the state in plain words, as a change log a reviewer can follow: what is done, what is
running, what is owed, each with its commit. A pause names what a successor must do in repository
terms. The host, slot, worktree and recipe details go in the Harness trace block at the end.

A landing comment names the merge commit, the version, and the suites run on the merge result.
A close summary is one or two sentences: what landed, as which commit and version, and where the
evidence is.

## Writing

- One idea per sentence. Plain words over the harness's: “the unchanged tree” rather than
  “the parent arm”, “a deterministic replay” rather than “lockstep”, “the scoop, carry and dump
  cycle” rather than “RS4” — or the term and its meaning together, the first time.
- Every number has a unit, a commit and a condition, and lives in a table when there are more than
  two. A percentile says over how many samples.
- Quote the decisive output line, not the whole log. A claim with no quoted line is a claim.
- Say what was not done. “Not run” is a finding; silence is not.
- Link an issue, PR or comment by number or URL, never by time.
- No AI attribution anywhere: no `Co-Authored-By`, no session link, no “Generated with”.

## Before and after

Before, from a merged PR body:

> Packet: `artifacts/manure-plan/evidence/2026-11-03-2137-walk-hold-at-rest/INDEX.md`.
> **The gain, card alone** (packet §8; `--all --render`, local slot 1, alternated
> `h-RS6 l-RS6 l-RS3 h-RS3`, head 4f04f3f1f against 8f4e54b3d, one cubin): RS6 at rest, both arms
> in one host mode (`account` 1.02–1.03 ms both): interval p50 311.1 → 253.9 ms.

After:

> ## Evidence
> Packet `docs/manure-plan/evidence/2026-11-03-2137-walk-hold-at-rest/INDEX.md`, committed in this
> PR. The cost reading below was taken in one session with no other process on the GPU, the two
> trees run alternately so that neither took the host's faster mode alone.
>
> | Reading, RS6 (the pushed-crate scenario) at rest | head 4f04f3f1f | this change 8f4e54b3d |
> |---|---|---|
> | engine update interval, p50 over the last two windows (ms) | 311.1 | 253.9 |
