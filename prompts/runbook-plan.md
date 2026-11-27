# Runbook — plan execution (mission 5, from 2026-09-23)

The harness implements the owner-approved manure plan. The plan binds over everything else,
including `runbook.md` and `design.md` where they conflict.

## Hosts (owner, 2026-11-21, 2026-11-23)

Host names, addresses and paths here are placeholders; `tick-prompt.md`'s PLACEHOLDERS paragraph says where each real value lives.

Harness work runs on the remote cards alone: `<gpu-host-b>` (RTX 5080) and `<gpu-host-a>` (RTX 5090, 24 GB). `bin/host_health.sh` orders them every 3 minutes: the operator's preference, `state/locks/harness-host-preference`, with a degraded host (no ssh, or an upload under 5 MB/s) moved last; `lease.sh status`'s `harness ticks:` line shows the order. A host moved last still takes work that copies little, such as a device case on a staged copy; an arm that copies a tree goes to the first host. A compile goes where `bin/build_host.sh` says (below). Kits, tests with a card, AOT, device suites and cost reads go there, never to this host's own card. This host's CPU takes compiles when `bin/build_host.sh` names its local lane (owner, 2026-11-26); the shim refuses a tick's container with a card on the local daemon. `lease.sh harness-only` is on with both hosts in that order: an issue-number acquire tries the first host first, a device or whole-card run takes the first host, and `remote-env.sh` routes a tick's docker to the first host. A recipe that names another host runs on these two instead. A cost read's parent and change arms stay on one card.

Each remote host is its own lane, with its own locks. The single remote lock names in the passages below (`/tmp/worv-dev-build-remote.lock`, `/tmp/worv-fast-set-remote.lock`) are replaced by per-host names:

| host | compile lock (`WORV_DEV_BUILD_LOCK=`) | suite lock (`flock`) |
|---|---|---|
| `<gpu-host-a>` | `/tmp/worv-dev-build-remote-<lock-name-a>.lock` | `/tmp/worv-fast-set-remote-<lock-name-a>.lock` |
| `<gpu-host-b>` | `/tmp/worv-dev-build-remote-<lock-name-b>.lock` | `/tmp/worv-fast-set-remote-<lock-name-b>.lock` |

**Which host compiles (operator, 2026-11-26).** Before each `tools/dev/iter.sh build`, run `eval "$($HARNESS_HOME/bin/build_host.sh)"`. It exports `DOCKER_HOST` and `WORV_DEV_BUILD_LOCK` for the first host whose compile lock is free, and leaves out a host that a cost read holds whole. It also leaves out the card a waiting cost read asks for, so that card can go quiet (a wait for any card keeps the quieter unheld host free). When every lock is taken, it picks the host with the fewest waiters. The build's outputs copy back here, so the arms may run on either card. Never queue a compile behind another host's compile while the other lane is free. With `state/local-cpu-lane` present, this host is a third lane, after the remote ones: its export line names `DOCKER_HOST=unix:///var/run/docker.sock`, the host's own dev build lock (shared with the owner's builds) and the builder image the file names. Every line sets `WORV_BUILDER_IMAGE`, so eval it again before each compile. The local lane is for compiles and card-less host tests; a device run, AOT or anything with a card stays on a remote host. CPU-only suites belong on it, so the remote cards stay free for runs that need one: the scheduled slow and fast sets run there, and a device-record run's host stage runs there too, before it takes a remote card for the device stage alone.

- **Never build or run a suite on a host whose card a cost read holds** (`lease.sh status`: a live `remote-all` on its slots). Use the other host's lane, so the read times the engine and not a compile.
- **Choosing a lane for a build or a suite.** Take the host whose lock is free. If both are free, take the one with the lower load1 per core (`ssh <host> cat /proc/loadavg; nproc`). If both are held, wait on the one with the shorter holder. Put that host in `DOCKER_HOST=ssh://<host>`.
- `remote-env.sh`'s default route sends a bare `docker` to the first host as the order stood when the tick started. A build or a suite on the other host names it explicitly.
- The binaries and logs come back to the worktree from either host, so a build on one host and its kits on the other are fine.
- A recipe in an older pause comment that names the single remote lock, or one host for a build, may use either lane. A kit still runs where its lease's export line points.

## Merge slots (owner, 2026-11-25): every hour is a slot

The owner asked for the piled work to run and land in parallel. Every hour is now a merge slot: a slot is open at any time. This overrides every passage below that names `hh % 3 == 0`. Landings still go one at a time through `bin/merge-lock.sh`, and a class B landing still waits while a class A `ai:ready` issue is left to land. A ready landing is claimed before new work.

## Testing (owner, 2026-11-24): implement, then check only what changed

The owner: "each job takes excessive testing time, just implement and only check the modified part". This section overrides every passage below, every issue's Acceptance and every operator comment that asks a change to prove more.

**What a change runs before it lands:**
- Build the touched extensions.
- Run the test files the change touches or adds: `tools/test/run_doctest_sharded.sh --file '*<file>.cpp' <binary>`, and the touched Python unittests. A new or changed behaviour gets its own case. Where the Ask names a case that fails on the parent, show it red there once.
- If device code changed (`kernels/`, `mpm/MpmGpuSolver.h`, or a device test file), rebuild the cubin. Run only the device cases of the touched test files, by name, under the device lease.
- Run a scenario arm only when the change exists to move that scenario, such as a cost cut or a law change. Use the one targeted scenario, seed 1322, one parent arm and one change arm.
  - A cost cut reads that pair's mean tick.
  - A law change reads the numbers its Ask names on that pair. If the resting surface can change, add one render pair of that scenario.
