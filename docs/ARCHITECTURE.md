# Architecture

## Design constraints

The harness works under four constraints, and every design choice follows from them:

1. **A tick can die at any moment** — network drop, rate limit, a crash. Nothing may be
   corrupted by a tick that never returns.
2. **Ticks run in parallel and cannot talk to each other.** Coordination has to happen through
   the filesystem and GitHub, not through shared memory.
3. **API budget is finite and shared with the human.** The harness must yield rather than
   starve its owner.
4. **All durable state must be human-inspectable.** If the only record of what happened is in
   a model's context, the run is unauditable.

## Components

### Launcher (`tick.sh`, no arguments)

Decides how many ticks should exist right now, then spawns that many runners. It is stateless:
every firing recomputes the answer from scratch, so a missed firing costs nothing and a
duplicate firing is harmless.

Ordered cheapest-first because it runs every couple of minutes:

1. Exit if the harness is paused (`state/paused`, or no live cron line) or the instance's
   `hooks/hold` says it is held.
2. Exit if the instance is incomplete (`docs/INSTANCE.md`), logging it at most hourly.
   With `SUPERVISOR=1`, send TERM to a stopped tick whose grace ended, start a supervisor tick
   when one is due, and exit inside the directive's pause or before its `wake_at`.
3. Count held slot locks. If none are free, exit — no API calls at all.
4. Refresh each agent's usage reading only if it is older than `USAGE_REFRESH_AFTER`.
5. Map each agent's windows to its cap, and sum the caps into the pool's. A directive's `width` lowers it.
   With every cap at 0 and a reset ahead, nothing starts before it; the launcher says so once.
6. Ask GitHub how many labeled issues are unclaimed (`QUEUE_DEPTH_CMD`, e.g. `$HARNESS_BIN claimable`).
7. `spawn = clamp(allowed − busy, 0, unclaimed)`, plus a rate-limited heartbeat when the queue
   is empty.

### Runner (`tick.sh runner <max> <delay>`)

Sleeps for its stagger, takes the first free slot via `flock`, and runs an agent CLI (Claude
Code, Codex or Gemini) with the composed prompt on stdin: the lane line, `prompts/tick-core.md`,
then the instance's `prompts/tick.md`. If no slot is free it exits silently — losing the race is
a normal outcome, not an error.

The runner starts every tick as its own child and records it in its slot's
`state/locks/slot<n>.tick`: pid, start time, agent, model and kind (work, lane or landing). A tick is
live while its pid has that start time, so a reused pid is never mistaken for one, and a slot holds
one tick at most, so a tool child that outlives its tick is never a second. `harness ticks` counts
live ticks from these files, and `libexec/tick-pid.sh` gives a claim to the first ancestor a slot
file records; on a device, to the process fleetd started. Both read only `/proc/<pid>/stat`, which
stays readable for an agent CLI that marks itself non-dumpable, as the release Codex CLI does. Two
installations on one host, and an operator's own agent sessions, stay out of each other's counts.

With `FLEET_DISPATCH=1` the runner hands the same prompt to `fleet dispatch --wait` on stdin
instead ([`docs/FLEET.md`](FLEET.md)). The slot, the model order, the cooldown and chaining stay
here: fleet only places the agent on a device and returns its output and exit status, so a tick
reads the same whichever device ran it.

A runner keeps its slot and chains: when a tick ends, it re-reads usage and the queue and starts
the next tick at once on a fresh context, so work finished early is followed immediately rather
than at the next cron firing. It stops when the queue is empty, usage no longer allows its slot, a
tick ended in under `CHAIN_MIN_TICK_S` (nothing claimable, or an error), after `MAX_CHAIN` ticks,
or when the harness is paused (cron line commented out, or `state/paused`) or held by `hooks/hold`.

Each tick runs on the best entry of the priority list (`state/tick-model`, else `TICK_MODELS`),
whose entries are `agent:model` across vendors, that is not cooling down and whose agent runs
fewer ticks than its usage windows allow. A tick that dies on its usage limit marks that entry in
`state/model-cooldown/` for `MODEL_COOLDOWN_S` and re-runs at once on the next entry, which may
be another agent's.

The stagger exists because two ticks starting in the same second will read the same unclaimed
issue list and claim the same issue. Staggering is cheaper and more robust than a distributed
lock for this.

