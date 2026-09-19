# Manure Continuum Engine — design summary for harness ticks

The plan of record is roadmap revision 5 (2026-09-18), filed as issue #1104 with its research:
`~/tmp_workspace/artifacts/manure-literature/roadmap/manure-roadmap.html` (read it with
`sed -e 's/<[^>]*>//g'`), the programme and owner decisions in
`~/tmp_workspace/artifacts/manure-literature/deep-research/direction-of-work-2026-09-18.md`, and the
verified reports beside it. The older long form (`~/tmp_workspace/artifacts/manure-mpm-design/`) is
superseded wherever it differs. This summary binds ticks; where it and revision 5 disagree,
revision 5 wins and the tick records the contradiction on its issue.

## Plan of record: revision 5 (do not re-litigate)
- Architecture: the height field carries and drives the machine and holds resting material;
  particles exist only where material flows. Full MPM cannot hold one barn heap (2.3–18× the 200k
  ceiling). This is how Chrono SCM and Servin 2021 / AGX work. ADOPTED by the owner on 2026-09-19, together
  with decision 15 (the published wall law) and decisions 24–25 (StVK-Hencky and the overstress
  return); the rulings are on #1104. Items 1–12 and 14–16 are claimable. Item 13, the three-arm
  tool-zone experiment, waits on owner decision 11 (does the driving policy need in-bucket
  dynamics?): no tick starts it or builds an arm until that is ruled. Sub-decisions inside the
  adopted items follow #1104 comment A2's recommendations unless the owner overrides them.
- Five earlier decisions are REVERSED. Never build further on them:
  1. The Newton implicit-MPM port. The grid stays explicit; the constitutive fix is a consistent
     local solve (StVK-Hencky stress and return, a viscoplastic overstress return in place of the
     dashpot and its clamp, an implicit cap solve; items 14–16). No implicit grid (decision 14).
  2. The sticky-up-to-a-cohesion-scaled-shear-limit wall. The node law becomes the published
     separating condition: a crossing test instead of the dx/2 cubic band, no adhesion term, rubber
     rolling share 0, wall friction from Landry's measured μ_s(TS), stickiness only as a tensile
     limit c_b·A (items 7–9, decision 15). The particle snap is deleted, not tuned.
  3. "Low-pass over 3 substeps". It is a 3-tap boxcar over three PHYSICS steps (12.5 ms), fed a
     substep mean. It stays as it is until the three-arm tool-zone experiment (decision 13); call
     it by its real name.
  4. Conversion by radius over the union of link spheres. It stays as the running rule, but no
     tick extends it; the zone is later sized by reaction convergence (item 17).
  5. Speed-based rest (< 0.05 m/s for 2 s; `pose_speed_m_s`, max particle speed, moving-particle
     counts). Rest is displacement ≤ 0.1 dx over a window, everywhere (item 2, decision 8). Measured
     2026-09-18: the parked machine's links move 1–2 mm in 10 s while its speed field reads 0.03 m/s;
     the carried load moves 0.17 mm in 8 s while its particles read 0.18 m/s.
- Particle bearing is on the way out: the a-priori weight division puts the machine on a diagonal
  (18.8 / 17.5 / 1.3 / 0 kN measured) and is deleted in item 11, when the field carries and drives the
  machine as one image. The jitter-injection fix (zero velocity for still links) was measured harmful
  and must not return.
- Materials: two, calibrated separately — fibrous bedded pack and screened slurry (owner, 2026-09-17).
  Refits come last (item 21), never against a boundary artefact.

## Engine facts (as built; still true)
- Material: MLS-MPM continuum on a sparse grid. Ship target = single-phase family over
  `dry_matter_pct`: fixed-corotated elastic + Drucker-Prager with cohesion + Herschel-Bulkley/Bingham
  viscoplastic term. Stackable (>20 % DM) heaps; semi-solid (10–20 %) smears; slurry (<10 %) flows to a
  yield thickness of 1–3 cm. Two-phase (solid+water) is a later upgrade, not in scope.
- Solver: explicit MLS-MPM on the device with a CPU reference the GPU step must match on fixed seeds.
  Newton's implicit MPM is a read-only reference for local-solve patterns, not a port target. Kernels are Warp sources compiled AOT to
  cubins by `worv.core.warp_compat/tools/aot_compile.py`; launched from C++ via `CuApi`; no Python,
  no Warp runtime at tick time; own dense-block sparse grid (8³ blocks), no `HashGrid`/`BVH`.
