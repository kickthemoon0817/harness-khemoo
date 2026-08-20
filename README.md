# claude-issue-harness

An autonomous loop that works a GitHub issue queue with Claude Code: it claims a labeled
issue, implements a fix in an isolated worktree, verifies it, opens a PR with real evidence,
and either lands it on a scratch branch or leaves it for human review. Concurrency is sized
from live API rate-limit headroom and queue depth, so it widens when you have budget and
throttles itself to zero when you do not.

It is a few hundred lines of bash around `claude -p` plus a runbook the ticks read. There is no
daemon, no database, and no server: cron fires a launcher, the launcher spawns runners, and all
durable state lives in GitHub issues and PRs where you can see it.

## How it works

```
cron (*/2)  ->  bin/tick.sh (launcher)  ->  bin/tick.sh runner  ->  claude -p
                      |                            |
                      |                            +- claims one issue, fixes it, opens a PR
                      +- decides HOW MANY runners to spawn:
                         min(usage-based cap - busy slots, unclaimed issues)
```

**Launcher.** Runs every couple of minutes and is ordered cheapest-first: it exits immediately
if every slot is held, refreshes usage telemetry only when the cache is stale, then reads the
queue. Most firings cost nothing and log nothing.

**Slots.** Concurrency is bounded by `flock`ed slot files. A runner that cannot take a slot
exits quietly, so spawning is racy-safe by construction.

**Allocator.** The worse of your 5-hour and weekly utilization maps to a cap:
`<55%: MAX_SLOTS`, `<70%: 6`, `<80%: 4`, `<85%: 3`, `<90%: 2`, `<95%: 1`, else `0`. With no
usage data it runs capped at 3 rather than blind at full width. Decisions are appended to
`state/logs/allocator.log`.

**Ticks.** Each runner is one `claude -p` invocation that reads `prompts/tick-prompt.md`, which
points at `prompts/runbook.md`. The runbook is where your project's rules live — the harness
itself knows nothing about your build, your tests, or your targets.

## Quickstart

```bash
git clone <this repo> ~/harness && cd ~/harness
cp config/harness.env.example config/harness.env   # set TARGET_REPO at minimum
cp prompts/runbook.template.md   prompts/runbook.md
cp prompts/tick-prompt.template.md prompts/tick-prompt.md
# fill in the <PLACEHOLDERS> in both prompt files — the runbook is the real work

gh label create ai     --description "Queued for the autonomous harness"
gh label create ai:wip --description "Claimed by a running tick"

./bin/tick.sh            # one manual firing; check state/logs/
crontab -e               # */2 * * * * /home/you/harness/bin/tick.sh
```

Label an issue `ai` and the next firing picks it up. Stop everything with `crontab -r`;
in-flight ticks finish on their own.

## Requirements

- `claude` (Claude Code) on PATH, authenticated
- `gh` authenticated with write access to the target repo
- `flock`, `setsid` (util-linux), bash 4+
- Optional: a usage-telemetry script writing `{timestamp, fiveHourPercent, weeklyPercent}`
  JSON. Without it the harness runs, capped at 3 slots.

## Layout

| Path | Purpose |
|---|---|
| `bin/tick.sh` | launcher + runner; the whole engine |
| `config/harness.env` | your paths, labels, and pool sizing (gitignored) |
| `prompts/runbook.md` | the rules ticks follow — mission, procedure, rails |
| `prompts/tick-prompt.md` | what a single tick is told to do |
| `state/logs/` | one log per tick, plus `allocator.log` |
| `state/locks/` | slot locks, resource lease, heartbeat stamp |

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — why the design is shaped this way
- [`docs/OPERATIONS.md`](docs/OPERATIONS.md) — running it, reading the logs, recovery
- [`docs/LESSONS.md`](docs/LESSONS.md) — failure modes found in production, and their fixes

Read `LESSONS.md` before your first long run. Every entry in it cost real time to discover.

## Safety posture

Ticks run unattended with permission prompts disabled, so the guardrails are in the runbook,
not in the tool layer. The defaults that matter:

- only labeled issues are ever worked;
- work happens in worktrees, never in your checkout;
- merging is scoped by base branch — the scratch branch may be landed by a tick, protected and
  shared branches are human-approval-only;
- no force-push, no reviewer assignment, no touching branches a tick did not create;
- evidence is archived honestly and gates are never weakened to obtain a pass.

Keep those in your runbook. They are the difference between an assistant and an incident.
