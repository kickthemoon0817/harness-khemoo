# Manure Continuum Engine — design summary for harness ticks

Authoritative long form: `~/tmp_workspace/artifacts/manure-mpm-design/manure-mpm-design.html`
(read it with `sed -e 's/<[^>]*>//g'` if you need the prose). This summary is binding where the
two differ in detail; the page carries the rationale.

## Decisions (do not re-litigate)
- Material: MLS-MPM continuum on a sparse grid. Ship target = single-phase family over
  `dry_matter_pct`: fixed-corotated elastic + Drucker-Prager with cohesion + Herschel-Bulkley/Bingham
  viscoplastic term. Stackable (>20 % DM) heaps; semi-solid (10–20 %) smears; slurry (<10 %) flows to a
  yield thickness of 1–3 cm. Two-phase (solid+water) is a later upgrade, not in scope.
- Solver source: port NVIDIA Newton's implicit MPM (Warp, Apache-2.0) for the material step, replacing
  its rigid side with PhysX links and its colliders with our boundary query; keep an explicit MLS-MPM
  CPU reference that the GPU step must match on fixed seeds. Kernels are Warp sources compiled AOT to
  cubins by `worv.core.warp_compat/tools/aot_compile.py`; launched from C++ via `CuApi`; no Python,
  no Warp runtime at tick time; own dense-block sparse grid (8³ blocks), no `HashGrid`/`BVH`.
- Machine: PhysX CPU at the robot's `physics_hz` (s76: 240). No second PhysicsScene, no GPU
  dynamics, compute contract (#440/#449/#630) untouched.
- Coupling: every collision-bearing link is a contact link (discovered; `contact_links_exclude`
  opt-out; per-link `surface` class steel|rubber|...). Boundary query per link: exact convex proxies
  by default, baked voxel SDF only for trimesh-only links, BVH mesh queries only in calibration mode.
  Boundary condition at grid nodes (sticky up to a cohesion-scaled shear limit, slip above); per-particle
  projection out of links always on; CPIC treatment for links annotated `cutting_edge` if needed.
  Force/torque per link = momentum removed at boundary nodes, low-pass over 3 substeps, clamped per
  link, applied via `omni::physx::IPhysx` (`getPhysXPtr` → `PxArticulationLink::addForce/addTorque`)
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
  for 2 s) beyond the settle radius (4.2 m). Never convert material inside a link, airborne, or moving.
  One ledger: field + particles + carried = built ± 1e-6, checked every tick, error on drift.
  Field gains a dry-matter-keyed rule: stackable = repose + critical face; wetter = shallow viscous flow
  to the yield thickness.
- Stepping: no own clock. MEASURED DECISION (#674, PR #687, recorded on #669):
  `physics_steps_per_solver_step = 1` is the mission default — the pre-step span is 0.054 ms p50 /
  0.073 ms p99 against the 4 ms budget because the substep only enqueues onto a private stream, so the
  async hold buys nothing and costs a stale force. `solver_steps_per_physics_step` follows the Courant
  split (3 substeps at 240 Hz / 0.05 m / 25 % DM), capped by `solver_steps_max`.
  THE BINDING CONSTRAINT IS THE PARTICLE COUNT, NOT THE RATIO: the 5 ms added-tick-wall budget is
  reached near 62 000 active particles (16.2 ms added at 195 112). The 200k `particle_budget` is a hard
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
- M3 RESTATED 2026-09-04 by the project owner (ruling in #710's closing comment): added tick wall p50 ≤ 16 ms / p99 ≤ 26 ms scooping at ~60k active particles (measured 13.86/21.53), p50 ≤ 15 ms idle (measured 12.40), GPU ≤ 512 MB (measured 55.8). HARD CEILING and the real bar: stay below the +49.8 ms the replaced PBD path costs for its GPU scene flip alone. Optimisation is DEFERRED to #751 (pipelining, P2G gather, smaller active set) and is not required to ship. The old 5/8 ms figures were set before measurement and no longer stand.
- Conversion of a 0.7 m³ pile ≤ 20 ms largest tick; GPU memory ≤ 512 MB.

## Rulings (2026-09-03, design owner; bind over issue text)
- Deterministic mode (#678): the `<= 1.3` cost ratio against the atomic path is WITHDRAWN — it was
  arbitrary. Gate on absolute cost only: deterministic mode must keep the added tick wall inside M3
  (p50 <= 5 ms, p99 <= 8 ms) at ~60k active particles; report both costs as evidence. Fixed-point
  (scaled int64/int32) atomics are permitted and PREFERRED over a sorted-bin gather, with the scale
  factor and the overflow-range argument documented next to the kernel.
- Particle budget (#674 spike): 5 ms added tick wall is reached near 62k active particles; 200k is a
  hard ceiling, never an operating point. The LOD degrade order sizes the work radius to hold ~60k.
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

## Mission 3 (2026-09-06): review fixes on ai/manure-mpm — rules that bind every tick
- The 2026-09-06 review (~/tmp_workspace/artifacts/manure-review-2026-09-06.html; read it before claiming)
  found what MEASUREMENT cannot: teardown races, model errors identical on both sides of a parity test,
  tests that skip and score as passes, config that validates but never reaches the solver. Your fix must
  therefore be proven by a test that FAILS ON THE CURRENT HEAD and passes after — a live gate that already
  passed on the buggy head is not evidence for a review fix. State in the PR which test failed before.
- Two blockers (#785 use-after-free, #786 lock-contended substep) are the mission's critical path; prefer
  them when claimable. Every other fix issue is independent and may run in parallel.
- A "gate passes" claim must name what the gate MEASURES. Nine gates on this branch were found passing on
  the wrong condition. If you touch a verdict, add the fixture that fails on the old behaviour.
- A documented [manure] key must be grep-able in the runtime that consumes it. Do not add or keep a knob
  the runtime cannot see (#789 is the cleanup; do not make it worse).
- Physics: yield parameters are EFFECTIVE (Drucker-Prager on Hencky stress paired with fixed-corotated
  elasticity). Do not "fix" the pairing in a fix-issue PR — #793 documents and pins it; changing the
  elasticity is a design decision for #682 calibration.
- PR #768 stays OPEN and UNMERGED. #795-class re-release rewrites its evidence; it is human-merged only.