### Supervisor (`tick.sh supervisor`, with `SUPERVISOR=1`)

An agent decides the harness's direction and rhythm; the script keeps every hard rule. The
supervisor is a tick, not a daemon: the launcher starts one when it is due, it reads one snapshot,
may apply a directive, and ends. Nothing must stay up, so the "no daemon" rule below stands, and a
supervisor that is absent, late, wrong or dead leaves the harness on its own formulas.

It is due when an event happened since its last run (a tick ended non-zero, a model hit its
limit, a usage window reset, the claimable count changed, a tick ran past the longest budget or the
instance logged a stall, one issue was claimed by three ticks in six hours, an operator's
`Operator` comment landed, the human lifted a pause, or the directive expires within ten minutes),
or when `SUPERVISOR_INTERVAL_S` passed. It never starts sooner than `SUPERVISOR_MIN_INTERVAL_S`
after its last start, waits that interval doubled for each failed run in a row, and starts only
while its agent has room for one more tick under its usage windows. At most one runs, under
`state/locks/supervisor.lock` and recorded in `state/locks/supervisor.tick`, outside the slots,
within `SUPERVISOR_BUDGET_MIN`. Its prompt is
`prompts/supervisor-core.md` and the instance's `prompts/supervisor.md`, then the output of
`harness health --json`: one bounded snapshot of usage, live ticks, recent tick outcomes, the
signals above, the queue, failing PRs of this repository's branches, this host, the directive and
the supervisor's own runs, with every string clipped and masked. An `Operator` comment counts only
when its author is in `OPERATOR_LOGINS`, else the repository's owner, a member or a collaborator:
anyone can comment on a public repository.

The supervisor acts through `harness directive apply`, and on GitHub as a tick does, every body
linted. A directive narrows or reorders what the configuration allows, never widens it:

| Field | Effect | Rail |
|---|---|---|
| `pause` | no tick starts and no runner chains until `until` | ends at the directive's expiry |
| `wake_at` | the launcher starts nothing until then | a wake event ends it early: a window reset, an operator's comment, the expiry nearing, the human lifting a pause |
| `width` | at most this many ticks; 0 starts nothing new, a landing tick included | clamped to the configured width, then to what the usage windows allow |
| `models` | the entries to try, in this order | a subset or reordering of the configured list; any other entry is refused |
| `focus`, `avoid` | a sentence on each tick's lane line | changes what a tick prefers, never what it may claim |
| `notes` | one line to the tick working an issue, through `libexec/usage-ok.sh` | shown once per tick |
| `stop` | TERM to the tick's own process group after `SUPERVISOR_STOP_GRACE_S`; its runner writes the trailer (`exit=143`) and chains as usual | only a live tick of this installation whose start time still matches and that leads its own group; any other is refused and logged |

The usage ceiling and caps, claims, the merge lock, the token's handling, the lint and the human's
pause stay in the script, and the human's pause (`state/paused`, the cron line, `hooks/hold`)
always beats a directive. With no directive, an expired one, or one that no longer validates
against the configuration (a model removed from the list, a hand edit), the launcher runs exactly
its own formulas; an expired or invalid directive is logged once. The directive is a plain file,
`state/directive.json`, and every change to it is a line in `state/logs/directive.log`.

### Slots

`state/locks/slotN.lock` held with a non-blocking `flock` for the lifetime of the tick
process. Slots give three properties for free: a hard concurrency bound, an accurate live-tick
count (count the held locks), and automatic release on death — the kernel drops the lock when
the process exits, so a crashed tick never leaks a slot.

### Allocator

Keeps every usage window of every agent under `USAGE_CEILING_PCT` (90 %), leaving the last
tenth of each for the human. `harness usage` reads each agent's windows from its own source:
Claude Code's usage cache, the rate limits Codex records in its session rollouts, a normalized
file any fetch script can write, or none, where only the usage-limit cooldown gates the agent.
Each window maps to its own cap and the agent's smallest one wins, so a healthy 5-hour window
cannot mask an exhausted weekly one. A window steps down to half width, then one slot, then none,
and stops well under the ceiling, because the ticks already running keep spending until they end:
a window under a day fills several times faster, so it stops at 80 % and a longer one at 88 %.
With no fresh reading an agent starts nothing. The pool's cap is the agents' caps summed, at most
`TICK_WIDTH`, so when one vendor's windows fill the next vendor's entries take the pool. The
ceiling itself is held inside the ticks: each runs `libexec/usage-ok.sh` for its own agent before
starting work and pauses two points under it, so its wind-down stays under it too.

