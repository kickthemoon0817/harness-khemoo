# Autonomous GitHub issue harness for Claude Code

The harness works a GitHub issue queue on its own. A tick claims one labelled issue, fixes it
in its own worktree, verifies the fix, and opens a pull request that carries the evidence. It
either lands the PR or leaves it for review. Cron fires a bash launcher every couple of minutes.
The launcher sizes the pool from your Claude usage and the queue depth, and each tick is one
`claude -p` run. Nothing runs as a daemon and there is no database: all durable state lives in
the issues and PRs. The core knows nothing about your project. Your build, tests, rules and
infrastructure live in an instance directory that you own.

## How a tick works

- Cron runs `bin/tick.sh`. It exits at once when every slot is held or the instance holds it,
  so most firings cost nothing.
- The launcher spawns runners up to the width both usage windows allow (5-hour and weekly),
  but never more than there are claimable issues. A runner takes a free slot (an `flock`ed
  file) or exits quietly.
- The runner composes the prompt (a lane line, `prompts/tick-core.md`, then the instance's
  `prompts/tick.md`) and runs `claude -p` on the best model that is not cooling down.
- The tick claims an issue (`bin/claim.sh`, a `WIP_LABEL` label and a claim comment), works in a
  worktree under `WORKTREE_ROOT`, lints every body with `bin/public_record_lint.py` before posting
  it, and ends with a summary. It stays inside its budget and the usage ceiling (`bin/usage_ok.sh`).
- A ready PR lands under `bin/merge-lock.sh`, one at a time.
- When a tick ends with work still queued, the runner starts the next tick at once on a fresh
  context. It stops after a short tick, after `MAX_CHAIN` ticks, or when the harness is paused,
  held or out of usage.
- A claim is held only while its PID lives. A tick that dies leaves an issue that the next tick
  resumes.

## Quickstart

```bash
git clone <this repo> ~/harness && cd ~/harness
cp -r templates/instance instance       # gitignored; this is your installation
mv instance/harness.env.example instance/harness.env
$EDITOR instance/harness.env            # TARGET_REPO and WORKTREE_ROOT at least
$EDITOR instance/prompts/tick.md        # the mission: what a tick does in your project
bin/tick.sh check                       # prints what it resolved; exit 1 names what is missing

gh label create ai     --description "Queued for the autonomous harness"
gh label create ai:wip --description "Claimed by a running tick"

crontab -e   # */2 * * * * /home/you/harness/bin/tick.sh
```

Label an issue `ai` and the next firing picks it up. The harness starts nothing until it has a
fresh usage reading (`USAGE_CACHE`, see `instance/harness.env`). Pause it with
`touch state/paused` or by commenting out the cron line. Ticks already running finish their claim.

Optional cron lines: `*/30 * * * * ~/harness/bin/pr_sweep.sh` closes PRs whose issue closed
elsewhere, and `0 * * * * ~/harness/bin/worktree_gc.sh --apply` removes worktrees whose work is done.

## Core and instance

| Lives in | What |
|---|---|
| `bin/`, `prompts/` (core) | the launcher, claims, the merge lock, usage gating, the lint, the dashboard, and the rules every tick follows |
| `instance/harness.env` | required: paths, labels, pool sizing, models, the dashboard's optional sources |
| `instance/prompts/tick.md` | required: the installation's mission, appended to the core prompt |
| `instance/prompts/` | optional: `runbook.md`, `glossary.md`, `public-record.md`, which the tick reads |
| `instance/lint_rules.json` | optional: the project's own lint rules |
| `instance/bin/`, `instance/tests/` | optional: the project's scripts (first on PATH) and its suites |
| `hooks/env.sh` | sourced before ticks run; what it exports reaches every tick |
| `hooks/hold` | exit 0 holds the harness while something every tick needs is down |
| `hooks/worktree-busy` | prints worktree directories in use, so `worktree_gc.sh` keeps them |
| `hooks/worktree-remove` | removes what `git worktree remove` left behind |

`HARNESS_INSTANCE_DIR` points the core at another instance directory. [`docs/INSTANCE.md`](docs/INSTANCE.md)
gives each contract.

## Dashboard

`bin/dashboard.sh` starts a read-only status page on 127.0.0.1 and prints its URL, which carries
an access token. It shows running ticks, the queue, landings, usage and the allocator log, plus
the instance's devices, run queues and build locks when `harness.env` configures them.
`bin/dashboard.sh status` prints the URL again, `bin/dashboard.sh stop` stops it, and
`bin/dashboard.py --help` lists the options.

## Fleet

[Fleet](docs/FLEET.md) watches devices and the agent sessions on them, and runs agents there. It is
optional. With `FLEET_DISPATCH=1` in `harness.env`, each tick runs through `fleet dispatch` on the
device `FLEET_DEVICE` names (`auto` picks one) and with the agent `TICK_AGENT` (default `claude`),
instead of a local `claude -p`. With `FLEET_STATUS_CMD="fleet status --json"`, the dashboard shows a
Devices and an Agent sessions panel. Unset, the harness runs on one host as above.
`templates/instance/fleet.toml.example` and `fleetd.toml.example` are starting points;
[`docs/INSTANCE.md`](docs/INSTANCE.md#fleet) gives the keys.

## Tests

```bash
tests/run.sh
```

It runs every `tests/test_*.sh` and `tests/test_*.py`, plus any suites under `instance/tests/`.
The shell suites need Linux: bash 4+, `flock`, `setsid` and `lslocks` (util-linux), `pgrep`
(procps), GNU `stat` and `date`, `bc`, `zstd`, `jq`, `curl`, `git` and `python3`. On another OS,
run them in a container started with `--init`, so killed processes are reaped. `python3 tests/test_dashboard.py` runs anywhere and skips the cases that need `/proc`.

## Safety posture

Ticks run unattended with permission prompts off, so the rails are in the prompts:

- only labelled issues are worked;
- work happens in worktrees, never in your checkout;
- a tick merges only PRs the harness opened and can verify, never into a protected branch;
- no force-push, no reviewer requests, no branches a tick did not create;
- evidence is reported honestly and gates are never weakened to get a pass;
- the public record is written for a reader who has only the repository, and is linted before
  every post.

An instance prompt may narrow these rails, never loosen them.

## Documentation

- [`docs/INSTANCE.md`](docs/INSTANCE.md): the instance interface, hooks, prompt composition, lint rules, dashboard sources
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): why the design has this shape
- [`docs/OPERATIONS.md`](docs/OPERATIONS.md): running it, reading the logs, recovery
- [`docs/LESSONS.md`](docs/LESSONS.md): failure modes seen in long runs, and their fixes
- [`docs/FLEET.md`](docs/FLEET.md): fleet, the optional Rust layer that monitors devices and agent sessions and dispatches to them
- [`prompts/public-record.md`](prompts/public-record.md): how every issue, PR and comment is written
