# Runbook — worv.env.manure continuum engine harness (mission 2)

Read by every tick before it does anything. Repo: `MaumAI-Company/isaac_sim`, checkout at
`/home/khemoo/tmp_workspace/isaac_sim` (TARGET_REPO — never edit it; it is the user's
working checkout). Work branch: `ai/manure-mpm` (cut from `ai/manure` @ 0c1920c3, which holds mission 1: the field
engine, contracts, gates). Never modify `master`, `ai/manure`, `feature/*`, `fix/*`, or any branch a
tick did not create. Harness home:
`/home/khemoo/tmp_workspace/claude-issue-harness` (state under `state/`).

## Mission (mission 3, 2026-09-06: review fixes — see the Mission 3 block at the end of design.md, which binds over anything below that conflicts)

Replace the kinematic material model of `worv.env.manure` with a real one: a WoRV-owned MLS-MPM
continuum solver (Warp kernels, AOT cubins, launched from C++) coupled TWO WAYS to the PhysX loader
through `omni::physx::IPhysx`, with the landed `HeapField` as the far representation. The binding
design is `/home/khemoo/tmp_workspace/claude-issue-harness/prompts/design.md` — read it in full
before claiming; the long form with rationale is
`~/tmp_workspace/artifacts/manure-mpm-design/manure-mpm-design.html`. Do not re-litigate its
decisions; record a blocking contradiction on the issue and stop.

