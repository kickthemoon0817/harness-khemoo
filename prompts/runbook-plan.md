# Runbook — plan execution (mission 5, from 2026-09-23)

The harness implements the owner-approved manure plan. The plan binds over everything else,
including `runbook.md` and `design.md` where they conflict.

## What binds, in this order

1. **`PRINCIPLE.md`** of the plan — engine rules P1–P10, validation rules V1–V5 and V2a.
2. **`DECISIONS.md` §1** — the owner's answers of 2026-09-23 (D1–D14).
3. **The item document** the issue names (`todo/Tnn-*.md`, `STRATEGY.md` probes), and the
   investigations it cites.
4. The repository's `CLAUDE.md` hard rules.
5. `runbook.md` — for the **mechanics only**, as listed below.

The plan's working copy is `/home/khemoo/tmp_workspace/artifacts/manure-plan/`. The same text is in the
repository under `docs/manure-plan/` on `ai/manure-mpm` (merged by #1318). Read it there.

## Which issue

- Only issues labelled **`ai:plan`** are claimable. An `ai` issue without `ai:plan` is not worked.
- An `ai:plan` issue whose body starts with `Operator: **take this before any other claimable issue.**`
  goes first; then resume a released claim (dead claimant); then the oldest `ai:plan` issue whose
  `Depends on:` issues are all closed.
- An issue labelled `ai:signoff` is waiting for the owner. Never claim it; never merge its PR.

## Read before you touch code

`PRINCIPLE.md`, `DECISIONS.md` §1, `TODO.md`, the item document the issue names, and for code that
is replaced or extended, the plan's line references (read the code at `origin/ai/manure-mpm` with
`git show` — never the operator's working checkout).

## Mechanics taken from runbook.md (unchanged)

Tick procedure steps 1–3 (setup under the stack lock, the claim with `tick-pid.sh`, the pause
comment, worktree reuse, build commands, AOT traps, sharded doctests, `--fast`, `-tc=`/`-sf=`), the
kit-run mechanics of step 4 (nucleus preflight, `iter.sh up/kit/topics`, the `lease.sh` export line,
domains, known traps), step 5 (submit), the GPU-slots section, and the Rails section except where
overridden below.

## Overrides (these replace the mission-4 rules they name)

1. **No gate verdict is acceptance (V1, V3).** `manure_gate.sh`'s PASS/FAIL line is a diagnostic.
   Acceptance is the evidence packet (V2) and, for behaviour changes, the owner.
2. **Evidence packet for every PR** under
   `/home/khemoo/tmp_workspace/artifacts/manure-plan/evidence/<YYYY-MM-DD>-<issue#>-<slug>/` with an
   `INDEX.md` naming every file (run, commit, scenario, camera, moment, what it shows). Never
   overwrite an evidence folder; a re-run gets a new one (V5.6). It holds, as the step needs:
   the fail-on-head fixture output (failing on head, passing on the fix), reference cases with
   expected values and sources, whole-window traces, frames, the setup check, the tick-cost reading.
   Until the T01 tools land (#1320–#1324 closed), use the existing gate frames and `state-series.jsonl`;
   after, use the render tool, the trace tool, the scenario runner and the packet template.
3. **Capture runs and the card.** Three render products at 1280×720 do not fit beside two sibling
   kits on the 16 GB card: captures come back empty and a kit can die of it (#1320). A capture run
   arms one camera at a time, or acquires every GPU slot so it holds the card alone; the packet says
   which. A render product is never read on the update it was armed on.
4. **Visual check by independent eyes (V5).** For every behaviour change (merge class B), spawn a
   subagent (the Agent tool) that did not write the change, give it ONLY the frames and the checklist
   (T01's V-a…V-i; T14's R-a…R-j where they apply) — never your description of what they should show —
   and save its `visual-check.md` in the packet. A *fails* or *cannot tell* blocks until explained.
5. **Merge rule by class** (the issue states the class):
   - **Class A** — no engine behaviour change (tools, tests, docs, investigations, reporting that
     changes no motion, or a refactor proven by byte-identical traces under lockstep): the tick may
     merge its own PR into `ai/manure-mpm` once the build, the fail-on-head fixture and every case in
     every touched test file pass, and the packet is written. Then close the issue.
   - **Class B** — a behaviour change: open the PR with the packet linked in `## Verification`, add
     `ai:signoff` to the PR and the issue, post `harness tick pid <PID> pausing: awaiting owner
     sign-off, packet <path>`, and END. Never merge a class B PR. The owner merges and closes.
   - If a class A issue turns out to change behaviour, treat it as class B and say why.
   - **Class B after the owner's sign-off.** When the issue carries a comment starting
     `Owner sign-off: approved`, a tick may land the PR: merge `origin/ai/manure-mpm` into the branch,
     move the extension version past the branch's if it was taken (one version, one CHANGELOG section),
     rebuild, re-AOT the cubin if kernels or the device solver moved, run every case of the touched
     test files on the merge result, push, merge the PR, remove `ai:signoff` and `ai:wip`, and close the
     issue with the merge commit. If the merge needs more than version and CHANGELOG resolution —
     a conflict in engine code the owner did not see — stop and post a `Plan finding:` instead.
6. **Principle checks in every PR body**: P1 (the diff names no body role — grep the diff for
   bucket, wheel, cutting_edge, carried, parked, tool box; explain any hit), P4 (ledger drift over the
   run), P9 (every new constant with its class: MATERIAL, DERIVED, SHARED RULE, SETTING — a bare
   number in a law is a defect), P10 (which interface the change sits behind, once A0 exists).
7. **Tick cost (P8, owner decision D4).** Correctness first: a change may slow the tick, but a PR
   that changes runtime code reports its tick-wall cost against the parent (TickWallSplit or the
   profiler, same session, both arms). No perf bar blocks a merge; T09 restores real time.
8. **Void mission-4 rules:** "PHYSICS FIRST" ordering, "LIVE VERDICT SIZE", the perf-drift and
   perf-control rules, "merge on fixture + live verdict", and `design.md`'s plan of record rev 5.
   Priority is the plan's order, encoded in `Depends on:`.
9. **Plan documents are the operator's.** Never edit `PRINCIPLE.md`, `TODO.md`, `DECISIONS.md` or
   an item document. Write only what the issue asks (for probes and investigations: the named file
   under `investigations/` in the plan folder) and evidence folders. A finding that would change the
   plan goes in an issue comment starting `Plan finding:` for the operator to fold in.
10. **Names.** Review scenarios are RS1–RS7; tasks are S1–S3 (owner decision D9).
11. **PhysX only** (owner decision D10): no Newton backend work. Newton appears only in Probe B,
    standalone, outside Kit.
12. **Never write to, reconfigure or restart** the Nucleus server 10.50.2.21, and never change an
    asset on it. Reading is what every run does, so a read-only stage open for evidence is fine.
    **Robot asset tests run on copies** in `omniverse://10.50.1.117/Users/sungminkim/` (owner,
    2026-09-24): copy the USD from 10.50.2.21, edit and load only the copy, and never commit a catalog
    URL that points into that directory. Promoting a tested asset is the owner's job.
    **Never touch** the owner's own Isaac Sim or GUI sessions, the
    operator's working checkout, `master`, or a PR you did not open (runbook.md's resume exception
    stands). No AI attribution in commits or PR bodies; conventional commits without parentheses;
    never force-push.

13. **A run counts only once its boot is proven.** Before quoting any number from a kit run, check its
    `kit.log`:
    - it must contain `scene bootstrap complete`;
    - it must contain neither `bootstrap FAILED` nor `lockstep invalidated`;
    - it must show the robot actually moving.
    **A kit that mounts the branch's `worv.robots.catalog` must also mount the branch's
    `worv.robots.locomotion` and `worv.robots.attachments` builds.** The baked image's locomotion rejects
    `[drive].wheel_odom_hz`, and the robot never boots. That is how #1367's first traces were all invalid.
    A run that fails this check is named as failed in the packet, never used.

## Done

When no `ai:plan` issue is claimable, print a one-line summary and end.
