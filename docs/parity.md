# pckr — parity spec and design decisions

pckr is a standalone tmux session picker TUI (Rust + ratatui), replacing
`scripts/tmux-session-picker.sh` in jowi-dev/devtools (devtools ticket GH-2).
This document is the behavioral contract ported from the bash script; the
devtools cutover PR deletes the script once every item here holds.

## Invocation

`pckr` with no arguments launches the TUI (designed to run inside
`tmux display-popup -E`). Subcommands (all also used by tests):

- `pckr list [--plain]` — print the session table. `--plain` prints the raw
  9-field TSV; without it, print the padded, colored table (header first).
- `pckr branch-status <path>` — print `<branch> [merged|unmerged]`,
  `[detached]`, or nothing; always exit 0.
- `pckr project-name <path>` — print parent-repo basename or `-`; exit 0.
- `pckr kill <session>` — kill session + worktree cleanup (below); exit 0.
- `pckr root-session [<session>]` — print the root session name for a
  worktree session (resolution below); exit non-zero only if unresolvable.
- `pckr jump-root [<session>]` — switch client to the root session; never
  fails: unresolvable root, root == self, or missing root session prints a
  message and exits 0 without switching.

Unknown subcommands print usage to stderr and exit 2 (the bash script hung
by falling through to the interactive UI; do not reproduce that bug).

## Session list

Source: `tmux list-sessions -F '#{session_name}|#{session_path}|#{@picker_status}|#{@picker_server}'`.

Row fields (TSV order for `--plain`, one row per session):
1. `name` — machine key, never displayed.
2. `idx` — 1-based row number.
3. `marker` — `*` if this is the current session (`display-message -p '#S'`), else `-`.
4. `name` again — display copy.
5. `attn` — `@picker_status` and `@picker_server` values concatenated with no
   separator; `-` if both empty.
6. `wt` — `wt` if `<session_path>/.git` is a regular FILE (linked worktree), else `-`.
7. `project` — see project-name; `-` on failure.
8. `branch` — from branch-status; `-` if none.
9. `status` — `merged` / `unmerged` / `detached` / `-`.

Table rendering: header `#`, ` ` (marker), `SESSION`, `ATTN`, `WT`,
`PROJECT`, `BRANCH`, `STATUS`; columns padded to max plain-text width
(`#` right-justified, rest left), two spaces between columns. Colors:
`status` green when `merged`, yellow when `unmerged`/`detached`; `branch`
always dim; nothing else colored.

### Refresh hook (plugin trigger contract — NEW, replaces hardcoded phoenix call)

Before building the list (initial and every refresh), read the tmux GLOBAL
user option `@picker_refresh_cmd`; if non-empty, run it via `sh -c`,
discarding output and errors. devtools sets it to invoke
`phoenix-picker-server.sh detect`. pckr knows nothing about phoenix.

## branch-status logic (ported verbatim from bash)

- Not a dir / not a git worktree → empty output, exit 0.
- Branch via `git branch --show-current` (NOT `rev-parse --abbrev-ref`:
  a tag named `main` makes ref-shortening return `heads/main`). Empty
  branch → `[detached]`.
- `resolve_base` order: `origin/HEAD` short name if non-empty →
  `origin/main` → `origin/master` → local `main` → local `master` → give up
  (empty output). Base short name == current branch → empty output.
- `git merge-base --is-ancestor HEAD <base>` → `<branch> [merged]`.
- Squash detection (git-delete-squashed trick): `mb = merge-base base HEAD`;
  `tree = rev-parse HEAD^{tree}`; `synthetic = commit-tree tree -p mb -m _`;
  `git cherry base synthetic` starting with `-` → `[merged]`, else
  `[unmerged]`. Any git failure along the way → `[unmerged]`.
- All git calls local-only; never fetch.

## project-name logic

`git -C <path> rev-parse --path-format=absolute --git-common-dir`, strip
trailing `/.git`, basename. Yields the parent repo name for linked worktrees
and the repo name for regular checkouts; `-` for non-git/missing paths.

## Kill flow

1. Refuse to kill the current session (silent no-op).
2. TUI only: classify the session via `tm runs kill-safety` (see below).
   `safe` proceeds straight to step 3; any other tier arms ConfirmKill
   instead of killing (see TUI behavior). The CLI `pckr kill <session>`
   subcommand skips classification entirely and always proceeds straight to
   step 3.
3. Capture `#{session_path}` BEFORE killing.
4. `tmux kill-session -t <session>` (best-effort).
5. If the path exists and `<path>/.git` is a file:
   `main_repo = git-common-dir` minus `/.git`; then
   `git -C <main_repo> worktree remove <path>` (no `--force`, best-effort);
   then `git -C <main_repo> worktree prune` (best-effort). Without
   `--force`, git refuses to remove a worktree with modified or untracked
   files, a lock, or submodules — in that case the directory is left in
   place with no fallback removal. This is a deliberate divergence from the
   predecessor bash script, which force-removed and fell back to
   `rm -rf <path>`; that combination once destroyed a real worktree's
   uncommitted work (GH-1).
