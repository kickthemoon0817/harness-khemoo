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
- An issue labelled `ai:signoff` is waiting for a sign-off. Never claim it; never merge its PR. The owner
  waived class B sign-off on 2026-09-26, so the operator reviews the packet and signs off in the owner's place.
- An issue labelled `ai:ready` waits for a merge slot (override 3c). While a slot is open, resume it before
  any other issue; outside a slot, never claim it.
- An issue labelled `ai:fable` is difficult or important, and it is worked on Fable 5.1 (the fable lane). The first line
  of your prompt says which lane this tick is on:
  - a fable-lane tick claims only `ai:fable` issues;
  - any other tick never claims one, unless its first line says Fable is cooling down on its usage limit.
  `ai:fable` is the operator's to set, like `ai:long`: a tick never adds or removes it.
- An issue labelled `ai:operator` is waiting for the operator. Never claim it. A pause that cannot go on
  until the operator answers (a `Plan finding:` the step depends on) adds `ai:operator` in the same command
  as the pause comment. A pause a later tick can resume by itself (no slot, the budget, owed runs) never
  adds it. The operator removes it after answering.

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
   **Fixtures check physics.** A behaviour change's fail-on-head fixture checks the law against a cited
   reference value wherever the step names one (T05 and T06 list them, for example FC1, FC3, EX1, EX4 and
   SL2). A pinned document, string, counter or hash is a guard, never the fixture. A test that pins a known
   wrong result (a negative convergence order, an out-of-band ratio) reports the value, not a pass.
