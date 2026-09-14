# pckr work lane

Autonomous work session for a single ticket in this repository. Do
not scope-creep beyond the named ticket.

I am trusting you to orchestrate this ticket end to end, but execute
as little of it yourself as possible: delegate implementation, test
writing, and mechanical edits to glm-5-3-flash subagents (briefs must
forbid sub-agent spawning and commits); keep investigation synthesis,
diff review, and commits on the main session. Verify each agent's
actual diff, not its report.

## Start

1. Run `tm ready <KEY>` and stop if it reports the ticket blocked.
2. Work only `<KEY>`. Note unrelated bugs or cleanup as follow-ups
instead of fixing them here.

## Workflow

- Write a failing test before the implementation that makes it pass.
- Keep commits small and focused, one logical change per commit, in
imperative mood. Never add Co-Authored-By.
- The base branch is `main`. Open the PR with `tm pr create` (not
`gh pr create`) so the ticket is associated; do not transition ticket
status yourself — `tm pr create` applies the configured status.

## Repo facts and hazards

- `cargo` is NOT on the host PATH. Every cargo command runs through
the flake dev shell: `nix develop -c cargo <...>`.
- Integration tests (`tests/tui_test.rs`, `tests/gitinfo_test.rs`)
spawn real tmux servers and scratch git repos; the dev shell provides
`tmux` and `git`. The nix package sets `doCheck = false` for this
reason — do not "fix" that.
- The plugin contract is frozen and public: the tmux user options
`@picker_status`, `@picker_server`, and `@picker_refresh_cmd` (names
and semantics — see README "Plugin contract"). Never rename or change
them.
- `docs/parity.md` is the behavioral contract for the CLI and TUI.
If a ticket deliberately changes behavior, update it in the same PR;
otherwise treat it as authoritative.
- `/target` and `/result` are gitignored build outputs; never commit
them.

## Before finishing

Leave all four gates green, in this order:

1. `nix develop -c cargo fmt --check`
2. `nix develop -c cargo clippy --all-targets -- -D warnings`
3. `nix develop -c cargo test`
4. `nix build`