6. Always exit 0.

### Kill-safety classification (tm dependency contract)

TUI kills (never CLI `pckr kill`) run `tm runs kill-safety <session_name>` to
classify the session before touching it (contract: tskmstr's
`docs/decisions/0005-kill-safety-classification.md`). stdout line 1 is one of
`live-run` / `root-session` / `safe` / `unknown`; line 2 is a human-readable
reason, display-only, never branched on. Non-zero exit, an unrecognized line
1 token, or `tm` not being installed all collapse to `unknown`. This is a
deliberate divergence from the bash picker, which killed unconditionally with
no classification step. Classification runs once per `x` keypress (skipped
entirely for the current session, see Kill flow above) and may block briefly,
since `tm` can consult `gh`.

## root-session / jump-root

Resolution order: (1) the session's `@root_session` tmux option if
non-empty; (2) fallback: if the session path's `.git` is a file, derive the
root session name from the main checkout's dirname basename with dots
mapped to dashes (matching tm's session naming). `jump-root` switches the
client to that session and never aborts the caller.

## TUI behavior (modal)

- NORMAL mode (initial): status line `[N] session >`; header/help line
  `NORMAL — enter:switch | x:kill | g:root | t:tiles | 1-9:jump | i:filter | q/esc:quit | [merged]=safe to close`.
  Keys: `j`/`k` (and arrows) move selection; `enter` switch to selected
  session and exit; `x` on the current session is a silent no-op (short-
  circuits before classification); `x` on any other row classifies it (kill
  flow above) — `safe` kills + refreshes silently, any other tier enters
  ConfirmKill instead of killing; `g` jump-root of the CURRENT session (no
  arg) and exit; digits `1`-`9` select row N of the currently visible
  (filtered) list and accept — no-op if N exceeds visible rows; `i` enter
  INSERT; `q` or `esc` quit.
- INSERT mode: status line `[I] filter > <query>`; help line
  `INSERT — type to filter | enter:switch | esc:normal mode`. All typed
  printable chars edit the filter (case-insensitive subsequence match on the
  display row text); backspace deletes; `enter` accepts the selected match;
  `esc` returns to NORMAL keeping the filter applied. Single-key commands
  (x, q, digits, g, i, j, k) MUST NOT trigger while in INSERT.
- ConfirmKill mode (armed when `x` classifies the selected session as
  `live-run`, `root-session`, or `unknown`): help line exactly
  `y=kill  any other key=cancel`; prompt line
  `Kill '<name>' + worktree? [<label>] <reason>`, where `<label>` is
  `live run` / `root session` / `unclassified` (unclassified covers
  `unknown`) and the trailing `<reason>` is omitted entirely when the tier's
  reason string is empty. `y`/`Y` confirms and proceeds to the kill flow;
  ANY other key (including digits, `g`, `i`, `q`, `esc`) cancels back to
  NORMAL, discarding the pending kill, leaving the session and worktree
  untouched. Single-key NORMAL commands (j, k, g, digits, i, q, x) MUST NOT
  trigger while in ConfirmKill — every non-`y`/`Y` key is swallowed by the
  cancel path instead.
- The filter persists when returning to NORMAL (digits then index into the
  filtered rows), matching the bash/fzf behavior.
- Selection clamps into range after refresh/filter changes.
- List refresh re-runs the refresh hook + full list build (matching fzf
  `reload($SELF list)` behavior after kill).

### Tiled view (additive, outside parity scope)

`t` in NORMAL mode toggles a tiled per-project view that did not exist in
the bash picker; it is documented in the README, not here. The flat view
above is the parity surface: launching pckr always starts in the flat NORMAL
mode, and every guarantee in this document holds there unchanged. The tiled
view adds one optional global tmux option, `@picker_tile_cmd` (documented in
the README), and no `tm` dependency; its kill path reuses the same
classification and ConfirmKill flow described above.

## Environment

- `TMUX_PICKER_SOCKET` — when set, ALL tmux invocations use
  `tmux -L <socket>` (superset of the bash script, which honored it only in
  root-session/jump-root; required for isolated integration tests).
- No other environment is read.

## Non-goals / kept out

- `@picker_status` semantics (❓ outranks ⏸) stay in the writer scripts
  (claude-picker-attention.sh); pckr only renders the option value.
- Phoenix detection stays in phoenix-picker-server.sh; pckr only runs the
  generic `@picker_refresh_cmd`.
- Ready-ticket logic (which issues count as ready to pick up, e.g. via
  `tm ready`) stays in the writer script behind `@picker_tile_cmd`; pckr
  only runs the command and renders its first stdout line.
