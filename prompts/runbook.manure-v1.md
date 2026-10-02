# Runbook — worv.env.manure production runtime harness

Read by every tick before it does anything. Repo: `MaumAI-Company/isaac_sim`, checkout at
`/home/khemoo/tmp_workspace/isaac_sim` (TARGET_REPO — never edit it; it is the user's
working checkout). Work branch: `ai/manure` (cut from `master` @ db9d98b9). Never modify
`master`, `feature/*`, `fix/*`, or any branch a tick did not create. Harness home:
`/home/khemoo/tmp_workspace/claude-issue-harness` (state under `state/`).

## Mission

Ship `worv.env.manure` as a production-grade native extension: barn-style cohesive manure
piles that a loader bucket scoops, pushes and dumps, rendered as deforming heaps, on the
default CPU-physics scenario. It is a NEW engine, deliberately separate from both
`worv.env.ground_cover`/`worv.env.terrain` (dynamic-LOD instancer engine) and
`worv.env.manure_chunks` (rigid-chunk prototype). It must not depend on either, and must not
modify either beyond the catalog switch described in the issues.

Design (decided, do not re-litigate; tracking issue #647 holds the rationale):
- Each pile is a 2.5D cohesive volume field: a grid of 5 cm columns over the pile's floor
  polygon plus margin, per column `height`, `moisture`, `compaction`. Solver is plain C++ on
  the CPU (a few thousand cells per pile is microseconds of work); a GPU kernel is NOT part of
  this mission.
- Cohesion rule: a column sheds volume to a neighbour only when the slope exceeds the angle of
  repose AND the unsupported face exceeds a critical height (`critical_face_height_m`). This
  yields heaped piles, a clean bite face after a scoop, slow slumping of over-steep faces.
- Bucket interaction is a sweep, never a collision: each tick read the bucket link's Fabric
  world matrix; model the bucket as floor plane, back wall, lip edge in bucket-local frame
  derived once at build time from the link's collision proxy bounds. Material above the floor
  plane and behind the lip while the lip advances into material → bucket load (scalar volume
  + centre of mass). Lip at ground and advancing → pushed into the columns ahead (bow wave).
  Bucket pitch past the dump angle → load released at the lip into the field.
- Rendering: one Fabric-updated `Mesh` per pile (points + normals written through usdrt each
  tick, authored once on the cold path in a pxr-only TU). A second small mesh under the
  bucket link morphs with the load fraction.
- Coupling: carried mass + scoop resistance through the Fabric `_worldForce`/`_worldTorque`
  attributes physx-fabric applies to mirrored bodies — VERIFY that channel with a controlled
  experiment before relying on it (issue-scoped).
- Wire contract stays the shipped one: `/manure/distribution` (`std_msgs/String` JSON, spec
  shape documented in `extensions/env/worv.env.manure_chunks/docs/README.md`) extended with
  optional material keys; every received spec rebuilds piles from scratch; empty `piles`
  clears. New `/manure/state` JSON publication is part of the mission.
- PhysX PBD particles are EXCLUDED (GPU-only; the scene flip costs 14.8→64.6 ms/tick on
  s76_v2 before any particle exists — measured 2026-09-02, artifacts in
  `~/tmp_workspace/artifacts/manure-pbd-probe-2026-09-02/`).

### Targets (each with its measurement)

- **M1 — build gate**: `tools/dev/iter.sh build worv.env.manure` green in a worktree AND the
  doctest binary (`extensions/env/worv.env.manure/bin/tests/test_*`) exits 0 when run inside
  the builder image. Final tick: full `docker compose build` green from the ai/manure
  worktree (doctest gate, extension version gate, settings parity, build-native checks).
- **M2 — behaviour gate** (robot `s76`, `WORV_ENVIRONMENT=TestPlane_bigtrimesh_ai414`, CPU
  physics = default, headless simrobot): publish the reference spec below, wait for
  `manure: built 1 piles`, drive `/cmd_motion` Joy axes `0,14.25,0,0,100` for 6 s. PASS when
  `/manure/state` reports pile volume down ≥ 10 % from the built value and bucket load
  ≥ 0.05 m³, the pile mesh points in usdrt changed, and one `rgb_front_simside` frame
  archived under `~/tmp_workspace/artifacts/manure-harness/<issue>/` shows the pile.
  Reference spec:
  `{"version": 1, "frame_id": "world", "piles": [{"polygon": [[2.5, -0.5], [3.3, -0.5], [3.3, 0.5], [2.5, 0.5]], "height": 0.5, "size": 0.15, "shape": "cube"}], "material": {"angle_of_repose_deg": 42.0, "critical_face_height_m": 0.3, "density_kg_m3": 700.0, "moisture": 0.7}}`
  (`size`/`shape` are the chunk-era keys; the new runtime must accept and ignore unknown
  keys it does not use, so old publishers keep working).
- **M3 — perf gate**: added tick wall p50 ≤ 2 ms and p99 ≤ 4 ms with 4 reference piles vs a
  no-manure baseline, same config as M2, `worv.core.profiling` JSONL reduced with
  `tools/test/reduce_jsonl.py` over the last 1500 ticks, robot idle then driving. Both
  numbers in the PR body; no GPU physics.
- **M4 — no regression**: `worv.env.manure_chunks` sources untouched; existing test tools
  under `tools/test/` still pass; kit files keep settings parity.

## Priority — fix, don't accumulate findings

- Claim the OLDEST open `ai` issue without `ai:wip` whose `Depends on:` issues are all
  closed (their PRs merged into `ai/manure`). If the oldest is blocked by dependencies, take
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
3. **Implement** — worktree: `git worktree add /home/khemoo/tmp_workspace/ai-worktrees/manure-<issue#> -b ai/<issue#>-<slug> origin/ai/manure`
   (after `git fetch`). If a later issue builds on an unmerged sibling PR, branch from that
   PR's branch instead and say so in the PR body. Follow the repo `CLAUDE.md` hard rules
   exactly (orchestrator phase, no per-tick pxr, pxr and usdrt never in one TU, settings via
   `ISettings`, `SettingResolve.h`, TOML config through `worv.core.config`, three-segment
   extension names, comments state constraints not history, no Co-Authored-By, conventional
   commits without parentheses).
   Reference implementations to copy patterns from (read, do not import):
   `extensions/env/worv.env.manure_chunks` (capability claim, subscriber, cold-path pxr
   authoring TU, version/changelog layout, doctest ConsoleApp premake project),
   `extensions/env/worv.env.climate` (native runtime + Fabric array writes,
   `FabricGpuArray.h`), `extensions/_template_worv_cpp/README.md` (new-extension checklist).
   Build: `tools/dev/iter.sh build worv.env.manure` from the WORKTREE root (plus any other
   ext you touched). Doctests: `docker run --rm -v <worktree>/extensions/env/worv.env.manure:/e:ro --entrypoint bash worv-builder:isaac6 -lc "/e/bin/tests/<binary>"`.
   Python gates: `python3 tools/test/test_extension_version_gate.py`,
   `python3 tools/test/test_kit_settings_parity.py`, `python3 tools/test/test_build_native_checks.py`.
4. **Verify** — run the target slice the issue names. Kit-level runs need the resource lease
   (below) and these steps from the worktree root: `cp /home/khemoo/tmp_workspace/isaac_sim/env/nucleus.env env/`;
   `tools/dev/iter.sh up worv.env.manure worv.robots.catalog <other touched native exts>`;
   `WORV_SIM_ROBOT=s76 WORV_ENVIRONMENT=TestPlane_bigtrimesh_ai414 tools/dev/iter.sh kit`;
   kit stdout is `/tmp/kit.log` INSIDE the `worv-iter` container. Publishers: the sidecar
   `worv-ros2cli` (created by `tools/dev/iter.sh topics`) runs
   `/usr/bin/python3` rclpy scripts — copy `pub_manure.py`/`pub_joy.py` from
   `~/tmp_workspace/artifacts/manure-instancer-diag-2026-09-02/` into it with `docker cp`,
   run as `source /opt/ros/jazzy/setup.bash && /usr/bin/python3 /tmp/pub_manure.py '<spec>' 4`.
   Boot to `scene bootstrap complete` takes ~40 s on TestPlane; poll the log in bounded loops.
   Known traps: `s76_v2` HEAD asset has DEAD boom/bucket cylinders (no drive target) — use
   `s76` for any gate needing bucket motion; a spec sent before `manure: subscribed` is not
   replayed; mounted ext dirs shadow baked `data/*.cubin` for climate/terrain (expected
   self-disable noise, not a regression); a robot starts at the origin facing +x.
   Archive evidence under `~/tmp_workspace/artifacts/manure-harness/<issue#>/` (logs, JSONL,
   frames) and quote the decisive lines in the PR body under `## Verification`. Scope
   honestly: "compile-verified only" when no live run backed a claim.
5. **Submit** — push the branch under the stack lock; `gh pr create --base ai/manure` with a
   conventional-commit title (no parentheses), body: what changed, `Fixes #<issue>`,
   `## Verification` with real evidence. Leave reviewers EMPTY.
6. **Land, by base branch** — PRs into `ai/manure` MAY be merged by the tick that opened them
   once M1 is green for the change and the issue's own gate passed (`gh pr merge --merge
   --delete-branch`, under the stack lock, re-checking mergeability immediately before).
   Never merge into `master`; the final integration PR `ai/manure → master` is opened by the
   release-readiness issue and stays open for the human. Never merge a PR you did not open.
   After merging: remove `ai:wip`, close the issue with a one-line summary, remove the
   worktree (`git worktree remove --force`; if root-owned build artifacts block it, clean via
   the builder image: `docker run --rm -v <worktree>:/w --entrypoint bash worv-builder:isaac6 -lc "rm -rf /w/extensions/*/*/obj /w/extensions/*/*/bin"` then `git worktree prune`).
7. **Done condition** — when every issue is closed and the integration PR is open with M1–M4
   evidence, refresh the tracking issue checklist and end the tick with no changes.

## Resource lease — one GPU, one warm container

The warm `worv-iter` container, the `worv-ros2cli` sidecar and the GPU are ONE serialized
resource. Any `iter.sh up/kit/topics`, kit session, or perf measurement requires the lease:

- Acquire under `flock /home/khemoo/tmp_workspace/claude-issue-harness/state/locks/resource.lock`
  AND `flock /tmp/isaac-cppmig-gpu-runtime.lock` (a second system on this host honours that
  path): write `<issue#> <PID> <UTC timestamp>` to
  `/home/khemoo/tmp_workspace/claude-issue-harness/state/locks/resource.lease`, then
  release the flocks; the lease file marks ownership for the session. Write the lease ONLY
  inside the flock and ONLY after reading the current file and confirming its PID is dead or
  the file is absent — a bare write outside the flock has already clobbered a live tick's lease
  once. Recipe (single command, run it verbatim with your issue number; the PID is computed inside):
  `flock /home/khemoo/tmp_workspace/claude-issue-harness/state/locks/resource.lock -c 'L=/home/khemoo/tmp_workspace/claude-issue-harness/state/locks/resource.lease; if [ -s $L ] && kill -0 $(awk "{print \$2}" $L) 2>/dev/null; then echo BUSY: $(cat $L); exit 1; fi; echo "<issue> $(/home/khemoo/tmp_workspace/claude-issue-harness/bin/tick-pid.sh) $(date -u +%FT%TZ)" > $L; echo ACQUIRED'`
  Sidecar: `tools/dev/iter.sh topics` blocks on `ros2 topic hz`; create the sidecar under
  `timeout 120` and never call `topics` inside a polling loop.
- Before treating the resource as busy, read the lease and `kill -0` its PID. A dead owner
  is orphaned: `docker exec worv-iter pkill -f "^/isaac-sim/kit/kit"` (ignore errors),
  overwrite the lease, proceed. Never wait on a dead owner; never wait more than 20 min on a
  live one — prefer a claimable issue that needs no GPU.
- Preflight before evidence runs: `nvidia-smi --query-compute-apps=pid,process_name --format=csv,noheader`
  must show no other kit process; if it does, the GPU is contended — do not measure.
- `tools/dev/iter.sh down` and delete the lease when your session ends. Cap any session at
  45 minutes. Write your tick summary to stdout BEFORE starting a kit session and append
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
- Version discipline: `worv.env.manure` goes 0.1.0 → 0.2.0 on the first runtime PR, then
  patch bumps per PR that changes it; each bump needs a matching top `docs/CHANGELOG.md`
  section (the version gate rejects otherwise). `apps/*.kit` versions stay 1.0.0.