3. **Capture runs and the card.** Three render products at 1280×720 do not fit beside two sibling
   kits on the 16 GB card: captures come back empty and a kit can die of it (#1320). A capture run
   arms one camera at a time. Even then a sibling kit of about 7 GB beside a capture kit and the
   display can run the card out of memory (#1325).
   - **At most two kits share the card** (`GPU_KITS_MAX`, 2). A kit that renders the robot's cameras needs
     5–7 GB of the 16 GB card, and a third kit dies of GPU out-of-memory at boot. A third single-slot
     acquire prints `BUSY kit limit` while a slot file is free: treat it as busy.
   - **Waits are served oldest first, across both lines.** A whole-card wait takes priority only while no
     single-slot issue in line has waited longer.
   - **Single-slot waits form a line.** A refused single-slot acquire records its wait. Once an issue has
     waited 20 minutes (across the ticks that resume it), the next slot that frees goes to it, and other
     ticks read `BUSY queued behind single-slot issue <n>`: treat that as busy. Keep polling in the
     foreground, at least every 60 seconds: a tick that stops asking for 2 minutes leaves the line, and a
     whole-card priority lapses the same way, so do host work before or after the wait, not during it.
   - **`runbook.md`'s "never wait more than 20 min" for a slot does not apply.** Do the step's host work
     first, then poll for as long as the owed runs still fit before the stop line, then pause with the
     resume recipe. The wait keeps its place in the line across the ticks that resume the issue.
   - **Device doctests take no kit slot.** For `iter.sh build --test --device`, run
     `lease.sh acquire <issue> --device` and use its export line. It admits one device run beside the kits
     when the card has 3 GiB to spare, and never during a whole-card run. Release it right after the tests.
     A whole-card acquire waits while a device holder lives, so the lane yields to a whole-card issue
     with priority: a new device run gets `BUSY device: whole-card issue N has priority`, and a holder
     asking again gets `YIELDED device` and loses its lease. Poll as for a slot, and do host work
     meanwhile. A sweep of many binaries asks once per binary, never holding the lane across the sweep.
   - **Only a run that renders needs the whole card.** Host measurement runs render nothing and take one
     slot.
   - **`--no-moments` identity runs take one slot.** Since #1419 (257d59520) they drop the scenario's C1
     video as well as the moments, so a kit holds about 5.1 GB and two run side by side. Take a plain
     `lease.sh acquire <issue>` on a tree that includes 257d59520. Parent and change trees both have it once
     you merge the head. Only a baseline or a perf A/B reading takes `--all --render`.
   - **A class B render session takes one slot.** Change against parent, lockstep with moments, frames for
     V5: take a plain `lease.sh acquire <issue>` and run its arms on that slot, like any other kit run.
     - **The frames are the same whatever shares the card.** Moments are step-locked (#1386).
     - **Memory fits.** A rendering kit (5–7 GB) fits beside an identity kit (5.1 GB) or another rendering
       kit (two at 11.6 GB) under the two-kit cap. The out-of-memory kill was a third kit.
     - **Tick cost** read in such a session is marked `shared card` in the packet. It is a recorded reading,
       not a bar (D4), and T09 measures real time.
   - **A baseline, or a perf A/B reading, holds the card alone.** It uses
     `lease.sh acquire <issue> --all --render`. Without `--render` the call is refused: a run that renders
     nothing takes one slot. Each call reserves every free slot for the tick and prints
     `RESERVING held=n/N` until the last sibling releases, then prints `ACQUIRED ... exclusive=all`
     with the export line.
   - **A reservation lapses after 10 minutes** (`ALL_RESERVE_TTL_S`). A single-slot tick may then take
     the slot, so a long run on another slot never leaves reserved slots idle. Once `ACQUIRED`, the
     other slots are held and never lapse.
   - **After 30 minutes of waiting, the issue gets priority** (`ALL_PRIORITY_AFTER_S`). The wait counts
     across the ticks that resume the issue. The oldest such wait wins: its reservations stop lapsing,
     single-slot acquires print `BUSY priority to whole-card issue N`, and other whole-card waiters
     print `YIELDING` and drop their reservations. Treat both as busy: keep doing the work that needs no
     card, and retry.
   - **While it waits, it does the work that needs no card.** It retries the acquire; each retry
     keeps what it has reserved.
   - **It releases as soon as the capture ends.** The packet says which mode the run used.
   - **Between kit runs, re-run `lease.sh acquire <issue>` on the slot you hold.** It hands the slot back
     unless a whole-card issue has priority. Then it prints `YIELDED` (exit 1): the lease has released your
     slot, and you have joined the single-slot line. Treat that as busy:
     - run `tools/dev/iter.sh down` for your container at once, under your export line;
     - then poll again.
     Re-acquire only between runs, never during one. A queue that held a slot for hours starved every capture
     while an advisory yield was ignored.
   - **Poll the lease from the tick's own shell,** in a bounded foreground loop. `lease.sh` records the
     tick's PID by walking up to it. A detached background loop has no tick above it, so `lease.sh`
     refuses it (exit 2). A lease recorded under the loop's own PID would read as dead the moment the
     loop ended, and a sibling would take the card mid-capture. A script that holds the lease for its
     whole run sets `LEASE_HOLDER_PID=$$`. Never edit `state/locks/`, including the `all-wait.*` records.
   - **A tick that holds the whole card for a baseline** runs only the baseline's arms under it, and
     releases before any other arm. A series of arms run under `--all` keeps every sibling off the card for
     the whole series.
   - A render product is never read on the update it was armed on.
3a. **Parent arms are shared per base commit.** Every refactor proving §F.3 against the same base runs the
   same parent arms (lockstep, `--no-moments`, same seed), so run them once and share them:
   - **Where they live.** `evidence/parent-<base9>/<scenario>-p/`, with the parent's build tree
     `ai-worktrees/parent-<base9>` and a `status` file in the same format as the arm queues. `<base9>` is
     the base's first nine hex digits: the head you merged before the proof.
   - **Reuse a finished one.** Before running a parent arm, look there. If `<scenario>-p rc=0 end`
     is in its `status` and the run passed override 13's boot check, compare your change arm against
     it and cite it in your packet.
   - **Otherwise claim and run it there.** `mkdir evidence/parent-<base9>/<scenario>-p` claims the arm:
     only the tick whose `mkdir` succeeds runs it, into that folder, not into its own packet. If the
     folder exists without a finished status, another tick is running it: run your change arms
     meanwhile.
   - **Check the base first.** Each parent arm runs twice (`<scenario>-p` and `<scenario>-p2`), and a proof
     uses a base only once each scenario's two parent arms are identical. If a pair parts, the base does not
     replay itself (§F.3's precondition). Post a `Plan finding:` before proving against it, as with #1436. A
     single parent arm per base let #1411's proof pass by chance while RS2 had begun to split.
   - **Every scenario of the set.** A §F.3 proof covers RS1–RS6. RS4 joined on bases from 8ec107ad3 (#1417,
     D16a), and runs under `lockstep_sync --no-moments` like the rest. A missing parent arm is a reason to claim and run it, never a reason to leave its scenario
     out. A class A merge with a scenario left out is not proven: pause instead, and say which arms are owed.
   - **One cubin.** Your change tree runs the parent tree's cubin (§F.3) while the kernel sources are
     unchanged.
   - **A pair that parts** is re-run with the parent arm in your own session, beside your change arm,
     before it counts as a finding (R32: timing, load).
3b. **Fetch before every arm, not only at the claim.** When `origin/ai/manure-mpm` has moved in code that
   runs in an identity arm, stop arming the old base. The arms already recorded stay as evidence. Merge the
   head, rebuild, and arm the new base's parents and your change there, because a proof on a base the slot
   no longer has cannot land.
   **A proof carries across a base move, scenario by scenario, where the chain check allows it.**
   - **The check.** Compare the new base's `<scenario>-p` against the old base's `-p` (`chain.py`, as 3c's batch check does).
   - **The condition.** They may part only in state keys the landings in between added or changed, with the particle
     hashes and the wrench trace identical, and the base move's diff shares no file with your change.
   - **Then** your change arm on the old base stands for that scenario on the new one. Merge the head, rebuild, run the
     touched test files, and cite the chain line and the keys set aside in the packet.
   - **Otherwise** re-run that scenario on the new base.
   - **Why.** Each base needs its `-p` arms anyway, so a class B landing that moves no physics no longer sends every
     class A proof back to the start.
   **When the base moves under a finished §F.3 proof,** merge it. Re-run the proof only if the move changes
   code that runs in an identity arm and can change §F.3's identity set: state documents, wrench trace or
   particle hashes. A move confined to any of these needs no re-run:
   - docs, tests, and tools that never run inside an identity arm (analysis and comparison scripts);
   - the replay trace's own hashing, which is outside the identity set;
   - extensions the identity kits do not mount.
   The scenario runner, the render companion and the scenario files run inside every arm and decide when
   things happen: the build hold (#1431), the props (#1437), the spec timing (#1449). A move there changes
   the arms, so re-run the proof on parents recorded with it.
   List the moved files in the packet and say why each cannot change the set. When in doubt, re-run.
3c. **A merge that moves the identity arms waits for a merge slot (owner, 2026-09-25).** Proofs then run
   against one base per window, and a landing no longer sends every other proof back to re-run.
   - **Slots.** A slot opens every three hours on the UTC clock (00:00, 03:00, …, 21:00) and lasts
     60 minutes. The window base is `origin/ai/manure-mpm` when the slot opens.
   - **Which PRs wait.** A class A PR that changes code running in an identity arm: an extension the
     identity kits mount, the scenario runner, the render companion or a scenario file. Docs, tests,
     analysis tools, unmounted extensions and investigations merge when ready, as before; 3b exempts them.
   - **Ready.** When the proof holds against the current head and the packet is written:
     - add `ai:ready` to the issue and the PR;
     - post `harness tick pid <PID> pausing: ready for the merge slot, proven on <base9>, packet <path>`;
     - END without merging.
   - **Landing in a slot.** `claimable.sh` offers `ai:ready` issues only while a slot is open. The tick that
     takes one runs these steps in order:
     1. `bin/merge-lock.sh acquire <issue>`, polled in the foreground. It holds one landing at a time.
     2. Fetch and merge `origin/ai/manure-mpm` into the branch. Resolve only version and CHANGELOG
        conflicts: one version, one section, as in override 5. On any other conflict, release the lock,
        remove `ai:ready`, post a pause naming the files, and re-prove in the next window.
     3. Check what the merge brought: every commit between the proof base and the head must be a
        batch-mate that landed in this slot or a move 3b exempts. Otherwise, as after an owner-signed
        class B landing, release, remove `ai:ready` and re-prove on the new head.
     4. Rebuild and run every case of the touched test files on the merge result.
     5. Push, merge the PR, close the issue with the merge commit, then
        `bin/merge-lock.sh release <issue>`.
     A landing that starts inside the slot may finish after the slot closes.
   - **The batch is checked as a whole.** Each batch-mate was proven against the window base, not against
     the others.
     - **Who checks.** The first tick that records a parent arm on a head a slot produced also compares it
       with the same scenario's parent arm on the window base.
     - **Which arm.** The window base is the newest first-parent ancestor with that finished arm under
       `evidence/parent-<base9>/`.
     - **What to set aside.** Any key a batch-mate's packet says it added.
     - **Record.** Write the result per scenario to `evidence/parent-<base9>/chain.txt`.
     - **If the arms part:** post a `Plan finding:` naming the batch's PRs, and add `ai:operator`.
   - **Class B landings on a sign-off also land in a slot, after the slot's class A batch.**
     - A landing tick claimed outside a slot prepares its merge result (merge, rebuild, touched tests, and
       any evidence the sign-off made a landing condition), then adds `ai:ready` and pauses.
     - In a slot, it takes the merge lock only once no class A `ai:ready` issue is left to land, or 30 minutes
       into the slot. Before merging, it merges the head the batch left, rebuilds, and re-runs its touched tests.
     - The head then moves at most once per window, and the next window's proofs all re-run against that one
       head.
     - When a class B change lands in a slot, the next head's chain check (above) is skipped for the
       scenarios that change moves. It is recorded as skipped, with the class B PR named.
4. **Visual check by independent eyes (V5).** For every behaviour change (merge class B), spawn a
   subagent (the Agent tool) that did not write the change, give it ONLY the frames and the checklist
   (T01's V-a…V-i; T14's R-a…R-j where they apply) — never your description of what they should show —
   and save its `visual-check.md` in the packet. A *fails* or *cannot tell* blocks until explained.
5. **Merge rule by class** (the issue states the class):
   - **Class A** — no engine behaviour change (tools, tests, docs, investigations, reporting that
     changes no motion, or a refactor proven by byte-identical traces under lockstep): the tick may
     merge its own PR into `ai/manure-mpm` once the build, the fail-on-head fixture and every case in
     every touched test file pass, and the packet is written.
   - **Every landing, class A or B, runs the image gates on its merge result:**
     `python3 tools/test/p1_role_ratchet.py` and `tools/test/extension_version_gate.py`. A word above its
     baseline lands only on an operator grant named in the landing comment; otherwise stop with a
     `Plan finding:`.
   - **A class B landing also runs the whole fast set on its merge result** (`bin/slow_set.sh fast`, host), not only its
     touched files. A case green on the head and red on the merge blocks the landing. #1464 turned two cases in files
     it did not touch red (#1524).
   - **A head that fails an image gate or the fast set is repaired first:** its repair issue is take-first. Other
     landings may go on only if they add no failure to the fast set on their merge result, and the landing comment
     names the head's own red cases. Then close the issue. A PR that 3c holds
     for a merge slot lands only in a slot, as 3c says.
   - **Class B** — a behaviour change: open the PR with the packet linked in `## Verification`, add
     `ai:signoff` to the PR and the issue, post `harness tick pid <PID> pausing: awaiting
     sign-off, packet <path>`, and END. Never merge a class B PR before its sign-off comment.
   - If a class A issue turns out to change behaviour, treat it as class B and say why.
   - **Class B after its sign-off** (landing in a merge slot, 3c). When the issue carries a comment starting
     `Owner sign-off: approved` or `Operator sign-off: approved`, a tick may land the PR: merge `origin/ai/manure-mpm` into the branch,
     move the extension version past the branch's if it was taken (one version, one CHANGELOG section),
     rebuild, re-AOT the cubin if kernels or the device solver moved, run every case of the touched
     test files on the merge result, push, merge the PR, remove `ai:signoff` and `ai:wip`, and close the
     issue with the merge commit. If the merge needs more than version and CHANGELOG resolution, it
     depends on the hunks:
     - **Different lines.** Where both sides edited adjacent but different lines, and one side replaced
       lines the other did not touch, resolve it as a union. Name each hunk and both commits in the
       landing comment, and the operator reviews it after the fact.
     - **The same line.** Where both sides edited the same line, stop and post a `Plan finding:` with
       the proposed resolution.
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
    - it must show the robot actually moving. Read that from the robot's joint states with
      `tools/eval/boot_check.py <run folder>` (#1509, PR #1511), never from `robots[].links`. Since #1476 that
      block lists only the links a zone composes.
      **A plan that commands no motion** (`boot_check.py` prints whether it does; RS1, RS2 and RS5 do not) proves
      its boot by the robot's joint-state series advancing in time: the articulation publishes throughout the run.
      It does not need joint travel. Whether the joints visibly settle after the build hold depends on when the
      observer starts, so travel there is not evidence either way (operator, 2026-09-27, #1466).
    **A kit that mounts the branch's `worv.robots.catalog` must also mount the branch's
    `worv.robots.locomotion` and `worv.robots.attachments` builds.** The baked image's locomotion rejects
    `[drive].wheel_odom_hz`, and the robot never boots. That is how #1367's first traces were all invalid.
    A run that fails this check is named as failed in the packet, never used.

## Done

When no `ai:plan` issue is claimable, print a one-line summary and end.
