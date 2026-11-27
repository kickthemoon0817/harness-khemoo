# Glossary — worv.env.manure, for the public record

The phrases a body uses the first time it names one of these. A term is defined where it is used,
in the body, because the reader has the repository and not this file. Repository paths are on
`ai/manure-mpm` of MaumAI-Company/isaac_sim unless said otherwise.

## The project

| Term | Write it as |
|---|---|
| `ai/manure-mpm` | the branch the continuum engine is developed on in MaumAI-Company/isaac_sim |
| `worv.env.manure` | the Isaac Sim extension that holds the engine, under `extensions/env/worv.env.manure/`; its version is in `config/extension.toml` and each landing bumps it with a `docs/CHANGELOG.md` section |
| the mirror | MaumAI-Company/worv.env.manure, the extension's own repository: `main` is the extension's history on `ai/manure-mpm`, one tag `v<version>` per landed version, a release per minor version |
| the plan | `docs/manure-plan/` in the repository: the rules, decisions, work items and investigations the engine is built to |
| `PRINCIPLE.md` | the plan's rules: engine principles P1–P10 and validation principles V1–V5 (and V2a) |
| P1 | the engine names no body role: no bucket, wheel or cutting edge in the engine's code |
| P4 | conservation is exact: the ledger's drift over a run is zero |
| P8 | correct first, then fast |
| P9 | no free constants: every constant is MATERIAL, DERIVED, SHARED RULE or SETTING |
| P10 | models are interchangeable behind interfaces |
| V1 | a human signs off behaviour: no automated verdict accepts a behaviour change |
| V2 | every behaviour change carries an evidence packet |
| V3 | automated checks are diagnostics, not verdicts |
| V4 | every number is checked against its data before it is written |
| V5 | the rendered result is looked at: an agent that did not write the change inspects the frames before a human does |
| D<n> | an owner decision recorded in `docs/manure-plan/DECISIONS.md`, cited with what it decided: "D19, the first version's release bar" |
| T<nn> | a plan work item, `docs/manure-plan/todo/T<nn>-*.md`, cited with its title: "T01, evidence and review" |
| a plan step | one step of a work item's Steps section: one issue, one PR |
| the first version | the release the owner reviews against D19's three bars: bar 1 the physics (fixtures, scenarios, visual check), bar 2 the resting surface (nothing moves at rest), bar 3 the cost against PhysX particles (PBD) |
| D23, the deferred list | what the first version ships without, each item with its issue and size |

## Changes and proofs

| Term | Write it as |
|---|---|
| class A | a change that alters no engine behaviour (tools, tests, docs, or a refactor proven identical); it lands on its build, its fixture and its touched tests |
| class B | a change that alters behaviour; it lands only after a human signs off its evidence packet (V1) |
| the evidence packet | the folder `docs/manure-plan/evidence/<date>-<issue>-<slug>/` whose `INDEX.md` names every file with the run, the commit, the scenario, the camera, the moment and what it shows (V2); images and videos stay out of git and the INDEX.md says so |
| a fail-on-head fixture | the test case written for the change, quoted failing on the unchanged tree and passing with the change |
| the fast set | the extension's doctest suites without the `[slow]` cases: `tools/dev/iter.sh build --test --fast worv.env.manure` |
| the whole set | every doctest case, `[slow]` included: `tools/dev/iter.sh build --test worv.env.manure`; device cases need `--device` and a GPU |
| an identity proof | a deterministic replay of each review scenario on the unchanged tree and on the change, whose state documents, particle hashes and coupling traces must match byte for byte (`docs/manure-plan/ARCHITECTURE.md`, "F.3 Refactor or new behaviour") |
| the unchanged tree, the base | the commit the change is compared against: the branch head it was cut from or last merged |
| the head | the branch's current tip |
| a deterministic replay | the scenario runner's lockstep mode with a fixed seed and the deterministic transfer, so the same tree reproduces the same run |
| the state document | the JSON the engine publishes on `/manure/state` each window: the extension README's "Wire contract" |
| the coupling trace | the per-step force and torque the engine applies to each body, recorded by the scenario runner |
| tick cost, tick wall | the wall-clock cost of one engine update per physics step, in ms, with the percentile and the window it was read over |
| a sign-off | the operator's (or owner's) acceptance of a class B packet, posted on the issue |
| the visual check | V5's inspection of the frames against the checklist in `docs/manure-plan/todo/T01-evidence-and-review.md`, by an agent that did not write the change |
| the gate scripts | `tools/test/manure_gate.sh` and its siblings, documented in `docs/manure_live_gates.md`; diagnostics, never acceptance (V3) |

## Review scenarios (T01)

| Term | Write it as |
|---|---|
| RS1 | the heap alone: a barn plateau with no body within 1 m for 120 s; material at rest stays at rest |
| RS2 | a body on the heap: a rigid box dropped onto the plateau and left 60 s; it sinks, is borne, and leaves a rut with raised edges |
| RS3 | drive onto the heap and park: the loader drives 10 s and parks 60 s; wheels on the surface, ruts, untouched regions unchanged |
| RS4 | scoop, carry, dump: the full cycle with a breakout moment; the load stays in the bucket while carried, the dump releases it and the deposit comes to rest |
| RS5 | slurry spread: slurry spreads to a film at its yield thickness |
| RS6 | the pushed crate: the loader pushes a free crate into the heap; a body with no role interacts like the machine's links |
| a moment | a step-locked capture point in a scenario: M0 after the build, M1 after contact, M2 at the end of the rest window; a step may name more |
| a seed | the scenario's random seed; a scenario read over several seeds names each one |

## Harness words that stay out of the record

tick, slot, lease, lane, line, merge slot, merge lock, whole card, card alone, local or remote
card, worktree, cubin hash, budget, "(mine)", "the next tick", the harness's file names and its
state. Say the condition they imply, or nothing.