- Machine: PhysX CPU at the robot's `physics_hz` (s76: 240). No second PhysicsScene, no GPU
  dynamics, compute contract (#440/#449/#630) untouched.
- Coupling: every collision-bearing link is a contact link (discovered; `contact_links_exclude`
  opt-out; per-link `surface` class steel|rubber|...). Boundary query per link: exact convex proxies
  by default, baked voxel SDF only for trimesh-only links, BVH mesh queries only in calibration mode.
  As built, the node condition blends nodes within dx/2 of a link with adhesion and a rolling share,
  and particles are snapped out of links; revision 5 replaces all three (Plan of record, item 2).
  Force/torque per link = momentum removed at boundary nodes, a 3-tap boxcar over three physics
  steps, clamped per link, applied via `omni::physx::IPhysx` (`getPhysXPtr` → `PxArticulationLink::addForce/addTorque`)
  inside the physics pre-step subscription. Default mode is ASYNC: forces held across
  `physics_steps_per_solver_step` substeps; sync per-substep mode kept for calibration.
- Native surface: PhysX SDK headers + `omni/physx/IPhysx.h` fetched pinned at Docker build (tag nearest
  omni.physx 110.1.13, commit-pinned, pattern of PR #290's review) into `/isaac-sim/vendor/physx/include`,
  consumed by a `with_physx` premake flag. Readback validation of one known link before the first force;
  mismatch → coupling disables itself with one error.
- LOD: the `HeapField` on ai/manure stays the far representation. Conversion field→particles inside
  the work radius (3 m default, follows the union of contact-link bounding spheres) OR, for links with a
  `cutting_edge`, by the Fundamental-Equation-of-Earthmoving failure wedge once the FEE reaction (applied
  to the link while the field is solid) reaches the failure force. Particles→field when at rest (<0.05 m/s
  for 2 s; rev 5 item 2 replaces this with displacement) beyond the settle radius (4.2 m). Never convert material inside a link, airborne, or moving.
  One ledger: field + particles + carried = built ± 1e-6, checked every tick, error on drift.
  Field gains a dry-matter-keyed rule: stackable = repose + critical face; wetter = shallow viscous flow
  to the yield thickness.
- Stepping: no own clock. MEASURED DECISION (#674, PR #687, recorded on #669):
  `physics_steps_per_solver_step = 1` is the mission default — the pre-step span is 0.054 ms p50 /
  0.073 ms p99 against the 4 ms budget because the substep only enqueues onto a private stream, so the
  async hold buys nothing and costs a stale force. `solver_steps_per_physics_step` follows the Courant
  split (3 substeps at 240 Hz / 0.05 m / 25 % DM), capped by `solver_steps_max`.
  THE BINDING CONSTRAINT IS THE PARTICLE COUNT, NOT THE RATIO: measured on #674, 5 ms of added tick
  wall is reached near 62 000 active particles (16.2 ms added at 195 112). The 200k `particle_budget` is a hard
  ceiling, never an operating point — the LOD degrade order must shrink the work radius so the active
  set stays near 60k during a scoop, and #676 owns that target.
  Superseded: `solver_steps_per_physics_step` (default 1) and
  `physics_steps_per_solver_step` (default 1) — exactly one may exceed 1, audit rejects
  both; `solver_steps_max` (4) caps the automatic Courant split; tick-rate work paced by `render_hz`,
  `surface_sync_every` (1). Lockstep: solver takes exactly the substeps the run mode drives.
- Grid: `grid_cell_m` default by DM (0.05 stackable / 0.035 semi-solid / 0.025 slurry), overridable per
  pile. CORRECTED 2026-09-04 (ruling on #679): the 0.025 m slurry cell CANNOT resolve the 1–3 cm
  slurry film — a yield stress arrests a layer through shear across its own thickness, so a film
  needs ~2 cells and 0.0195 m of film on 0.025 m cells is 0.78. Film physics is gated at BENCH scale
  (6 L deposit, cell from `resolvedFilmCellM`); production-scale slurry is gated qualitatively only
  (it flows, it does not hold a face). Do not re-derive a film check at the production cell; `particles_per_cell` 8 (4–27); `quality` preset fast|balanced|fine; validated at configure
  against boundary-query error and Courant margin. Sparsity derived, never a knob; ceilings
  `particle_budget` (200k), `grid_block_budget`, `gpu_memory_mb` (512); degrade order = shrink work
  radius → coarsen cell for that pile → refuse conversion, each logged once.
- Device: `[manure] device = auto` follows `CudaSettings.h` (physics device, else ordinal 0); ordinal
  or UUID allowed; per-arch cubin validated; cross-device render copy (P2P, host fallback) when the
  solver is not on the render GPU.
- Rendering: marching cubes over grid density → Fabric `Mesh` per active zone (dirty blocks only),
  spray point-instancer for airborne material, field meshes as today; moisture → roughness/albedo.
- Contract: `/manure/distribution` unchanged + optional `material.mpm` (`dry_matter_pct` primary; modulus,
  friction angle, cohesion, viscosity, yield stress, hardening overrides). `/manure/state` grows
  `particles`, `active_blocks`, `cells`, `gpu_mb`, `device`, effective solver rate, Courant margin,
  `active_piles`, per-link `carried_m3` and `coupling_n`.
- Retired from ai/manure: bucket sweep, in-bucket morph mesh, rigid clumps, `_worldForce` write.
  Kept: capability claim, contracts, HeapField + mesh, wheel compaction on the field, gate + perf
  scripts, docs layout. Read-only: manure_chunks, ground_cover, terrain, bootstrap/scene compute code.

## Budgets (targets to measure, RTX 5080, s76, TestPlane, CPU physics 240 Hz, render 20 Hz)
- Solver step 200k particles / 30k cells ≤ 0.30 ms; twelve substeps incl. upload/readback ≤ 4.0 ms.
- Surface mesh ≤ 0.8 ms per moved tick; LOD+ledger+state ≤ 0.3 ms.
- M3, ONE BAR: added tick wall p50 ≤ 18 ms / p99 ≤ 28 ms scooping, as `tools/test/manure_perf_ab/report.py` enforces since PR #1066 (the 16/26 of 2026-09-04 and the 5/8 before it are retired). Read once per image, never per PR. Pre-step span: reported; a 2 ms ceiling is proposed (owner decision 6, pending). Measured on the 2026-09-04 ruling (#710's closing comment): scooping at ~60k active particles (measured 13.86/21.53), p50 ≤ 15 ms idle (measured 12.40), GPU ≤ 512 MB (measured 55.8). HARD CEILING and the real bar: stay below the +49.8 ms the replaced PBD path costs for its GPU scene flip alone. Optimisation is DEFERRED to #751 (pipelining, P2G gather, smaller active set) and is not required to ship. The old 5/8 ms figures were set before measurement and no longer stand.
- Conversion of a 0.7 m³ pile ≤ 20 ms largest tick; GPU memory ≤ 512 MB.

## Rulings (2026-09-03, design owner; bind over issue text)
- Deterministic mode (#678): the `<= 1.3` cost ratio against the atomic path is WITHDRAWN — it was
  arbitrary. Gate on absolute cost only: deterministic mode must keep the added tick wall inside M3
  (18/28 ms) at ~60k active particles; report both costs as evidence. Fixed-point
  (scaled int64/int32) atomics are permitted and PREFERRED over a sorted-bin gather, with the scale
  factor and the overflow-range argument documented next to the kernel.
- Particle budget (#674 spike): 5 ms of added tick wall is reached near 62k active particles; 200k is
  a hard ceiling, never an operating point. The LOD degrade order sizes the work radius to hold ~60k.
- AOT gotcha: Warp treats an artifact already at its output path as a cache hit, so re-AOT in a
  worktree can ship a stale cubin against new kernel signatures (`cuLaunchKernel: invalid argument`).
  `rm -f data/*.cubin data/*.ptx` before the AOT step until worv.core.warp_compat 0.2.3 lands.

- M3 ruling (2026-09-04, project owner): the bar is the alternative this engine replaced, not an assumed fraction of a tick. PBD costs +49.8 ms for its scene flip before any particle and couples one way only; this engine costs +13.86 ms scooping at 60k particles with two-way coupling and CPU physics preserved. Ship at measured cost; optimise later (#751). Do not re-open the performance question against the retired 5 ms number.

- Slurry ruling (2026-09-04, #679): a resting yield-stress deposit is a DOME — its crest stands above
  the film; the film is what the rim thins to. Gate the rim (or the slump relation between volume,
  yield stress and spread radius), never the crest against the film. Measured: 0.78 film cells cannot
  arrest, 2.0 holds 0.0242 m, 3.1 holds 0.0436 m, and a 6.25-cell control arrests at 0.152 m against a
  0.156 m film.

- Flicker ruling (2026-09-04, #775): a flicker verdict is taken only between frames whose shading-step
  shares agree within MANURE_GATE_FRAME_MAX_STEP_DELTA (default 0.02); ineligible pairs are reported and
  skipped, and a burst with fewer than two eligible pairs FAILS. The 0.05 ceiling stands. The iso
  hysteresis of #775's item 1 is inert by measurement (area swing 0.01710 banded vs 0.01728 unbanded) —
  a crossing is placed at the nominal level whatever side its corners count on; do not re-propose it.

## Standing rules from mission 3 (2026-09-06) that still bind every tick
- The 2026-09-06 review (~/tmp_workspace/artifacts/manure-review-2026-09-06.html; read it before claiming)
  found what MEASUREMENT cannot: teardown races, model errors identical on both sides of a parity test,
  tests that skip and score as passes, config that validates but never reaches the solver. Your fix must
  therefore be proven by a test that FAILS ON THE CURRENT HEAD and passes after — a live gate that already
  passed on the buggy head is not evidence for a review fix. State in the PR which test failed before.
- A "gate passes" claim must name what the gate MEASURES. Nine gates on this branch were found passing on
  the wrong condition. If you touch a verdict, add the fixture that fails on the old behaviour.
- A documented [manure] key must be grep-able in the runtime that consumes it. Do not add or keep a knob
  the runtime cannot see (#789 is the cleanup; do not make it worse).
- Physics: yield parameters are EFFECTIVE (Drucker-Prager on Hencky stress paired with fixed-corotated
  elasticity, a mismatch of +26 % / −17 % on the deviator at 30 % stretch). The pairing changes only in
  programme item 14 (StVK-Hencky, owner decision 24), never inside another issue's PR.
- PR #768 stays OPEN and UNMERGED. #795-class re-release rewrites its evidence; it is human-merged only.
