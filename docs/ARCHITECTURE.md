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
3. Count held slot locks. If none are free, exit — no API calls at all.
4. Refresh each agent's usage reading only if it is older than `USAGE_REFRESH_AFTER`.
5. Map each agent's windows to its cap, and sum the caps into the pool's.
6. Ask GitHub how many labeled issues are unclaimed (`QUEUE_DEPTH_CMD`, e.g. `$HARNESS_BIN claimable`).
7. `spawn = clamp(allowed − busy, 0, unclaimed)`, plus a rate-limited heartbeat when the queue
   is empty.

### Runner (`tick.sh runner <max> <delay>`)

Sleeps for its stagger, takes the first free slot via `flock`, and runs an agent CLI (Claude
Code, Codex or Gemini) with the composed prompt on stdin: the lane line, `prompts/tick-core.md`,
then the instance's `prompts/tick.md`. If no slot is free it exits silently — losing the race is
a normal outcome, not an error.

The runner starts every tick with `HARNESS_TICK` in its environment, naming the state directory,
slot, agent, model and kind (work, lane or landing). A tick is the topmost process carrying it:
an agent's node shim and its native process, and every tool shell, inherit the variable, so all of
them resolve to the process the runner started. `harness ticks` counts live ticks this way, and
`libexec/tick-pid.sh` finds the tick a claim belongs to; a dispatched tick on a device carries
fleetd's `FLEET_SESSION` instead. Counting by marker rather than by command line keeps two
installations on one host, and an operator's own agent sessions, out of each other's counts.

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

## What is deliberately absent

**No daemon** — cron plus flock is more robust than a supervisor process that can itself die.
**No queue database** — GitHub already is one, and it is auditable.
**No inter-tick messaging** — ticks coordinate through issue labels; anything richer would
create states a human cannot inspect.
**No retry logic in the harness** — a failed tick just ends; the next firing re-derives what to
do. Retries hide problems that logs should surface. The one exception is a model's usage limit,
which says nothing about the work: that tick re-runs once per remaining entry, and the trailer
in its log names the agent and model each attempt used.
