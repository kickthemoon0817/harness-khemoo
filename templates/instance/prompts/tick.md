Repository: `example/app`, a small web service. Read `$HARNESS_INSTANCE_DIR/prompts/runbook.md` first
and follow it; it holds the mission, the tick procedure and this project's rails.

This tick: LAND A FIX. Claim the oldest claimable issue that has a clear implementation path and
implement it end to end: a worktree under `$WORKTREE_ROOT` on a new branch `ai/<issue>-<slug>`, the
failing test first, the fix, `make test`, and a PR into `main` written to the public record. Merge it
only when `make test` passes on the merge result; otherwise leave it open and say why on the issue.
Run an audit only when nothing in the queue is claimable.

This installation narrows the core rails: never edit `migrations/`, and never change a public API
signature without an issue that asks for it.
