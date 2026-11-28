# Autonomous GitHub issue harness for coding-agent CLIs

The harness works a GitHub issue queue on its own, with Claude Code, Codex or agy (the Antigravity
CLI). A tick claims one labelled issue, fixes it in its own worktree, verifies the fix, and opens a
pull request that carries the evidence. It either lands the PR or leaves it for review. Cron fires
a bash launcher every couple of minutes. The launcher sizes the pool from each agent's usage windows and
the queue depth, and each tick is one non-interactive agent run, on the best model of one priority
list across agents. Nothing runs as a daemon and there is no database: all durable state lives in
the issues and PRs. The core knows nothing about your project. Your build, tests, rules and
infrastructure live in an instance directory that you own.

## How a tick works

- Cron runs `bin/tick.sh`. It exits at once when every slot is held or the instance holds it,
  so most firings cost nothing.
- The launcher spawns runners up to the width the agents' usage windows allow (for Claude, its
  5-hour and weekly windows), but never more than there are claimable issues. A runner takes a
  free slot (an `flock`ed file) or exits quietly.
- The runner composes the prompt (a lane line, `prompts/tick-core.md`, then the instance's
  `prompts/tick.md`) and pipes it to the agent CLI of the best `TICK_MODELS` entry
  (`agent:model`) that is not cooling down and whose agent has room under its windows. An entry
  may name one of several accounts of an agent, `agent@account:model`, each a folder signed in to by
  hand and counted on its own (`AGENT_ACCOUNTS_DIR`, docs/INSTANCE.md; check the providers' terms
  first).
- The tick claims an issue (`libexec/claim.sh`, a `WIP_LABEL` label and a claim comment), works in a
  worktree under `WORKTREE_ROOT`, lints every body with `"$HARNESS_BIN" lint` before posting
  it, and ends with a summary. It stays inside its budget and the usage ceiling (`libexec/usage-ok.sh`).
- A ready PR lands under `libexec/merge-lock.sh`, one at a time.
- When a tick ends with work still queued, the runner starts the next tick at once on a fresh
  context. It stops after a short tick, after `MAX_CHAIN` ticks, or when the harness is paused,
  held or out of usage.
- With `SUPERVISOR=1`, a supervisor tick starts on events (a failed tick, a usage limit, a reset,
  a loop, an operator's comment) or every half hour. Run with no tools, it reads `harness health`
  and answers with a directive (`harness directive`) that pauses the harness, narrows its width,
  reorders its models, focuses ticks or stops a looping one, and a few comments and labels. The
  harness applies them itself, inside the configuration and the usage caps. With no
  directive, an expired or an invalid one, the harness runs on its own formulas.
- A claim is held only while its PID lives. A tick that dies leaves an issue that the next tick
  resumes.

## Quickstart

```bash
git clone <this repo> ~/harness && cd ~/harness
cargo build --release                   # target/release/harness, and fleet and fleetd
cp -r templates/instance instance       # gitignored; this is your installation
mv instance/harness.env.example instance/harness.env
$EDITOR instance/harness.env            # TARGET_REPO and WORKTREE_ROOT at least
$EDITOR instance/prompts/tick.md        # the mission: what a tick does in your project
bin/tick.sh check                       # prints what it resolved; exit 1 names what is missing

gh label create ai     --description "Queued for the autonomous harness"
gh label create ai:wip --description "Claimed by a running tick"

crontab -e   # */2 * * * * /home/you/harness/bin/tick.sh
```

Label an issue `ai` and the next firing picks it up. An agent starts nothing until it has a
fresh usage reading: Claude's usage cache, Codex's session rollouts, or the source
`USAGE_SOURCE_<agent>` names (see `instance/harness.env`). Pause it with
`touch state/paused` or by commenting out the cron line. Ticks already running finish their claim.

Optional cron lines: `*/30 * * * * ~/harness/bin/pr-sweep.sh` closes PRs whose issue closed
elsewhere, and `0 * * * * ~/harness/bin/worktree-gc.sh --apply` removes worktrees whose work is done.

## Core and instance

| Lives in | What |
|---|---|
| `bin/` (core) | what cron and operators run: `tick.sh`, `pr-sweep.sh`, `worktree-gc.sh`, `dashboard.sh` |
| `libexec/` (core) | what the launcher and ticks call: claims, the merge lock, the usage check, the lint, the dashboard server |
| `lib/` (core) | shell sourced by the scripts: loading the instance |
| `prompts/` (core) | the rules every tick follows and the supervisor's |
| `crates/` (core) | the Rust workspace: `harness` (the lint, the queue count, live ticks, usage windows, the supervisor's health and directive, the dashboard), and `fleet`, `fleetd` and their protocol ([`docs/FLEET.md`](docs/FLEET.md)) |
| `instance/harness.env` | required: paths, labels, pool sizing, models, the dashboard's optional sources |
| `instance/prompts/tick.md` | required: the installation's mission, appended to the core prompt |
| `instance/prompts/` | optional: `runbook.md`, `glossary.md`, `public-record.md`, which the tick reads, and `supervisor.md` for the supervisor |
| `instance/lint_rules.json` | optional: the project's own lint rules |
| `instance/bin/`, `instance/tests/` | optional: the project's scripts (first on PATH) and its suites |
| `hooks/env.sh` | sourced before ticks run; what it exports reaches every tick |
| `hooks/hold` | exit 0 holds the harness while something every tick needs is down |
| `hooks/worktree-busy` | prints worktree directories in use, so `worktree-gc.sh` keeps them |
| `hooks/worktree-remove` | removes what `git worktree remove` left behind |

`HARNESS_INSTANCE_DIR` points the core at another instance directory. [`docs/INSTANCE.md`](docs/INSTANCE.md)
gives each contract.

## Dashboard

`bin/dashboard.sh` starts a read-only status page on 127.0.0.1 and prints its URL, which carries
an access token. It shows running ticks, the queue, landings, usage and the allocator log, plus
the instance's devices, run queues and build locks when `harness.env` configures them.
`bin/dashboard.sh status` prints the URL again, `bin/dashboard.sh stop` stops it, and
`harness dashboard --help` lists the options.

## Fleet

[Fleet](docs/FLEET.md) watches devices and the agent sessions on them, and runs agents there. It is
optional. With `FLEET_DISPATCH=1` in `harness.env`, each tick runs through `fleet dispatch` on the
device `FLEET_DEVICE` names (`auto` picks one), with each entry's agent as the adapter, instead of
a local agent CLI. With `FLEET_STATUS_CMD="fleet status --json"`, the dashboard shows a
Devices and an Agent sessions panel. Unset, the harness runs on one host as above.
`templates/instance/fleet.toml.example` and `fleetd.toml.example` are starting points;
[`docs/INSTANCE.md`](docs/INSTANCE.md#fleet) gives the keys.

## Tests

```bash
tests/run.sh
```

It builds the `harness` binary (unless `HARNESS_BIN` names one) and runs every `tests/test_*.sh`,
plus any suites under `instance/tests/`. The shell suites need Linux: bash 4+, `flock`, `setsid` and
`lslocks` (util-linux), `pgrep` (procps), GNU `stat`, `date` and `env` (coreutils 8.31 or later),
a C compiler (`cc`), `bc`, `zstd`, `jq`, `curl`, `git` and `cargo`. On another OS, run them in a container started with `--init`, so killed processes are
reaped. The Rust tests (`cargo test`) cover the lint, the queue count, live ticks, usage windows, the
dashboard and fleet; they run anywhere and leave out the cases that need
`/proc` and `lslocks` where those are missing.

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