Reading percentages rather than counting tokens keeps the harness honest about usage it did
not cause — the human's own sessions consume the same budget.

### Ownership by liveness

A lock held for a long session is fragile: if the holder dies, others cannot tell whether the
thing it guards is still in use. So the harness pairs a short `flock`, which serializes the
handoff, with a record of `issue PID timestamp` held for the session. `libexec/claim.sh` (one tick per
issue) and `libexec/merge-lock.sh` (one landing at a time) both work this way. Ownership is then testable
by anyone with `kill -0`, and a dead owner's record is taken over at once.

This is the single most important pattern in the harness: **ownership is proven by process
liveness, never by elapsed time.** The same rule governs the claim comments on issues, and it is
the rule to follow for any exclusive resource an instance manages (a device, a test host).

### Core and instance

The core knows nothing about the project. Its build, tests, devices and rules live in the
instance directory: `harness.env`, the installation's prompt, and optional hooks that the core calls
at fixed points (`hooks/env.sh` before ticks, `hooks/hold` before spawning or chaining,
`hooks/worktree-busy` and `hooks/worktree-remove` in worktree cleanup). Device policy, such as which
tick may use which machine and how a shared device is leased, belongs to the instance's prompt and
scripts. The dashboard shows it only through the status commands the instance configures.
`docs/INSTANCE.md` gives each contract.

### Issue queue as the work protocol

Issues are the queue, labels are the state machine (`ai` = queued, `ai:wip` = claimed), and PRs
are the output. Nothing about a run lives only in a tick's context: a claim is a label plus a
comment with a PID, progress is comments and evidence, results are PRs. A human can read the
entire history in the GitHub UI, and any tick can resume any other tick's work from it.

## Failure model

| Failure | Handling |
|---|---|
| Tick crashes | Kernel releases the slot; PID-liveness frees the claim and the merge lock |
| Network outage | Ticks exit with `exit=1`; cron retries; nothing corrupted |
| Rate limit exhausted | Allocator reaches 0 and stops spawning; in-flight ticks finish; chains stop at the next boundary |
| One model's limit reached | The tick re-runs on the next entry, which may be another agent's; that entry cools down; the launcher spawns nothing while every entry cools |
| Two ticks race one issue | Stagger plus label claim; the loser picks the next issue |
| Claim orphaned | The next tick sees a dead claimant PID and resumes from the recorded state |
| Something every tick needs is down | `hooks/hold` holds the harness; nothing spawns or chains until it recovers |
| Launcher missed | Stateless recompute; the next firing catches up |
| Supervisor crashes, hangs or errs | Its budget's timeout ends it; the run counts as failed and backs off; the directive in force runs to its expiry, then the defaults hold |
| Directive malformed or out of date | Ignored and logged once; the defaults hold |

What the design leaves open, knowingly:

- Two runners may both pick the last room of one agent in the moment between a pick and the
  slot file it leads to. The second tick runs past that agent's cap; if its windows are full, its
  limit message cools the entry as usual.
- An agent runs for a few milliseconds before its slot file exists, so a scan in that moment does
  not count it.
- A tool shell runs as the harness's user and could write a slot file of its own. No design that
  trusts files of one user can tell that from the runner's.
- The dashboard counts the lane's agent whenever `LANE_LABEL` is set, not only when the launcher
  wants a lane tick, so the pool it shows is an upper bound, and it says so.

## What is deliberately absent

**No daemon** — cron plus flock is more robust than a process that must stay up and can itself die;
the supervisor is one more tick that cron's launcher starts and that ends.
**No queue database** — GitHub already is one, and it is auditable.
**No inter-tick messaging** — ticks coordinate through issue labels; anything richer would
create states a human cannot inspect.
**No retry logic in the harness** — a failed tick just ends; the next firing re-derives what to
do. Retries hide problems that logs should surface. The one exception is a model's usage limit,
which says nothing about the work: that tick re-runs once per remaining entry, and the trailer
in its log names the agent and model each attempt used.