- The image gates that run in seconds: `p1_role_ratchet.py` and `extension_version_gate.py`.
- **A law change on by default is the exception (operator, 2026-11-25).** A change that is on by default and edits the material law (`mpm/MpmMaterial.h`, the constitutive return and its strength knots) or the contact law (`CouplingBoundary.h`, the guard, the exchange's convert and return) moves every behaviour case, so its touched part is the whole host `test_manure_spec` set, `[slow]` included, host only, before it lands. A change behind a setting that is off by default keeps the touched-file rule; the full host set runs when the default is turned on.

- **A measurement-only step lands with its feature, not alone (operator, 2026-11-25).** A census, counter or probe written to read a design's estimate before the build is read from the issue's own branch, and lands with the feature it measures. Landing it on its own costs a merge, a rebuild and a re-test each time the head moves, for code that changes nothing.

**What a change does not run:** the whole fast set, the whole device suite, RS1–RS6 identity arms, extra seeds, repeated cost sequences, kernel captures, or V5 beyond that one pair. `device_record.py check` may read stale on a change, and the full regression below rewrites the record.

**Landing:**
- Merge the head, rebuild, re-run the touched files, then push and merge.
- If the head moved since the proof, re-run only the touched files. Nothing else is re-proven, unless the move changed one of the same files, in which case re-run that file's tests too.
- A class B change still waits for the operator's sign-off, read on the evidence above.
- **This holds for class B landings and for the device record too.** A landing, of any class, never runs the whole fast set or the whole device suite, and never writes the device record. Older passages below that ask a class B landing or a batch-mate for them are overridden. If device code changed, run only the device cases of the touched device test files, by name.

**The full regression** runs as its own issue, which the operator files about once a day: the whole fast set, the whole device suite and its record, and RS1–RS6 against the last full run's parent arms. A red there is filed against the landing that caused it.

## What binds, in this order

1. **`PRINCIPLE.md`** of the plan — engine rules P1–P10, validation rules V1–V5 and V2a.
2. **`DECISIONS.md` §1** — the owner's answers of 2026-09-23 (D1–D14).
3. **The item document** the issue names (`todo/Tnn-*.md`, `STRATEGY.md` probes), and the
   investigations it cites.
4. The repository's `CLAUDE.md` hard rules.
5. **`public-record.md`** — how every issue body, PR body, comment and close summary is written,
   for a reader who has the repository and nothing else; `bin/public_record_lint.py` gates each post
   (override 14).
6. `runbook.md` — for the **mechanics only**, as listed below.

The plan's working copy is `$PLAN_DIR/`. The same text is in the
repository under `docs/manure-plan/` on `ai/manure-mpm` (merged by #1318). Read it there.

## Which issue

- Only issues labelled **`ai:plan`** are claimable. An `ai` issue without `ai:plan` is not worked.
- An `ai:plan` issue whose body starts with `Operator: **take this before any other claimable issue.**`
  goes first; then an issue whose landing is still to prepare: an `Operator sign-off: approved` comment and no
  `ai:ready` yet, so finished work lands before new work and does not chase the head; then resume a released claim
  (dead claimant), the one whose last pause comment is oldest first, so
  one long issue cannot take every tick while others wait; then the oldest `ai:plan` issue whose
  `Depends on:` issues are all closed.
- An issue labelled `ai:low` (owner, 2026-11-24) is lower priority. Claim one only when no claimable issue without `ai:low` is left, landings included. The speed path (the first version's 150 ms bar) carries `ai:first`, and always goes first.
- **While an `ai:first` issue is claimable, an `ai:fable` issue without `ai:first` is not claimed either** (owner, 2026-11-26): new design rounds and low-priority work wait for the take-first queue. `bin/claimable.sh` counts the same way, so ticks are not started for work that waits.
- An issue labelled `ai:signoff` is waiting for a sign-off. Never claim it; never merge its PR. The owner
  waived class B sign-off on 2026-09-26, so the operator reviews the packet and signs off in the owner's place.
- An issue labelled `ai:ready` waits for a merge slot (override 3c). While a slot is open, resume it before
  any other issue; outside a slot, never claim it.
- An issue labelled `ai:fable` is in its diagnosis or design phase: a root cause to find, a law to choose, or a question
  the evidence does not yet settle. It is worked on Fable 5.1 (the fable lane). The first line of your prompt says which
  lane this tick is on:
  - a fable-lane tick claims only `ai:fable` issues, resumes included: a released claim without the label at the
    moment of the claim is not the fable lane's, even if the label left seconds before (operator, 2026-10-10, #1845);
  - any other tick never claims one, unless its first line says Fable is cooling down on its usage limit.
  `ai:fable` is the operator's to set, like `ai:long`: a tick never adds or removes it. The operator takes it off once the
  design is settled and what remains is implementation, runs or a landing. When your work reaches that point on a fable
  issue, end your pause comment with the line `Design settled: the rest is implementation and evidence.`
- An issue labelled `ai:operator` is waiting for the operator. Never claim it. A pause that cannot go on
  until the operator answers (a `Plan finding:` the step depends on) adds `ai:operator` in the same command
  as the pause comment. A pause a later tick can resume by itself (no slot, the budget, owed runs) never
  adds it. The operator removes it after answering.
  - **Before that pause, read the issue's comments again.** The operator answers while your tick runs, often within
    minutes of the finding. If an `Operator` comment newer than your `Plan finding:` answers it, act on it, or name it in a
    budget pause without `ai:operator`. Never pause waiting for a ruling that is already posted (operator, 2026-10-15:
    #1889 paused at 10:58Z on a question answered at 10:48Z).

- **Read the issue's comments again after the claim, before the first build or lease, before each new queue of arms or
  whole-card session, and before the merge lock.**
  The operator's rulings and lane labels land seconds before or after a claim. A ruling posted after your claim binds
  your tick as one posted before it (operator, 2026-10-15: #1894 claimed at 12:06:26Z, the ruling came at 12:06:41Z,
  and the step landed without it).
  A queue started on a superseded recipe spends the card twice (operator, 2026-10-23: #1960's skip-identity arms ran
  after the 12:4xZ ruling dropped the skip, and #1973 reserved the card for a reading the 18:4xZ ruling moved).

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
   `$PLAN_DIR/evidence/<YYYY-MM-DD>-<issue#>-<slug>/` with an
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
   **A cost read may take a whole remote card (operator, 2026-11-16).** A per-change gain read (parent and change
   alternated) needs a card with no other process on it and a quiet host, not the local card in particular.
   `lease.sh acquire <issue> --all-remote` grants every slot of one remote host whose card runs nothing else and whose
   load1 is under 6 (`bin/remote_room.sh --alone`); ask again between arms to keep it. Run every arm of the read on
   that one card, and name the card and the host load in the report. Take it before waiting for the local card's whole-card
   grant. The first version's speed item (the engine against PhysX particles) is still read on the local card.
   **A cost read times the engine's own spans, not an nsys capture (operator, 2026-11-16).** The gain read takes the
   tick, the GPU step and the named phase rows from the kit's tick-wall and profiler spans. `nsys start` has hung the
   kit in three arms (#2233 twice, #2235 once). Capture nsys only when the Ask names a kernel row, and as its own
   arm after the span arms. A hung capture voids that arm only. The scheduled suites now skip a host whose card a
   cost read holds (`bin/slow_set.sh`), so check `lease.sh status` rather than the suite logs before an arm.
   **A cost arm ends after the step it reads (operator, 2026-11-16, e7c69b8d4).** Run each arm of a gain read with
   `--through-sim-s` at the end of the step or window it reads: RS6's push 13, RS3's drive and its windows 2–3 20.
   A rest-tick read is a separate, unclipped arm. A clipped arm is never an identity arm (`series_compare` voids it).
   **Bring a container up with `iter.sh up <exts> && iter.sh sidecar`.** The sidecar starts from a toolset image
   built once per host, so no queue backgrounds `iter.sh topics`, greps its log or kills it mid-replacement any more;
   the runner kills the kit at collect, so no queue waits on a kit stop either.
   **What a PR carries from the packet (operator, 2026-11-15).** Under `docs/manure-plan/evidence/<packet>/` a PR commits
   only what a reader needs to follow and repeat the reading: `INDEX.md` and any other `.md` reading with the tables, the
   readers and scripts (`.py`, `.sh`), queue files (`.queue`), scenario and probe inputs (`.toml`, `.diff`). Raw outputs stay in the
   artifacts packet and are never committed: suite and build logs, boot checks, comparison dumps, status files, state
   series (`.log`, `.txt`, `.jsonl`, `.out`, `.pid`, `status`). The main repository's `info/exclude` skips them; never
   `git add -f` one. `INDEX.md` gives every number the sign-off or the landing reads, so nothing it cites must be opened.
   Before a PR is marked ready, `git rm --cached` any such file it already carries.
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
     **The acquire's exit code is the lease.** Never pipe `lease.sh acquire` through `tail`, `head` or `grep`
     (a pipe returns the last command's status): run it alone, or with `set -o pipefail`, and start nothing on
     the card unless it exited 0 (operator, 2026-10-23: #1965 ran a device case on a whole-card session after
     `| tail -1` hid a `BUSY device`).
     A device run takes one shard while any kit holds the card (`run_doctest_sharded.sh <binary> 1`; check
     `nvidia-smi --query-compute-apps=pid --format=csv,noheader`), and at most 2 on a card no kit holds: the
     lease reserves 3 GiB for the whole run, and each shard holds its own context and arrays. Four shards
     beside the kits ran the card out of memory twice on 2026-10-15 (#1871's run sent a parent kit to the CPU
     reference; #1878's parity read 4 `cuMemAlloc` failures), and two shards beside two kits once more (#1845)
     (operator).
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
   - **Balance the identity queues (operator, 2026-11-16).** Since 9595097a1 (#2227) RS6's zone freezes 9 s after
     its push (stamp 34.0 s on seed 1322), and its arm runs 175–186 s on a quiet host, like RS1's. With three slots
     run {RS6, RS2}, {RS1, RS4}, {RS3, RS5}; with two, {RS6, RS1, RS2}, {RS3, RS4, RS5}. A tree without 9595097a1
     does not freeze on RS6 and runs about 430 s: merge the head first.
   - **RS6's identity arm ends after the freeze (operator, 2026-11-16, e7c69b8d4).** A class A change arm runs RS6 as
     `--variant identity` (the push, then 12 simulated s of rest) and is compared with the shared parent's whole
     `RS6-p`; `series_compare` pairs the marks and wrench lines both hold.
     - The variant arm must end frozen: its last document reads `zone_frozen: true`, `frozen_steps` of at least 480
       and every `freeze_wakes` cause 0. An arm that ends unfrozen is void for identity: run RS6 whole.
     - The parent arm stays whole. It also serves class B pairs and the chain check, which never run the variant.
     - Run RS6 whole, never the variant, when the change touches `mpm/ZoneFreeze.h`, `model/BodySleepGate.h`,
       `mpm/ZoneSurface.h`, `mpm/LodExchange.h`, `model/LegacyExchange.h`, `mpm/FieldSupport.h`, `HeapField.h`,
       `model/PhysxBackend.h`, `model/IPhysicsBackend.h`, a hunk of `mpm/ManureZone.h` that names freeze, sleep,
       wake, rest or quiet, or `rest_freeze_after_s`. Four changes have parted RS6 first inside its rest (#1744,
       #2026, #2227, #1858), the last at 83.75 s, and all four touch that list.
     - A class B review, a release reading and every reading of M2→M3 run RS6 whole.
   - **A single-slot grant may be on the remote card** (slots 4–5, domains 80–81, `remote=` in the ACQUIRED line).
     - Its export line sets `DOCKER_HOST` and puts `bin/remote-docker` first on `PATH`. So every `docker` and
       `nvidia-smi` call made under it, from `iter.sh`, `scenario_run.py` and the queue, runs on that host.
       The shims copy the run's bind mounts across and read that card.
     - A remote arm is identical to a local one: RS4 on parent-af846aec8 matched bit for bit, 431/431
       documents and the whole wrench trace. So change and parent arms may run on either card.
     - **Two build lanes.** The local compile lock (`/tmp/worv-dev-build.lock`, held by `iter.sh build` for the
       compile only) serves one compile at a time for every tick on this host. When it is held at all (operator,
       2026-11-16: a first waiter sat 40 minutes behind two compiles and missed its merge slot), or the local load1 is
       past twice `nproc`, build and run host suites on a remote host instead: the first of `<gpu-host-a>` and  (See "Hosts" above.)
       `<gpu-host-b>` whose `REMOTE_CARD_HOST=<host> bin/remote_room.sh` prints `ok` and whose card no
       cost read holds (`lease.sh status`: no live `remote-all` on its slots):
       `DOCKER_HOST=ssh://<host> PATH=$HARNESS_HOME/bin/remote-docker:$PATH WORV_DEV_BUILD_LOCK=/tmp/worv-dev-build-remote.lock tools/dev/iter.sh build …`
       (#2002, 2026-10-28). Every tick uses that one remote lock name, so the remote lane also compiles one build at
       a time beside its kits. The binaries and logs come back to the worktree as from any remote run.
     - **One fast set per host at a time.** The doctest runner shards a suite over min(nproc, 8) processes, so two or
       more fast sets on one host thrash each other and none finishes inside a tick (three were cut off on
       2026-10-28 at load1 55–76). Run a fast set or a whole-binary host suite under that host's suite lock,
       detached to a log: `flock /tmp/worv-fast-set.lock tools/dev/iter.sh build --test --fast …` here, and
       `flock /tmp/worv-fast-set-remote.lock` with the remote lane's export line on the remote host. Take the lane
       whose lock is free; if both are held, wait on the one with the shorter holder. A single touched file
       (`-sf`) needs no lock. Run it with `tools/test/run_doctest_sharded.sh --file '*<file>.cpp' <binary> [workers]`
       (repeat `--file` for several files; `--fast` still drops `[slow]`). The runner refuses `-sf=` and any other
       argument it does not know, so a touched-file run can no longer quietly become a whole-binary run.
     - **Landings take the suite lanes first while a slot is open** (operator, 2026-10-29). A tick that is not landing
       in the slot does not start a fast set or a whole-binary suite while both of these hold:
       - a merge slot is open (UTC hh % 3 == 0, the first 60 minutes);
       - an `ai:ready` issue is open.
       It runs `-sf` files, builds and host probes meanwhile, and takes the lane once the slot closes or no
       `ai:ready` issue is left. One reason: a class B batch-mate re-runs the whole fast set on the head its batch
       leaves. On 2026-10-29 #1998 and #2006 queued behind member-evidence suites on both lanes and missed the
       09:00Z slot.
     - Keep every kit-side call of the run under the export line.
     - **Both hosts run work (owner, 2026-10-08: "use both devices, this one and also the remote one together").** The
       local host has been on since 2026-10-07 23:59Z (`lease.sh local --on`; no `state/locks/local-card-off`).
       - Leases grant the local card's slots and each remote host's together (`lease.sh status` lists the hosts and their slots). A local grant's export line
         carries no `DOCKER_HOST`, so its run, a `--device` run included, runs here. That is correct: never stop such a
         run as a breach.
       - The remote host is shared with other sessions. Before a build, a test suite or a kit on the remote lane, run
         `bin/remote_room.sh` (`--render` for a rendering kit). Use the remote lane only when it prints `ok`; on `busy`
         run the work locally. A lease grants a remote slot under the same check.
       - Two remote hosts take explicit runs: `<gpu-host-b>` (16 GB card, shared) and `<gpu-host-a>`  (See "Hosts" above.)
         (24 GB card, the faster CPU, usually free). Check a host with `REMOTE_CARD_HOST=<host> bin/remote_room.sh` and
         put that host in `DOCKER_HOST=ssh://<host>` below; prefer `<gpu-host-a>` when both print `ok`.  (See "Hosts" above.)
       - The remote lane is reached explicitly: `DOCKER_HOST=ssh://<gpu-host-b>
         PATH=$HARNESS_HOME/bin/remote-docker:$PATH WORV_DEV_BUILD_LOCK=/tmp/worv-dev-build-remote.lock …`, under
         `flock /tmp/worv-fast-set-remote.lock` for a fast set. Prefer it for a suite while the local load is high: the
         cron slow set runs on the local host every third hour.
     - **While the local host is off** (`lease.sh local --off`, only on the owner's word), builds, AOT, tests and kits
       all run on the remote host:
       - Every bash a tick starts carries an exported `docker` function (`bin/remote-env.sh`). It sends each `docker`
         command through `bin/remote-docker` to the remote host, inside or outside an export line.
       - Before a run, the shim copies the paths the run mounts to the remote. After a foreground run it copies the
         writable mounts back, so `iter.sh build`'s binaries, an AOT's cubin and a test's logs land in the worktree here
         as before. A detached kit copies nothing back; its outputs come back through `docker exec` and `docker cp`.
       - Runs that share a worktree or a packet wait for each other while one of them writes it.
       - A `docker` binary reached past the function finds no daemon: `timeout docker …`, `xargs docker`, `env docker`,
         Python's `subprocess` and a `sh` script read a `DOCKER_HOST` that names a missing socket
         (`…/use-the-docker-function-or-bash-c.sock`) and fail to connect instead of running on this host.
         - Put the docker call inside bash: `timeout 600 bash -c 'docker run …'`.
         - Run Python that calls docker under a slot's export line, which sets `DOCKER_HOST` and the shims' `PATH`.
       - `docker compose` goes through the function to the remote daemon but copies no mounts; ticks do not use it.
       - `lease.sh` then grants remote slots only. A whole-card run takes both remote slots, and a device run's export
         line carries `DOCKER_HOST`.
     - **Every scenario arm mounts `worv.core.scene` from its own tree, parent arms included,** until the runtime image is
       rebuilt from `ai/manure-mpm` (owner's release step). The image bakes master's 0.5.4, which lacks #1636's
       external-force flag (0.5.6). So an arm without the mount runs pre-flag physics, and a pair that mounts it on one
       side only parts at the pile's build (#1682). `bin/arm_queue.sh`'s default `--base-exts` mounts it. `tools/eval/series_compare.py` voids a pair
       whose kits loaded different builds of an extension.
     - **Every scenario arm mounts `worv.core.utils` from its own tree too, parent arms included,** once #1750 has
       landed and until the image is rebuilt. RS6's ground is an `omniverse://` URL. The image's 0.2.2 reduces it to its
       basename and loads the original trimesh map without an error, and `scenario_run.py` then fails the arm. The
       default `--base-exts` mounts it beside `worv.core.scene`.
     - **A per-point impulse from PhysX's contact report is not evidence where a pair has more than one friction
       patch** (#1754). The report lists points in narrowphase order, while the solver writes impulses in patch
       order. Read per-pair totals, and per-point separations and normals. A per-point attribution needs a diagnosis
       arm with `eMODIFY_CONTACTS` raised on the pair (`evidence/2026-10-24-1754-report-order/checks/report_order.py`),
       which puts the report in order and can part a drive by millimetres.
     - **Closed issues' packets are trimmed daily** (`bin/evidence_trim.py`). Their wrench traces over 20 MB are
       `wrench-trace.csv.zst` (read with `zstd -dc`); their `.npy` AOVs, the video frames an MP4 holds, and void runs are
       gone (`CLEANED.txt` says what). Old bases' parent folders keep only their small text. Re-run an arm to regenerate.
     - **Which card a timing read takes (operator, 2026-11-26).** A change read against its parent, as pairs on one card, holds on either card: run it on whichever card is free (no `--want-host`, so the queue takes the first card that is free and quiet). Only a phase read against the 150 ms bar needs `<gpu-host-b>`, the card every bar number comes from. Never leave a card idle waiting for the other one for a pair read; `bin/card_dispatch.sh` unpins a queue that waits 5+ minutes on a busy card while another is free, idle and quiet, by writing `any` to its `<runs>/want-host`. A queue that reads the bar passes `--bar-read` to `bin/arm_queue.sh`, and is never moved. A whole-card run that times nothing, such as a render pair for V5, takes `--acquire-args "--all-remote --no-cost"`: the card still runs nothing else, but the host's load is not read and its builds go on beside it, so a render never waits for the build host to go quiet.
- **Work while a queue waits (operator, 2026-11-26).** A queue waiting for a card needs no watching: the runner polls and the stall watch reports a long wait. While it waits, do the issue's card-free work: the next Ask's design, code, compile and host cases. Do it in a separate worktree on its own branch, because an arm reads the queued trees' `bin/`; never rebuild or edit a tree a live queue may still run. Never spend a wait in a sleep loop, and never pause an issue for want of a card while card-free work is left. A device-record or other card run takes its card only for its device stage: host suites first, with no card.
- **Readings ride with the change (operator, 2026-11-26).** A reading that only measures (a bar table, an attribution, a setting read alone) is part of the issue whose change it decides, never an issue of its own; its numbers go in that issue's packet. A test-only issue exists only for the daily full regression, its device run, and a failing gate.
- **Run arms through `bin/arm_queue.sh`, never a copy of a runner script.** The packet holds a queue file
       (`<name>.queue`, one `tree|arm|scenario|extra_exts|kit_settings|seconds|outdir|flags` line an arm), its inputs
       and its outputs. The script takes the lease itself, so launch it detached, outside any export line:
       `setsid nohup $HARNESS_HOME/bin/arm_queue.sh --issue <n> --packet <packet> --queue <packet>/<name>.queue
       --deadline <epoch> [--acquire-args "--all-remote"] [--collect /tmp/<file>,…] >/dev/null 2>&1 &`.
       - Poll `<packet>/runs/status` until its last line is `RELEASED`. Exit 4 is no lease by the deadline, 3 an early
         end (STOP, the deadline, a lost lease).
       - `--help` lists the rest: a wanted host for a whole remote card, the margins, the baseline runner
         (`--runner baseline`), host load sampling and the head check.
       - A second queue of the issue runs as its own process with its own queue file, and takes its own slot.
   - **A class B render session takes one slot, with `lease.sh acquire <issue> --render`.** Change against
     parent, lockstep with moments, frames for V5, run on that one slot.
     - **The frames are the same whatever shares the card.** Moments are step-locked (#1386).
     - **One rendering kit at a time.** A rendering kit now takes about 8 GB. It fits beside an identity kit
       (5.1 GB) on the 16 GB card, but two rendering kits do not: two RS3 attempts died of GPU out-of-memory
       that way (#1524).
       - `--render` marks the slot. While another tick's rendering kit is live, the call prints
         `BUSY render` and records a wait that holds no place ahead of kits that can start.
       - A renewal between arms keeps the mark.
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
     whole run sets `LEASE_HOLDER_PID=$$`, as `bin/arm_queue.sh` does. Never edit `state/locks/`, including the
     `all-wait.*` records.
   - **A tick that holds the whole card for a baseline** runs only the baseline's arms under it, and
     releases before any other arm. A series of arms run under `--all` keeps every sibling off the card for
     the whole series. Between the baseline's runs, renew with `lease.sh acquire <issue> --all --render`. While
     another issue has priority it prints `YIELDED card` (exit 1): every slot is released and you have joined
     the whole-card line. Treat it as `YIELDED`: run `tools/dev/iter.sh down` for your container at once, then
     poll again.
   - A render product is never read on the update it was armed on.
3a. **Parent arms are shared per base commit.** Every refactor proving §F.3 against the same base runs the
   same parent arms (lockstep, `--no-moments`, same seed), so run them once and share them:
   - **Where they live.** `evidence/parent-<base9>/<scenario>-p/`, with the parent's build tree
     `ai-worktrees/parent-<base9>` and a `status` file in the same format as the arm queues. `<base9>` is
     the base's first nine hex digits: the head you merged before the proof.
   - **Reuse a finished one.** Before running a parent arm, look there. If `<scenario>-p rc=0 end`
     is in its `status` and the run passed override 13's boot check, compare your change arm against
     it and cite it in your packet.
   - **A landed change's arms are the new base's first parent arms (operator, 2026-11-16).** Before running
     `parent-<new9>/<scenario>-p`, compare `git rev-parse <new head>^{tree}` with the tree of the `commit` in a
     finished `<scenario>-c/run.json` from the landing's packet. If they are equal, and the flags, seed, kit settings
     and mount set match, link that arm in as `<scenario>-p` with a `status` line naming its source, and run only
     `-p2` if the proof asks for a second parent arm. A slot that landed two changes has no such arm: run them.
   - **Otherwise claim and run it there.** `mkdir evidence/parent-<base9>/<scenario>-p` claims the arm:
     only the tick whose `mkdir` succeeds runs it, into that folder, not into its own packet. If the
     folder exists without a finished status, another tick is running it: run your change arms
     meanwhile. The arm's queue line names `evidence/parent-<base9>` as its outdir: `bin/arm_queue.sh` runs into
     the empty folder your `mkdir` made, and its `start` and `rc=` lines go to that folder's `status` too.
   - **A queue's STOP file stays in its own packet.** `bin/arm_queue.sh` reads `<packet>/STOP` and
     `<packet>/runs/STOP`, and writes its self-stop on a CPU-reference fallback to `<packet>/runs/STOP`, also while it
     runs a parent arm into the shared folder. Never put a STOP in `evidence/parent-<base9>/` (operator, 2026-10-15:
     #1871's fallback STOP there stopped #1845's `RS6-pm-s1323`).
     It reads STOP before each arm, so the running arm finishes and writes its state series and census.
   - **Check the base first.** A proof uses a base only once each scenario replays on it (§F.3's precondition).
     Two readings show that, and a scenario needs one of them:
     - **The chain line** (3c). The base's `<scenario>-p` identical to the window base's `-p` in `chain.txt` is two
       runs of the same physics that agree. Run no `-p2` for that scenario.
     - **A second parent arm.** Where the chain line is missing, was skipped for a class B landing, or parts, run
       `<scenario>-p2` and use the base once `-p` and `-p2` are identical.

     If neither holds, the base does not replay itself. Post a `Plan finding:` before proving against it, as with
     #1436. A single unchecked parent arm per base let #1411's proof pass by chance while RS2 had begun to split.
   - **Every scenario of the set.** A §F.3 proof covers RS1–RS6. RS4 joined on bases from 8ec107ad3 (#1417,
     D16a), and runs under `lockstep_sync --no-moments` like the rest. A missing parent arm is a reason to claim and run it, never a reason to leave its scenario
     out. A class A merge with a scenario left out is not proven: pause instead, and say which arms are owed.
   - **Keys set aside in every identity comparison:** `header`, `models`, `models_sha256` and
     `collider_set.queries` (operator, 2026-09-29).
     - The last is a reader-query count that includes the queries made before the first document. It depends
       on when the observer starts, not on the physics, so it parts by one between identical runs.
   - **One cubin.** Your change tree runs the parent tree's cubin (§F.3) while the kernel sources are
     unchanged.
   - **A pair that parts** is re-run with the parent arm in your own session, beside your change arm,
     before it counts as a finding (R32: timing, load).
3b. **Fetch before every arm, not only at the claim.** When `origin/ai/manure-mpm` has moved in code that
   runs in an identity arm, stop arming the old base. The arms already recorded stay as evidence. Merge the
   head, rebuild, and arm the new base's parents and your change there, because a proof on a base the slot
   no longer has cannot land.
   **A head move re-runs only what its overlap touches (operator, 2026-11-16).** Merge the head and rebuild, then
   read `git diff --name-only <old base> <new base>` against the change's own files:
   - **Change arms.** Read the new base's `chain.txt` before arming anything. If it is not written yet, run the new
     base's `-p` arms (3a) and write it; never re-run the six change arms ahead of it. A scenario whose chain line is
     identical carries by the rules below; only a scenario that parts, or that a class B landing moved, is re-run.
   - **Touched test files.** The change's own touched test files, never the ones the move touched. Re-run them
     on the merge result when the move changed one of them, a header they include, or a production file the
     change also edits. Otherwise the earlier log carries. A move's own test files are read by the fast set on the
     merge result and by the scheduled whole set, not by the landing.
   - **The fast set.** It runs once, on the merge result that lands (override 5, one run per tree). A prepared run
     carries across a move that brings no production file and no test file of the extensions the change touches.
   - **The device suite.** Re-run it only when the move or the change touches device code (override 5).
   - **Never re-run for a head move:** the fail-on-head output, a mutation, a class B render pair on a scenario
     that carries, or a cost read. A mutation is re-run only when the merge changed the lines it mutates. A cost
     read is a difference of arms on one base and stands unless the move changes a file under the phase rows it
     reports; say so in the packet.

   **A proof carries across a base move, scenario by scenario, where the chain check allows it.**
   - **The check.** Compare the new base's `<scenario>-p` against the old base's `-p` (`chain.py`, as 3c's batch check does).
   - **The condition.** They may part only in state keys the landings in between added or changed, with the particle
     hashes and the wrench trace identical, and the base move's diff shares no file with your change.
   - **Then** your change arm on the old base stands for that scenario on the new one. Merge the head, rebuild, run the
     touched test files, and cite the chain line and the keys set aside in the packet.
   - **Otherwise** re-run that scenario on the new base.
   - **A class B pair carries the same way (operator, 2026-10-02, #1702).** A change-against-parent pair on scenario S
     stands on a new base when the move shares no file with the change beyond version and CHANGELOG, and every landing
     in the move is tests only, class A by identity, or class B whose own packet shows S's particle hashes and wrench
     identical to its parent. A class B landing that parts S's particles voids S's carry: re-pair S on the new base.
     Name each carried pair and its base in the sign-off packet.
     - **A shared file does not void the carry when the move is identity-proven** (operator, 2026-10-08, #1777). The
       pair still carries across a file the move shares with the change when all of these hold:
       - every landing in the move shows S identical to its parent in its own packet, particle hashes and wrench, but for
         keys it added. That covers tests-only moves, class A by identity, and a class B landing whose packet shows S
         untouched (operator, 2026-10-10, #1820 across #1828);
       - in each shared file, the merge result differs from the new base by exactly the change's own lines;
       - the touched test files, the fast set and, for a change in device code, the device parity binary pass on the
         merge result.

       Name each shared file and that check in the packet. A landing in the move whose packet parts S's particles still
       voids S as above.
     - **Where the landing's packet has no pair for S, the chain check decides** (operator, 2026-10-03, #1734). Compare the
       new base's `<S>-p` against the old base's `<S>-p` (`chain.py`), which costs nothing extra because each base needs its
       `-p` arms anyway. Identical particle hashes and wrench mean S's pair carries. Never re-pair a scenario the landing
       may not have moved before reading that check.
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
   - **The lock decides.** A landing whose merge lock was taken inside the slot may finish after the slot
     closes. A tick that would take the lock after the close does not take it: it prepares its merge result,
     pauses with the recipe, and lands first in the next slot.
   - **Which PRs wait.** A class A PR that changes code running in an identity arm: an extension the
     identity kits mount, the scenario runner, the render companion or a scenario file. Docs, tests,
     analysis tools, unmounted extensions and investigations merge when ready, as before; 3b exempts them.
   - **Ready.** When the proof holds against the current head and the packet is written:
     - a perf step, whose issue asks for a gain, is ready only once its gain read has posted against the
       same base, stated as taken with no other process on the GPU. Identity alone does not make it ready: a cut that does not pay is dropped,
       not landed (operator, 2026-11-03, #2148; P7);
     - add `ai:ready` to the issue and the PR;
     - post a pause comment that says in plain words the change is ready to land, proven on `<base9>`, with
       the packet as `docs/manure-plan/evidence/<packet>/INDEX.md`, and the line `harness tick pid <PID>
       pausing` inside its Harness trace block (override 14);
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
     4. Rebuild and run the touched test files (override 5's definition) on the merge result. A landing that changes code in
        an extension the identity kits mount also runs the whole fast set there, class A included (operator,
        2026-10-09, #1822). A change to a shared quantity can red a test it never touched: #1812's audit change
        to the thin-plate split reddened `test_added_mass_exchange.cpp`'s contact-energy check, and only the
        cron fast set caught it after the landing. Done before the lock, the fast set costs the slot nothing.
     5. Push, merge the PR, close the issue with the merge commit, then
        `bin/merge-lock.sh release <issue>`.
     A landing that starts inside the slot may finish after the slot closes.
     - **Prepare before the lock** (operator, 2026-10-09). Do steps 2–4 on the current head *before* step 1,
       together with any re-AOT, the fast set or device parity the landing owes: merge, rebuild, test.
       Then take the lock and fetch.
       - **The head is still the base you prepared on:** push and merge at once.
       - **It moved:** release the lock, re-prepare on the new head, and try again inside the slot.
       - **It moved by tests-only commits alone** (3b exempts them, and none touches the change's own files): merge,
         rebuild, and run every case of the test files those commits touched, every test file that includes them, and
         the change's own touched files, on the new merge result. A moved test file whose binary builds from no file
         the change touches carries the head's own fast-set result instead, once that run is green on the new head. The prepared fast set and device parity carry,
         because no production file and no other test file differs from the prepared merge result. Any other move
         re-prepares in full (operator, 2026-10-24: a tests-only move at 11:16Z sent two prepared landings back
         through the whole fast set, and both missed the 12:00Z slot).

       The lock is for the push alone, not for a 25-minute AOT or fast set. One landing that holds it through
       its preparation fills the slot, and every landing behind it, class B above all, slips three hours.
       Take it only once the merge result's suites are green. A preparation still running when the slot closes
       waits for the next slot; taking the lock to keep the slot while a suite runs is the case this rule forbids
       (operator, 2026-10-10, #1835: lock at 09:57Z, fast set under it until 10:06Z, merged 10:07Z).
   - **A train for PRs that share a file (operator, 2026-11-17).** Two ready PRs that edit the same production file
     conflict when the first lands, and the second then re-proves on the new head. On 11-16 #2242 and #2244 were
     each proven three times this way. So, before a PR proves for a slot:
     - **Find the PR ahead.** List the open `ai:ready` PRs (and any PR whose tick posted `ready` earlier) that share a
       production file with yours (`gh pr view <n> --json files`). Ignore version, CHANGELOG and device-record files.
     - **None:** prove against the window base as now; PRs that share no file land as one batch.
     - **One or more:** take the earliest-ready one as your base. Merge its branch into yours, resolving the shared hunks
       as unions, and prove on that merge result. Say `rides on #<pr>` in your ready comment and in the PR body.
     - **Landing a rider.** Land it in the same slot right after the PR it rides on. Step 3 accepts the commits
       that PR's landing brought as a batch-mate's, and step 2 then meets no conflict outside version and
       CHANGELOG, because the rider already holds them.
     - **If the PR ahead is dropped or re-proves,** the rider re-merges onto whatever is ahead of it then, and
       proves once more. That is one re-proof, where without the train it pays one per slot until it lands first.
     - **Proofs still run in parallel.** Only the base a rider proves on changes. Its cost read carries, unless the
       PR ahead changes a file under the rows the rider reports.
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
     - Class B batch-mates in one slot do not void each other's carries: each merges the head the batch
       left and re-runs its touched tests and the fast set there (operator, 2026-10-29, #1998). Because no
       proof covers the combination, the first tick that records parent render arms on the slot's final
       head reads V5 against the window base's renders for the scenarios the batch moves. A worse item no
       batch-mate's own V5 placed is a `Plan finding:` naming the batch, with `ai:operator`.
3d. **PRs are proven in groups, not one by one (owner, 2026-09-29).** One proof covers a group, and the
   combination that lands is the combination tested.
   - **A group is an issue.**
     - The operator opens it with `ai:plan` and `ai:group`, and lists its members in landing order
       (`Members: #a, #b, …`) and the group's class.
     - While a member is in a group it carries `ai:operator` and a `Group: #<g>` comment, and its own tick
       runs no scenario arm.
     - The group's tick claims the group issue as usual.
   - **Class A group** (identity-preserving members whose host checks are green):
     1. **Build the group branch.** Create `ai/group-<g>` from `origin/ai/manure-mpm` and merge each member's
        branch in order.
        - A member whose merge needs more than version and CHANGELOG resolution leaves the group. Post the
          conflict on it, drop it from the list, and lift its hold.
     2. **Host checks.** Build, then run every case of the union of the members' touched test files, and the
        fast set.
     3. **One set of arms.** Run RS1–RS6 change arms of the group branch against the shared parent pairs
        (3a), one set for the whole group, on either card.
     4. **All identical.** Set aside only the keys the members' packets name.
        - Post `Group proof: identical on <base9>, packet docs/manure-plan/evidence/<packet>/INDEX.md` on each member.
        - Lift the holds and add `ai:ready` to each member and its PR.
        - The members land one by one in the slot, in group order, as 3c says.
     5. **A scenario parts.** Bisect on that scenario only: re-run it on half the members, then narrow.
        - The parting member leaves the group with the reading, to become class B or be fixed.
        - The rest are re-proven on that scenario.
        - Every run goes in the group's packet.
   - **Class B group** (behaviour changes to the same physics, such as the bucket–steel contact):
     1. **Member evidence.** Each member keeps its own fixtures and fail-on-head evidence in its own packet.
        A group may be filed while its members are still implementing (operator, 2026-10-29, #2042). Each member
        then keeps its lane and finishes that evidence without scenario arms and without the fast set. The group's
        host checks run the fast set once, on the combination. Each member posts
        `Member evidence ready: <branch>@<hash>, packet docs/manure-plan/evidence/<packet>/INDEX.md` and holds itself with `ai:operator`. The group issue
        stays on `ai:operator` until every member has done so, and the operator lifts it.
     2. **Read the combination.** The group branch, built as above, carries every member. Its scenario arms
        (change against parent), V5 and tick cost are read on that combination, the tree that will land.
     3. **Sign-off and landing.** The operator signs off the group on the group's packet.
        - The members then land together in one slot, in order, after that slot's class A batch.
        - Each re-runs its touched tests on the merge result.
4. **Visual check by independent eyes (V5).** For every behaviour change (merge class B), spawn a
   subagent (the Agent tool) that did not write the change, give it ONLY the frames and the checklist
   (T01's V-a…V-i; T14's R-a…R-j where they apply) — never your description of what they should show —
   and save its `visual-check.md` in the packet. A *fails* or *cannot tell* blocks until explained.
   - **Read a moment by its held physics step (operator, 2026-10-02, #1700).** In a run with moments, the state
     documents' `header.stamp.sim_time_s` runs ahead of physics time by the moment holds (M2's held step 12721 is
     stamped 61.6 s, not 53.0 s). Match documents to M1–M3 by each moment's `held_step`, never by the stamp; an
     "M2→M3" read by the stamp is the settle before M2.
   - **RS6's push is a bifurcation (#1704, operator 2026-10-01).** Sub-millimetre differences before the drive decide
     whether the crate climbs the face (z ≈ 1 m) or wedges against it (z ≈ 0.67 m); unchanged parents end at Base x
     1.52–2.05 m, and the moments alone move a draw 0.32 m. So:
     - RS6's push travel, crate path and V-i are read over at least three seeds per arm (the scenario runner's seed
       option, as `2026-10-01-1626-seed-draws/` did), each draw's branch named from its final Base x and crate z
       (`evidence/2026-10-01-1704-parked-sleep/checks/final.py`);
     - a single draw that passes V5 needs no more draws. A single draw's worse frames are the change's only when they
       recur in the change's draws of the same branch and are absent from the parent's draws of that branch;
     - never attribute a deeper or shallower park to a change from one draw per arm.
   - **RS3's rest is read over three seeds too (operator, 2026-11-02, #2132).** Changes to the pile's settle part the run
     before the drive, so each tree parks on another draw. The still-body group passed RS3's rest on seed 1322 alone
     and moved material beside the parked rear wheels on 1323 and 1324. So:
     - RS3's rest rows (every row within 2 cm of a link within r_rest over the rest window, the group's row probe and
       `rest_tracks.py`) and V5 at rest are read on seeds 1322, 1323 and 1324 per arm;
     - a bar is a maximum, so quote the worst track per seed, never a percentile.
     - **A change that picks the branch itself** (operator, 2026-10-02, #1704): when every draw of the change takes one
       branch over at least three seeds, and the parent's branch per seed repeats on the base, the branch move is the
       change's. Read RS6 across branches, and do not count the branch against it. Weigh a change-only V5 item with a
       census of the particles there: support root, asleep or awake, the holding rule, speed. Supported debris of the
       branch does not block; material the change itself holds up does.
5. **Merge rule by class** (the issue states the class):
   - **What each kind of change owes (operator, 2026-11-16).** A change runs its row and nothing more, unless the
     issue's Ask names another run. A check outside the row is read by the scheduled suites (`bin/slow_set.sh`:
     the fast set hourly, the whole set every third hour) or at the release, not by the tick.

     | kind | owes | does not run |
     |---|---|---|
     | **Evidence or harness only** (packets, readings, investigations, analysis and comparison tools that never run inside an arm) | the tool's own unittest, if it has one | builds, suites, arms, gates |
     | **Tests or docs only** (no file an identity arm runs) | build; the touched test files; the fast set only when a shared test header changed; the image gates | arms, cost, mutations; the device suite unless a device test file changed |
     | **Class A, identity holds** | build; the fixture failing on head, or one mutation where no fixture can fail on head; the touched test files; RS1–RS6 change arms against the shared parents, once; on the merge result the fast set once and the image gates; the device suite only for device code; tick cost from the identity arms' own spans | a second set of arms on a head move that carries; the device suite for host code; a cost arm, unless the issue asks for a gain |
     | **Class B, behaviour change** | all of class A's row with the fixture checking the law (override 2); with the setting off, RS1–RS6 identical to the parent where the change has a setting; change against parent on each scenario the change moves (RS3 and RS6 on three seeds when they are among them); one render pair and V5 on those scenarios only; on the merge result the fast set and the device suite once | pairs, seeds or renders on a scenario the off arm or the chain line shows unmoved |

     - **Touched test files** means the cases outside `[slow]` of each test file the diff changes, plus the `[slow]`
       cases the diff adds or edits, run by name. The file's other `[slow]` cases run too when the diff changes
       code outside a case body in that file, a test header the file includes, or the fixture those cases call.
       Otherwise the whole set reads them on the head.
     - **Device code** is `kernels/`, `MpmGpuSolver.h`, `GpuBoundary.h`, `GpuHostTraffic.h`,
       `worv.core.warp_compat` and the device project's own test files and stubs. A change to another file of the
       record's hash set runs only the device cases that read it, by file
       (`tools/test/run_doctest_sharded.sh --file '*test_zone_hold.cpp' <binary> 1` under the device lease), and
       writes no record.
     - **A perf step** (the Ask names a gain) adds one gain read: the one scenario and phase rows the Ask names,
       parent and change alternated twice (p c p c), card alone. A second scenario or an nsys capture only when the
       Ask names it.
   - **Class A** — no engine behaviour change (tools, tests, docs, investigations, reporting that
     changes no motion, or a refactor proven by byte-identical traces under lockstep): the tick may
     merge its own PR into `ai/manure-mpm` once the build, the fail-on-head fixture and the touched test
     files (as defined above) pass, and the packet is written.
   - **Every landing, class A or B, runs the image gates on its merge result:**
     `python3 tools/test/p1_role_ratchet.py` and `tools/test/extension_version_gate.py`. A word above its
     baseline lands only on an operator grant named in the landing comment; otherwise stop with a
     `Plan finding:`.
   - **Every landing also reads the device record on its merge result:**
     `python3 tools/test/device_record.py check --root .` (no GPU). A red check means the merge result changed a
     file of the device suite's hash set. What the landing then owes depends on the files (operator, 2026-11-16):
     - **Device code, or any class B change in the hash set.** Run the device suite on the merge result, once
       (`tools/dev/iter.sh build --test --device --suite test_mpm_gpu_parity worv.env.manure`, holding the device
       lease), commit the `tests/device_record.toml` that run writes as its own commit, and read `check` green
       before the merge.
     - **A class A change to host code only.** Its identity arms ran the device path on RS1–RS6 and came back
       identical. Land with the record as the head has it, name the changed hash-set files in the landing comment
       under `Device record: stale, <files>`, and leave `tests/device_record.toml` untouched, so two such landings
       in one slot never conflict on it.
     - **The refresh (operator, 2026-11-26).** A stale record is repaired once a day, by the daily full
       regression's one device run on that day's head, not after each landing: a landing never opens a refresh
       issue. A stale record blocks only the release image build, which the release-readiness issue runs after
       reading `check` green, so between those the head carries a stale record named in each landing comment. A
       device-code landing still runs the device suite on its own merge result, as above.

     Never write the record by hand. A device run that fails only in `cuMemAlloc: out of memory` is a card reading,
     not a verdict: re-run those cases alone by name once the card has room, and never the whole suite twice.
   - **One run per tree (operator, 2026-11-11).** A suite's green log belongs to the tree it ran on
     (`git rev-parse HEAD^{tree}`). Never run the fast set, a whole-binary suite or the device suite twice on one
     tree: cite the log. Before its merge result exists, a branch runs only the cases of its touched test files;
     the fast set and the device suite run on the merge result, once. A re-merge that brings no file the change or
     its tests touch carries the earlier logs by the file check of 3b.
   - **Mutations (operator, 2026-11-11; narrowed 2026-11-16).** A mutation shows that a new case can go red. A
     fixture that fails on head already shows it, so such a case needs none. Where no fixture can fail on head (the
     change adds the API its case reads, or the behaviour is unchanged by design), run one mutation for each rule
     the change adds, at most three, unless the issue names more. Build the mutant in the change's own worktree
     (dev build, one file), run the one case by name (`-tc=`), revert, and quote the red lines. No suite, no arm
     and no device run on a mutant.
   - **A class B landing also runs the whole fast set on its merge result** (`bin/slow_set.sh fast`, host), not only its
     touched files. A case green on the head and red on the merge blocks the landing. #1464 turned two cases in files
     it did not touch red (#1524).
   - **A head that fails an image gate or the fast set is repaired first:** its repair issue is take-first. Other
     landings may go on only if they add no failure to the fast set on their merge result, and the landing comment
     names the head's own red cases. Then close the issue. A PR that 3c holds
     for a merge slot lands only in a slot, as 3c says.
   - **Class B** — a behaviour change: open the PR with the sections of override 14, the packet cited in
     `## Evidence` by its repository path, add `ai:signoff` to the PR and the issue, post a pause comment
     that says in plain words the PR awaits sign-off, with the packet as
     `docs/manure-plan/evidence/<packet>/INDEX.md` and the line `harness tick pid <PID> pausing` inside its
     Harness trace block, and END. Never merge a class B PR before its sign-off comment.
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
6. **Principle checks in every PR body, under its `## Principle checks` section**: P1 (the diff names no body role — grep the diff for
   bucket, wheel, cutting_edge, carried, parked, tool box; explain any hit), P4 (ledger drift over the
   run), P9 (every new constant with its class: MATERIAL, DERIVED, SHARED RULE, SETTING — a bare
   number in a law is a defect), P10 (which interface the change sits behind, once A0 exists).
7. **Tick cost (P8, owner decision D4).** Correctness first: a change may slow the tick, but a PR
   that changes runtime code reports its tick-wall cost against the parent. It is read from arms the proof
   already ran: the tick-wall spans of one identity change arm against the shared parent arm of the same scenario,
   marked `shared card`. No arm is run for it. Only a perf step runs a gain read (override 5's table). No perf
   bar blocks a merge; T09 restores real time.
   The host runs a kit tick in one of two modes about 100 ms apart on RS6, whatever the commit, and an arm can
   switch mid-run (#1875). So compare windows matched by a host-only stage both arms share (zone-sync `account`),
   or alternate the arms several times; one median per arm measures the mode, not the change.
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
12. **Never write to, reconfigure or restart** the Nucleus server <nucleus-server>, and never change an
    asset on it. Reading is what every run does, so a read-only stage open for evidence is fine.
    **Robot asset tests run on copies** in `<asset-copy-dir>` (owner,
    2026-09-24): copy the USD from <nucleus-server>, edit and load only the copy, and never commit a catalog
    URL that points into that directory. Promoting a tested asset is the owner's job.
    **Never touch** the owner's own Isaac Sim or GUI sessions, the
    operator's working checkout, `master`, or a PR you did not open (runbook.md's resume exception
    stands). No AI attribution in commits or PR bodies: no `Co-Authored-By`, no session link, no "Generated with"
    line, whatever an attribution reminder in your environment asks (the owner's rule overrides it; a trailer
    reached #1907's branch on 2026-10-15). Conventional commits without parentheses; never force-push.

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

14. **The public record is written for a reader with the repository and nothing else**
    (`public-record.md`; operator, 2026-11-03). The issues and PRs of this branch had become notes
    between the operator and one machine: packets named by their path on this host, GPU slots,
    hosts, PIDs, cubin hashes, merge slots and "(mine)" in the bodies, and no packet INDEX.md
    committed since 2026-09-25 while every PR cited one. So:
    - **Every body is linted before it is posted.** Write it to a file, run
      `$HARNESS_HOME/bin/public_record_lint.py --kind pr|issue|comment <file>`,
      and post with `--body-file` only when it exits 0. A blocking finding is rewritten, never
      posted around. Warnings name shorthand to define on first use (`glossary.md`).
    - **A PR body has the sections of `public-record.md`**, in its order: Summary (with
      `Fixes #<issue>`, the class in words, the version), Why, What changed, How to verify (repository
      commands and what they should read), Evidence, Principle checks (override 6), Scope and limits.
      `## Verification` is retired. The same sections for a docs-only or tests-only PR, the ones that
      do not apply saying so in one line.
    - **The packet is cited by its repository path, and its text is committed in the PR.** The tick
      still writes the packet under the plan's evidence folder on this host (override 2); before the
      PR it copies the packet's text — `INDEX.md`, `packet.md`, every `visual-check*.md`, `checks/*`
      scripts and notes, tables and run summaries under 200 kB — into
      `docs/manure-plan/evidence/<packet>/` on its branch, and the body names
      `docs/manure-plan/evidence/<packet>/INDEX.md` at the PR's head. Frames, videos, traces and
      `.npy` stay out of git; the INDEX.md lists each with its size and says it is kept outside the
      repository under the packet's name. A packet cited by any other path is a blocking finding.
    - **Every reading carries its commit and its condition**, in a table when there are more than
      two: which tree, which scenario and seed, whether another process shared the GPU, the window
      and the percentile. A comparison is stated only between arms taken in one session, and says so.
    - **Harness words stay out of bodies**: tick, slot, lease, lane, merge slot, merge lock, whole
      card, worktree, cubin hash, budget, "(mine)", "the next tick". The measurement condition they
      imply is stated instead, or nothing. An operator ruling is quoted as what it decided, linking
      the comment, never as a time of day.
    - **A comment's visible text is the state in plain words**, each item with its commit: what
      is being started, done, running or owed, and what a successor must do. The protocol line the
      launcher parses — `harness tick <UTC> pid <PID> claiming|resuming, budget <n> min`,
      `harness tick pid <PID> checkpoint <n> at <UTC>`, `harness tick pid <PID>
      pausing|withdrawing|releasing` — goes **inside** the `<details><summary>Harness trace</summary>`
      block at the end, with the labels, the host, the slot, the worktree and the resume recipe. The
      launcher finds the line anywhere in the comment; in the open, "harness tick … pid … claiming
      (host …), budget 120 min (labels: …)" is the record of one machine, and the lint blocks it.
    - **A close summary** is one or two sentences: what landed, as which commit and version, and
      where the evidence is. A landing comment names the merge commit, the version and the suites run
      on the merge result.
    - An issue a tick files (one that blocks it) has the defect layout of `public-record.md`:
      `## Summary`, `## Observed`, `## Expected`, `## Evidence`, `## Ask`, a `Depends on:` line and
      `Class:`; `Depends on:` keeps its exact form because the launcher reads it.

## Done

When no `ai:plan` issue is claimable, print a one-line summary and end.