Mission 1 (issues #648–#657, PR #668) is the base: keep its capability claim, wire contracts,
`HeapField` + mesh, wheel compaction, gate/perf scripts and docs layout; retire the bucket sweep,
in-bucket morph, rigid clumps and `_worldForce` write as the issues direct. `worv.env.manure_chunks`,
`worv.env.ground_cover`, `worv.env.terrain`, and the bootstrap/scene compute-contract code stay
read-only.

### Targets (each with its measurement)

- **M1 — build gate**: `tools/dev/iter.sh build worv.env.manure` (plus touched exts) green in a
  worktree; doctest binaries exit 0 in the builder image; AOT cubins for the manure kernels compile
  through `worv.core.warp_compat/tools/aot_compile.py` in the builder; version gate, settings parity
  and build-native self-tests pass. Final: full `docker compose build` green.
- **M2 — behaviour gate** (robot `s76`, `TestPlane_bigtrimesh_ai414`, CPU physics 240 Hz, render
  20 Hz, headless simrobot, lights companion): `tools/test/manure_gate.sh` PASS on the reference spec
  (pile 0.7 m³ at 25 % dry matter 2.5 m ahead: after a 2 s drive with the lip at ≤0.16 m, pile volume
  down ≥ 10 %, bucket `carried_m3` ≥ 0.05, coupling force on the bucket link non-zero while cutting)
  PLUS the new checks the issues add: slab failure of a 0.5 m face, sticking on the back wall until
  the dump pitch, spill under tilt, ruts after a drive-through, and a slurry push (0.5 m³ at 8 % DM
  spreads to its yield thickness and flows around the bucket sides). Frames + state documents archived.
- **M3 — perf gate**: `tools/test/manure_perf_ab.sh` with substep spans: added tick wall p50 ≤ 5 ms,
  p99 ≤ 8 ms scooping with four reference piles; ≤ 0.3 ms with no active zone; pre-step span
  ≤ 4 ms at 240 Hz (or the async ratio the spike selected, recorded on the tracking issue);
  conversion hitch ≤ 20 ms; GPU memory ≤ 512 MB. Numbers in the PR body, JSONL archived.
- **M4 — no regression**: read-only extensions untouched; existing tool tests pass; kit settings
  parity; the compute contract unchanged (`WORV_GPU_PHYSICS` semantics, `overrideGPUSettings`
  policy, single-scene stepping).

## Priority — fix, don't accumulate findings

- Claim the OLDEST open `ai` issue without `ai:wip` whose `Depends on:` issues are all
  closed (their PRs merged into `ai/manure-mpm`). If the oldest is blocked by dependencies, take
  the next one that is not. Prefer resuming a released claim (issue with `ai:wip` whose
  recorded PID is dead) over fresh work.
- Do not file a new issue unless it blocks the fix in hand. Note anything else in one
  sentence in the tick log.
- A tick that ends with no code change and no verified evidence is a failed tick unless it
  was blocked; say so plainly.

## Tick procedure

1. **Setup** — `git fetch origin`. Every push and
   branch mutation runs under `flock /home/khemoo/tmp_workspace/claude-issue-harness/state/locks/stack.lock`.
   Never run `git checkout`, `git stash`, `git clean` or any edit in TARGET_REPO itself.
2. **Claim** — add `ai:wip`, comment `harness tick <UTC timestamp> pid <PID> claiming
   (host <hostname>)`. The PID must be the long-lived `claude -p` process, never a tool
   shell (those die within seconds and would look orphaned). Get it ONLY with
   `PID=$(/home/khemoo/tmp_workspace/claude-issue-harness/bin/tick-pid.sh)` in the same command
   that posts the comment; never use `$$`, `$PPID` or a PID copied from earlier output,
   and verify with `ps -o args= -p <PID>` before posting. Liveness rule: a claim is held
   while that PID is alive (`kill -0`); a dead claimant's issue is resumable from its recorded
   state (open PR, comments). After adding the label, re-read the issue comments: if another
   LIVE PID claimed it within the last few minutes, withdraw (remove nothing — the label is
   theirs) and pick the next claimable issue. If no issue is claimable (every open one is
   claimed by a live PID or blocked by an unmerged dependency), print a one-line summary.
   Leaving an issue unfinished with `ai:wip` kept: post `harness tick pid <PID> pausing: <state,
   PR, worktree, exact resume recipe>` — the launcher counts an issue whose claimants have all
   withdrawn, paused or died as claimable again, so a successor spawns for it.
   saying so and END THE TICK immediately — do not audit, do not start a kit session.
   PAUSE MEANS END: the pausing comment is the last thing a tick does before its summary. Nothing
   runs after it — no suite, no re-check, no extra probe. If more work is worth doing, it is the
   next tick's, from the recipe.
3. **Implement** — worktree: FIRST check whether `/home/khemoo/tmp_workspace/ai-worktrees/mpm-<issue#>`
   already exists (a killed tick leaves one, with uncommitted work and no commits). If it does, `cd`
   into it, read `git status` and the files, and CONTINUE that work; never delete it or start a
   parallel branch. Otherwise create it: `git worktree add /home/khemoo/tmp_workspace/ai-worktrees/mpm-<issue#> -b ai/<issue#>-<slug> origin/ai/manure-mpm`
   (after `git fetch`). If a later issue builds on an unmerged sibling PR, branch from that
   PR's branch instead and say so in the PR body. Follow the repo `CLAUDE.md` hard rules
   exactly (orchestrator phase, no per-tick pxr, pxr and usdrt never in one TU, settings via
   `ISettings`, `SettingResolve.h`, TOML config through `worv.core.config`, three-segment
   extension names, comments state constraints not history, no Co-Authored-By, conventional
   commits without parentheses).
   PR #290's head is fetched locally as `pr/290` (`git show pr/290:extensions/env/worv.env.manure/...`):
   reuse its Dockerfile PhysX header fetch (rework to the pinned-fetch pattern its review asked for),
   its `IPhysx` physics-step subscription, spawn hygiene and Fabric GPU array path. Newton's implicit
   MPM (github.com/newton-physics/newton, Apache-2.0) is the solver reference; fetch it read-only into
   the worktree's scratch dir, never vendor it wholesale — port the kernels you need with attribution
   in the commit message, not in comments.
   Reference implementations to copy patterns from (read, do not import):
   `extensions/env/worv.env.manure_chunks` (capability claim, subscriber, cold-path pxr
   authoring TU, version/changelog layout, doctest ConsoleApp premake project),
   `extensions/env/worv.env.climate` (native runtime + Fabric array writes,
   `FabricGpuArray.h`), `extensions/_template_worv_cpp/README.md` (new-extension checklist).
   Build: `tools/dev/iter.sh build worv.env.manure` from the WORKTREE root (plus any other
   ext you touched). The PhysX-header issue changes the Dockerfile BUILDER stage: after it lands,
   every tick must run `tools/dev/iter.sh builder` once from its worktree before building (the
   builder image tag `worv-builder:isaac6` is shared — rebuild it under the stack lock and post the
   image id on the issue). AOT cubins: the tooling carries this now (#916, PR #918) -- `iter.sh build` prints
   which kernel artifact the tree holds for the host's SM, `iter.sh up` refuses a mounted
   `worv.env.manure` with none and prints the exact AOT command to run, and all three live gates
   refuse both that mount and a first `/manure/state` document that does not name a `cuda:<ordinal>`
   device. Run the command the refusal prints; `WORV_MANURE_ALLOW_CPU=1` is the only way past it, for
   a run whose subject really is the CPU reference. Mounted ext dirs shadow baked cubins, so a kernel
   change still needs the step -- but a tick no longer has to remember it. A cubin that EXISTS but was compiled from older kernel sources is the worse trap: the fence passes, the device runs, and any counter whose slot moved reads garbage (two #961 verdicts were this). Before every AOT run `rm -f extensions/env/worv.env.manure/data/*.cubin extensions/env/worv.env.manure/data/*.ptx`, and re-run AOT after ANY merge or edit that touches `kernels/*.py` or `MpmGpuSolver.h`. In a tick compile ONLY the host card's arch: put `WORV_AOT_ARCHS=120` in front of the `aot_compile.py` call inside the printed command's `-lc '...'` string (one cubin in ~1 min instead of three plus PTX in ~3.5 min); the image build compiles the full set itself. The reverse trap is as real: a plugin built before a checkout against cubins built after it dies with `cuMemcpyDtoHAsync: an illegal memory access` and the solver fence refuses the run — rebuild the plugin after EVERY checkout, then AOT. Doctests: run them SHARDED, or a fail-on-head plus fix pair costs two nine-minute runs of `test_manure_spec`: `docker run --rm -v <worktree>:/w:ro --entrypoint bash worv-builder:isaac6 -lc "bash /w/tools/test/run_doctest_sharded.sh /w/extensions/env/worv.env.manure/bin/tests/<binary>"` (defaults to min(nproc, 8) shards, merges the counts into doctest's own summary, and fails if the shards did not between them run every listed case). `tools/dev/iter.sh build --test <ext>` does the build and that run in one command, for EVERY binary the extension declares (both of manure's, one summary line each and one `ext-test gate:` total) with the card fenced off the container, so its device cases skip and are counted as `DEVICE-SKIPS`; `--test --device` gives it the card and is refused unless this tick holds the lease. Add `--fast` (`iter.sh build --test --fast <ext>`, or `--fast` before the binary on the wrapper) to drop doctest's `[slow]` test suite -- the two dry-matter slope-agreement cases that are 72 % of the suite's wall -- for a fail-on-head compile check. SUITE BUDGET per tick: iteration runs `--fast` on the ONE binary the change touches; a fail-on-head check runs only the new case (`-tc=<case name>` on that binary, no shards); the FULL set runs at most ONCE per tick, on the commit you propose to merge, and never before a fix is in hand. Each run names its set in its own summary line. The unsharded single-process form is `docker run --rm -v <worktree>/extensions/env/worv.env.manure:/e:ro --entrypoint bash worv-builder:isaac6 -lc "/e/bin/tests/<binary>"`.
   Python gates: `python3 tools/test/test_extension_version_gate.py`,
   `python3 tools/test/test_kit_settings_parity.py`, `python3 tools/test/test_build_native_checks.py`.
4. **Verify** — run the target slice the issue names. For a mission-3 review fix the FIRST verification is the new test failing on the unfixed head (check it out, run it, quote the failure), then passing on the fix. Kit-level runs need the resource lease
   (below) and these steps from the worktree root: `cp /home/khemoo/tmp_workspace/isaac_sim/env/nucleus.env env/`;
   `tools/dev/iter.sh up worv.env.manure worv.robots.catalog <other touched native exts>`;
   `WORV_SIM_ROBOT=s76 WORV_ENVIRONMENT=TestPlane_bigtrimesh_ai414 WORV_KIT_ARGS="--exec /tmp/mg/lights.py" tools/dev/iter.sh kit`
   (`WORV_KIT_ARGS` passthrough landed in mission 1; `iter.sh kit` does NOT forward `WORV_GPU_PHYSICS`,
   which this mission never needs);
   kit stdout is `/tmp/kit.log` INSIDE the `worv-iter` container. Publishers: the sidecar
   `<container>-ros2cli` (`worv-iter-ros2cli` for the default container; created by `tools/dev/iter.sh topics`, name from `worv_ros2cli_sidecar_name` in `tools/dev/ros_domain.sh`) runs
   `/usr/bin/python3` rclpy scripts — copy `pub_manure.py`/`pub_joy.py` from
   `~/tmp_workspace/artifacts/manure-instancer-diag-2026-09-02/` into it with `docker cp`,
   run as `source /opt/ros/jazzy/setup.bash && /usr/bin/python3 /tmp/pub_manure.py '<spec>' 4`.
   Boot to `scene bootstrap complete` takes ~40 s on TestPlane; poll the log in bounded loops.
   Domain: `iter.sh up|kit|topics` pin `ROS_DOMAIN_ID` (derived from the container name when
   unset, never 0), and `manure_gate.sh`, `manure_perf_ab.sh` and `manure_teardown_gate.sh` REFUSE
   a run whose `ROS_DOMAIN_ID` is unset or 0 — export one (e.g. `export ROS_DOMAIN_ID=77`) for the
   whole session, name it on `lease.sh acquire <issue> <domain>`, and use the same one for every
   `iter.sh` and gate command of that run.
   Known traps: the baked image ships `worv.comm.base` 0.10.0 against the repo's 0.11.0 — a worktree
   that mounts only `worv.env.manure` gets a NULL comm, never logs `manure: subscribed`, and the gate dies
   at the subscriber wait with nothing in the log (four ticks lost a run to this). ALWAYS
   `iter.sh build worv.comm.base worv.comm.ros2 worv.env.manure` and mount all three. `s76_v2` HEAD asset has DEAD boom/bucket cylinders (no drive target) — use
   `s76` for any gate needing bucket motion; a spec sent before `manure: subscribed` is not
   replayed; mounted ext dirs shadow baked `data/*.cubin` for climate/terrain (expected
   self-disable noise, not a regression); a robot starts at the origin facing +x.
   Archive evidence under `~/tmp_workspace/artifacts/manure-harness/<issue#>/` (logs, JSONL,
   frames) and quote the decisive lines in the PR body under `## Verification`. Scope
   honestly: "compile-verified only" when no live run backed a claim.
5. **Submit** — push the branch under the stack lock; `gh pr create --base ai/manure-mpm` with a
   conventional-commit title (no parentheses), body: what changed, `Fixes #<issue>`,
   `## Verification` with real evidence. Leave reviewers EMPTY.
6. **Land, by base branch** — PRs into `ai/manure-mpm` MAY be merged by the tick that opened them
   once M1 is green for the change and the issue's own gate passed (`gh pr merge --merge
   --delete-branch`, under the stack lock, re-checking mergeability immediately before).
   Never merge into `master`; the final integration PR `ai/manure-mpm → master` is opened by the
   release-readiness issue and stays open for the human. Never merge a PR you did not open, with one exception: a tick resuming an issue whose earlier claimant is dead may merge that issue's open PR once it has re-run the suites on the branch with the base merged in and re-proven the fail-on-head fixtures, and says so in the merge comment.
   Never force-push a pushed branch, including to rebase it: bring the base in with `git merge origin/<base>` and resolve there. A rebase that rewrites pushed commits is a rails breach even when the contents are identical (#846 tick 4).
   Never `git stash` in a worktree: the stash list belongs to the whole repository and is shared with the user's checkout (#900's tick popped into it). Park work with a WIP commit on the tick's own branch instead.
   ONE REVIEW PER TICK: there is no separate review tick and no review gate. Instead, after the change is written and its fixture passes, and BEFORE the commit and PR, spend ONE review pass on your own work — exactly one, scoped to this tick's purpose and the code it touches: the fix itself, the fixture's discriminating power (would it fail on head for the stated reason, and only that reason?), the callers and the host/device pair of what you changed, and the invariant the issue names. Act on what it finds in the same tick, or say on the PR why not. Do not review the whole extension, do not open a second pass, and do not review another tick's work. A `[fable-review]` comment from the retired review tick is HISTORY, not a gate: read it for findings, never wait for a CLEAR. A fix merges on its fail-on-head fixture and its live verdict, nothing else.
   PERF DRIFT (measured 2026-09-16): this host's scoop window drifts about 6 ms BETWEEN SESSIONS — the same 3.0.23 image read +21.89 and +28.33 hours apart, while idle read about +3.9 both times. So a cached control validates the ABSOLUTE bar only. NEVER state a comparison between two perf numbers taken in different sessions, in an issue, a PR body or a tick summary; a relative claim needs both arms in ONE session under one lease. Ten merges on this branch were attributed from cross-session readings and none of it was falsifiable.
   PERF IS MEASURED ONCE, AT THE IMAGE — NOT PER PR (operator rule 2026-09-15, after 27 of 40 ticks went to perf pairs instead of the feature). A fix PR merges on TWO things: its fail-on-head fixture and its live verdict. No `manure_perf_ab.sh` run is owed for a merge, no same-session control, no interim rule, no absolute ceiling at the PR. The ONLY perf reading is the release image's (`#937`-class issue): if the head is over the restated M3 there, ONE headroom issue fixes it with the stage split, once. A tick that spends over a third of its budget on measurement scaffolding, test scaffolding or perf when the issue's ask is a physics fix has misread its ask.
   PHYSICS FIRST: an issue whose ask is a material, exchange, coupling or surface behaviour outranks any gate, tools, probe or perf issue for the card and for the claim — but only while it still has a user-visible symptom. Once a physics issue's remaining scope is host/device consistency or internal tidiness, or an operator comment on it says it yields, it drops BELOW the release issue, and a tick that has spent three claims on it without landing says so and takes the release instead; claimable.sh order follows the issue, and a tools tick yields the lease to a physics tick that is waiting.
   LIVE VERDICT SIZE: three draws for a fix (six only on the release image); a settle window only where a clause reads settle-end; a diagnostic drive reads the drive alone.
   (For the release image's single perf reading) the BASE CONTROL is CACHED per base commit: `bin/perf_control.sh dir` prints a usable cached control for the current `origin/ai/manure-mpm` head (empty when none), `bin/perf_control.sh tree` gives the shared built+AOT'd base worktree (`ai-worktrees/base-<sha>`, never deleted by a tick), and `bin/perf_control.sh new` a dir to run a fresh control arm into. Run the fix arm FIRST; then `perf_control.sh check <fix RESULTS.md>` — it accepts the cache only when it is under 24 h old and the fix arm's idle p50 is within 0.5 ms of the cached control's (the host-state check that made the control same-session). Accepted: pass it as `MANURE_PERF_CONTROL` to re-read the bands and do not run a control arm. Refused: run one control arm into `perf_control.sh new` and it becomes the cache. One control arm per base commit per day is the budget; a tick never rebuilds a base worktree that `tree` already holds.
   Nucleus preflight before ANY kit launch: run `/home/khemoo/tmp_workspace/claude-issue-harness/bin/nucleus_ok.sh` (exit 0 = go). It returns at once when the host-side probe (cron, every minute) shows `10.50.2.21:3009` stable for 5 minutes; only without a fresh reading does it fall back to the 5-minute in-line loop. Never write your own probe loop. The server flaps (2026-09-07, 2026-09-14): a kit launched into a flap logs `resolveServerPath failed` after ~130 s, pushes host load past 20, and the arm is void. If the loop fails, pause the claim and do not take the lease.
   Never search the filesystem from `/` or `/mnt`: headers live in the builder image (`docker run --rm worv-builder:isaac6 find /opt /isaac-sim -name Version.h`) or under the worktree. A `find`/`bfs` rooted at `/` crawls the NFS and CIFS shares, sits in D-state for hours after the tick exits, and its threads alone push load1 past the 16-core bound every live arm is fenced on (four such crawls held every tick's live arm off for a whole morning on 2026-09-10).
   Never `pkill -f <pattern>` on a bare command name: four ticks share this host. Stop your own processes by the PID you recorded when you started them, or anchor the pattern on your own worktree path (`pkill -f '<worktree>/.*build'`), and never on `claude`, `kit`, `docker` or `python` alone.
   Any script that launches, stops or restarts a kit (`iter.sh kit`, `manure_gate.sh`, `manure_perf_ab.sh`, `manure_teardown_gate.sh`, a smoke run of any of them) runs ONLY under the lease; there is no unleased smoke run. A tick without the lease must not touch `worv-iter`.
   This host's docker default runtime is `nvidia`: a plain `docker run` of ANY doctest binary or python in the builder image takes the card, its DEVICE cases run instead of skipping, and it counts as an unleased device run. A host-only doctest run passes `--runtime=runc` (or `-e CUDA_VISIBLE_DEVICES=`) explicitly; only a leased run omits it.
   A device doctest run (`test_mpm_gpu_parity`, any suite with DEVICE cases on `cuda:0`) also runs ONLY under the lease: it contends the card and makes a sibling's perf arm refuse itself. Host-side compile and the `--fast` set need no lease.
   After merging: remove `ai:wip`, close the issue with a one-line summary, remove the
   worktree (`git worktree remove --force`; if root-owned build artifacts block it, clean via
   the builder image: `docker run --rm -v <worktree>:/w --entrypoint bash worv-builder:isaac6 -lc "rm -rf /w/extensions/*/*/obj /w/extensions/*/*/bin"` then `git worktree prune`).
7. **Done condition** — when every issue is closed and the integration PR is open with M1–M4
   evidence, refresh the tracking issue checklist and end the tick with no changes.

## Resource lease — one GPU, one warm container

The warm `worv-iter` container, the `worv-iter-ros2cli` sidecar and the GPU are ONE serialized
resource. Any `iter.sh up/kit/topics`, kit session, or perf measurement requires the lease:

- Take, check and drop the lease ONLY through
  `/home/khemoo/tmp_workspace/claude-issue-harness/bin/lease.sh acquire <issue#>` (prints ACQUIRED, or
  BUSY with the live holder and exits 1), `lease.sh release`, `lease.sh status`. It takes both flocks,
  refuses a live holder, computes the tick PID itself, and only lets the holder release. A bare write to
  `resource.lease` is a bug: it has twice clobbered a sibling's live session and once killed its kit.
- Before treating the resource as busy, read the lease and `kill -0` its PID. A dead owner
  is orphaned: `docker exec worv-iter pkill -f "^/isaac-sim/kit/kit"` (ignore errors),
  overwrite the lease, proceed. Never wait on a dead owner; never wait more than 20 min on a
  live one — prefer a claimable issue that needs no GPU.
- Preflight before evidence runs: `nvidia-smi --query-compute-apps=pid,process_name --format=csv,noheader`
  must show no other kit process; if it does, the GPU is contended — do not measure.
- `tools/dev/iter.sh down` and delete the lease when your session ends. Cap any kit session at
  15 minutes (a bowl burst is 8; a gate draw is 10). Write your tick summary to stdout BEFORE starting a kit session and append
  after it.

## Rails

- One issue per tick, bounded scope. Never force-push. No AI attribution in commits.
- Only `ai`-labeled issues are ever worked. Never touch unlabeled issues.
- Never request or assign a PR reviewer.
- `worv.env.manure_chunks` and `worv.env.ground_cover`/`worv.env.terrain` sources are
  read-only for this mission (the only allowed touch is the catalog `[manure]` switch and
  kit dependency lines named in the issues).
- Ticks are finite `claude -p` batch runs: no ScheduleWakeup, no ending a turn to wait.
  Bounded polling inside one command; tear everything down before the turn ends.
- Never weaken a gate or threshold to obtain a pass; archive failed evidence honestly.
- Version discipline: `worv.env.manure` goes 0.2.9 → 0.3.0 on the first PR that lands solver code, then
  patch bumps per PR that changes it (merge `ai/manure-mpm` and bump past whatever landed first); each bump needs a matching top `docs/CHANGELOG.md`
  section (the version gate rejects otherwise). `apps/*.kit` versions stay 1.0.0.
