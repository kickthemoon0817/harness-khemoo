# Lessons

Failure modes observed in a multi-day production run. Each one cost real time; the fixes are
already in `bin/tick.sh` and the runbook template, but they are written down because the
reasoning matters more than the patch.

## Ticks are finite batch runs — scheduling anything is fatal

A tick called a scheduling/wakeup tool to nap while a long job ran, then ended its turn. Under
`claude -p` the process exits when the turn ends: the wakeup never fired, and the job, its
lease, and its issue claim were all orphaned. The log was empty, so it looked like a crash.

**Fix:** the tick prompt states the finite-batch contract explicitly — no wakeups, no ending a
turn to wait, wait synchronously inside bounded polling loops, tear everything down before the
turn ends. `bin/tick.sh` also appends `exit=N duration=Ns` to every log so a dead tick is
distinguishable from a quiet one at a glance.

## Claims and leases must be liveness-based, not time-based

The first design expired stale claims after four hours. When a tick died holding the only GPU,
every other tick politely queued behind it for hours — a convoy behind a corpse.

**Fix:** ownership is held only while the owning PID is alive. Check `kill -0`; a dead owner's
lease is reclaimed immediately after cleaning up what it left running. Time-based expiry is
the fallback for records with no readable PID, never the primary test.

A related rule: a successor should **resume** a released claim from its recorded state (open
PR, posted evidence, pending verification) rather than restart the work. Recording state on
the issue as you go is what makes that possible.

## A serial resource does not mean serial ticks

One GPU meant only one tick could measure at a time. The naive conclusion — run one tick — was
wrong: implementation, porting, static analysis, and review need no exclusive resource.

**Fix:** the runbook tells ticks to prefer a claimable issue that does *not* need the resource
when the lease is held by a live tick. That is what makes an eight-wide pool useful behind a
one-wide bottleneck.

## Two agent systems on one machine will collide silently

A parallel agent session shared the same GPU and container. Neither knew about the other, and a
measurement run was quietly contaminated — the numbers looked fine and were wrong.

**Fix:** agree on one lock path across *all* systems that touch the resource, and require an
uncontended preflight check before any measurement that produces evidence. Cross-system
coordination cannot be inferred; it has to be declared in both runbooks.

## Cron interval is refill latency

With a 30-minute cron, a tick finishing at :14 left its slot idle until :43. Most "is it
stuck?" moments were just that gap.

**Fix:** make the launcher cheap enough to run every couple of minutes (exit before any API
call when all slots are held; refresh usage only when the cache is stale; log nothing on
no-ops), then fire it often. Refill latency drops to about two minutes. Rate-limit the
empty-queue heartbeat tick separately, or a frequent launcher will spawn audits forever.

## Discovery outruns repair unless you forbid it

Given an open mandate, ticks filed findings far faster than they fixed them: 34 issues opened
against 10 closed in a day, a queue no pool could drain, and dozens of open PRs.

**Fix:** a fix-first priority block at the top of the runbook — land a fix, do not file a new
issue unless it blocks the fix in hand, note anything else in one sentence in the tick log. The
same pool then produced 34 merged PRs and zero new issues. Discovery mode is worth one phase,
not the whole run.

## Measure under load, not at rest

An idle measurement passed comfortably and was declared done. Under real load the same target
failed by 20% — the idle number never exercised the code paths that cost.

**Fix:** the mission section must state the exact configuration a target is measured under, and
"under load" must be part of it when the system has a load. Also require the gate to prove its
red direction: a threshold that has never failed is not known to work.

## Trust the flags, verify anyway

GitHub reported a PR `MERGEABLE/CLEAN` that broke a multi-PR merge sequence at step four,
because each merge shifts the base for the next one.

**Fix:** re-check mergeability immediately before each merge in a sequence, and simulate the
whole sequence before starting it. Never merge on another run's evidence.

## Naming a thing does not define it

A convention directory named `IgnoreCollision` was read as "these objects have no collider."
It actually meant "collision events with these objects are ignored" — the colliders were
load-bearing for terrain sampling. Acting on the misreading would have broken an unrelated
subsystem.

**Fix:** when a convention's meaning is inferred from a name, verify it against a consumer of
that convention before acting, and record the ruling somewhere ticks read. Semantics that live
only in a maintainer's head will be guessed wrong.

## The environment is part of the finding

A robot "would not move," and days of physics hypotheses followed. The real causes were
geography: it had spawned 2.5 m from a building and was pushing against it, and the fallback
test map was a 4 m tile it drove straight off.

**Fix:** before diving into a subsystem, verify the scenario itself — position, scale, bounds,
and preconditions. Cheap environment checks first, expensive theories second.
