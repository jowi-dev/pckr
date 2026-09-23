# pckr

pckr is a tmux session picker TUI: a modal, keyboard-driven popup that lists
your tmux sessions with worktree, branch, and merge-status awareness, and
lets you switch, kill, or jump between them without leaving the keyboard.
It's a Rust rewrite of a bash + fzf script (`scripts/tmux-session-picker.sh`)
from [jowi-dev/devtools](https://github.com/jowi-dev/devtools), built to be a
single static binary with no fzf/bash dependency.

## Install

Via Nix, run directly:

```
nix run github:jowi-dev/pckr
```

As a flake input, consumed into a home-manager configuration:

```nix
pckr.url = "github:jowi-dev/pckr";
```

## tmux integration

Add to `~/.tmux.conf`:

```
unbind s
bind s display-popup -E -w 80% -h 60% "pckr"
bind g run-shell "pckr jump-root"
```

Optionally set `@picker_refresh_cmd` (e.g. `set -g @picker_refresh_cmd
"phoenix-picker-server.sh detect"`) to run an arbitrary shell command before
each list build, letting other tools refresh session metadata just before
pckr renders it. You can also set `@picker_tile_cmd` to run a per-project
command in the tiled view (see Plugin contract below).

## Keys

pckr launches into the tiled view (see [Tiled view](#tiled-view)); `t`
switches to the flat list below and back.

NORMAL mode (flat view):

| Key | Action |
|---|---|
| `j` / `k` / arrows | Move selection |
| `enter` | Switch to selected session, exit |
| `x` | Kill selected session (+ worktree cleanup), refresh — asks for confirmation unless `tm` classifies it safe to reap |
| `o` | Open the selected session's pull request in the browser (`gh pr view --web`); stays in the picker |
| `g` | Jump to root session of the current session, exit |
| `1`-`9` | Jump to and switch to the Nth visible row |
| `i` | Enter INSERT (filter) mode |
| `t` | Return to tiled view |
| `q` / `esc` | Quit |

INSERT mode:

| Key | Action |
|---|---|
| any printable char | Append to filter (case-insensitive subsequence match) |
| `backspace` | Delete last filter char |
| `enter` | Switch to selected match, exit |
| `esc` | Return to NORMAL, keeping the filter applied |

Tiled view, TILES focus (tile grid):

| Key | Action |
|---|---|
| `h` / `l` / left/right arrows | Move across tiles |
| `enter` / `j` | Open the selected project's session list |
| `t` | Switch to flat view |
| `g` | Jump to root session of the current session, exit |
| `q` / `esc` | Quit |

Tiled view, SESSIONS focus (drilled session list):

| Key | Action |
|---|---|
| `j` / `k` | Move selection |
| `enter` | Switch to selected session, exit |
| `x` | Kill selected session, same tiered confirmation as flat view |
| `o` | Open the selected session's pull request in the browser (`gh pr view --web`); stays in the picker |
| `h` / `esc` | Back to tiles |
| `t` | Switch to flat view |
| `q` | Quit |

## Tiled view

pckr opens in a tiled, per-project view with the first tile selected.
Pressing `t` switches to the flat session list; `t` again returns to tiles.
Sessions are grouped into one tile per project, using the same PROJECT value
shown in the flat list (resolved locally from git). Each tile shows a
roll-up: session count, unmerged-branch count, an active count (the number
of the project's sessions whose `@picker_phase` is `working`), a blocked
count (shown as a red `[N blocked]` marker if any sessions are blocked), the
aggregated `@picker_status`/`@picker_server` attention flags (concatenated
with no separator, or `-` when none set), and two lines from
`@picker_tile_cmd`: a ready count (`<value> ready`, or `- ready` if unset or
unavailable) and a spend figure (`<value> spend`, or `- spend` if unset or
unavailable). pckr itself never calls `tm`; the tiled view works fully
without `tm` on `PATH`.

The tile grid stretches to fill the entire popup, with the column count
determined by popup width and a floor of about 28 columns by 4 lines per
tile so project titles, session counts, and flags stay legible. When tiles
exceed the available height, the grid scrolls by complete rows. Drilling
in (`enter` or `j` from TILES focus) hides the grid and replaces it with the
selected project's session list under a breadcrumb `tiles › <project>`;
pressing `h` or `esc` returns to the full tile grid. The selected tile is
filled in reverse video. The filter (`i`, INSERT mode) applies to the flat
view only.

## Plugin contract

pckr renders the per-session tmux user options `@picker_status` and
`@picker_server` in the ATTN column, concatenated with no separator; any
tool may set them with `tmux set-option -t <session> @picker_status "❓"`.
The `@picker_last_active` option (Unix epoch seconds of the agent's last
activity, e.g. `tmux set-option -t <session> @picker_last_active "$(date +%s)"`)
is rendered as the AGE column showing minutes since activity (e.g. `3m`, `1h12m`);
ages strictly older than 15 minutes (`STALE_AFTER_SECS` in src/model.rs) render
in red. When unset or not a valid non-negative integer, the AGE cell is blank.
The writer (e.g. `tm runs event` or a hook) is outside pckr.
pckr also runs the global `@picker_refresh_cmd` (via `sh -c`) before each
list build, initial and every refresh. These option names and semantics are
a frozen public contract other tools can depend on.

The global `@picker_tile_cmd` option runs a shell command once per project
at each list build. The command runs as `sh -c "$cmd" sh <project> <root>`,
where `$1` is the project name and `$2` is the project root, with working
directory set to the root. The same two values are exported as
`PICKER_PROJECT` and `PICKER_ROOT`, so a script named directly as the
command (which sees no positional arguments) still receives them. All
projects' commands run concurrently with a one-second deadline; commands
still running at the deadline are killed.

The command's stdout is a set of `key=value` lines, one field per line.
pckr splits each line at the first `=` and trims the key and the value; if
a key repeats, the last line wins. Unknown keys, blank lines, and later
lines with no `=` are ignored, and a field with an empty value counts as
missing. Two keys are recognized: `ready`, shown on the tile as
`<value> ready`, and `spend`, shown on the next line as `<value> spend`. A
missing field shows `-` in its place (`- ready`, `- spend`). As a legacy
form, if the first non-empty line has no `=`, it is used as the `ready`
value, so writer scripts that print just a count keep working. If the
option is unset, the command exits non-zero, or it times out, both fields
show `-`; nothing is logged or displayed. Sessions outside a git repo have
no root, so their tile always shows `- ready` and `- spend`. pckr renders
both values verbatim: no arithmetic, units, or thresholds are applied. The
writer script picks the spend window and unit (for example tokens or
currency over the last 24h, computed from `tm runs`). This option is
optional and joins the frozen contract family. For example, a writer
script that prints a `tm ready` count and a 24h spend total:

```tmux
set -g @picker_tile_cmd '~/bin/ready-count'
```

```
ready=3
spend=$4.20/24h
```

The tiled view's per-tile attention roll-up reads only the two per-session
options above; the ready and spend lines are the only things
`@picker_tile_cmd` feeds.

The per-session option `@picker_runner` names the agent runner a session
uses (for example `claude` or `opencode`). pckr renders it verbatim in the
RUNNER column of the flat and drilled session lists, `-` when unset; the
lane launcher sets it with `tmux set-option -t <session> @picker_runner
claude`. It is render-only: there is no allowlist of runner names and pckr
never acts on the value.

`@picker_phase` is an additive per-session option that pckr renders
verbatim in a PHASE column after RUNNER, in `pckr list`, the flat view, and
the drilled session list (`-` when unset; `pckr list --plain` omits it). Writers such as `tm runs` and devtools hooks set it with
`tmux set-option -t <session> @picker_phase <token>` or unset with
`tmux set-option -u -t <session> @picker_phase`. Token vocabulary:

| Token | Meaning |
|---|---|
| `started` | Agent run just began; branch may have no commits yet |
| `working` | Run is actively producing commits |
| `review` | Work is in review (e.g. PR is open) |
| `blocked` | Run stopped because its ticket has an open blocker (e.g. `tm ready` reported blocked); rendered in red |
| *(unset)* | No lifecycle signal; normal status coloring applies |

pckr only renders the value; it never validates or writes `@picker_phase`.
Beyond rendering it: a `merged` STATUS is not painted green
(safe to close) while any phase is set, because a freshly started branch
with no commits is an ancestor of its base and reads as `merged`. The PHASE
cell renders red when `blocked`. The tiled view counts each project's
`working` sessions as its active count and marks projects with `blocked`
sessions with a `[N blocked]` count; pckr never ages out any phase, so
clearing it is the writer's job (for example `tm runs reap` or a hook).

pckr also reads the global `@picker_usage` option at every list build, after
`@picker_refresh_cmd` runs, and renders it as-is on its own line under the
help line (flat and tiled views), for example
`tmux set-option -g @picker_usage "claude 62% | opencode 3.1M tok"`. pckr
never parses the value; the writer decides what it says. When the option is
unset or empty, no line is shown. This option is part of the same public
contract.

`@picker_pr` is an optional, render-only per-session option: free text
such as `ci:pass rev:1/1`, set with
`tmux set-option -t <session> @picker_pr "ci:pass rev:1/1"`. When any
session has it set, pckr adds a trailing PR column to the flat list and
the drilled session list and shows the value verbatim. pckr never parses
or colors it (prefix a symbol if you want one), and sessions without it
are unchanged. Writers refresh it from `@picker_refresh_cmd` or on their
own schedule. It is not included in `pckr list --plain`.

## Kill confirmation

Pressing `x` shells out to `tm runs kill-safety <session>` (from
[tskmstr](https://github.com/jowi-dev/tskmstr)) to classify the selected
session before killing it:

| Tier | Meaning | Behavior |
|---|---|---|
| `safe` | Nothing important running | Kills + cleans up worktree silently |
| `live-run` | A live task is running in the session | Prompts for confirmation |
| `root-session` | The per-project hub session other sessions jump back to | Prompts for confirmation |
| `unknown` | `tm` couldn't classify it (or isn't installed) | Prompts for confirmation |

Without `tm` on `PATH`, every kill falls into `unknown` and prompts — that's
the safe default. The tier contract (the exact tokens and their meaning) is
pinned in tskmstr's `docs/decisions/0005-kill-safety-classification.md`; pckr
just consumes it.

## Opening a PR

Pressing `o` opens the selected session's pull request in the browser via
`gh pr view --web`, run in the session's path. `gh` is optional; if it's
missing or the command fails, a one-line `pr: <error>` message appears on the
prompt line until the next key press. The picker stays open in all cases.

## CLI subcommands

- `pckr` — launch the interactive TUI.
- `pckr list [--plain]` — print the session table (`--plain` for raw TSV).
- `pckr branch-status <path>` — print `<branch> [merged|unmerged]` or `[detached]`.
- `pckr project-name <path>` — print the parent repo's basename, or `-`.
- `pckr kill <session>` — kill a session and clean up its worktree, if any.
  Cleanup is skipped (the directory is left in place) if the worktree has
  uncommitted or untracked changes.
- `pckr root-session [<session>]` — print the resolved root session name.
- `pckr jump-root [<session>]` — switch the client to the root session.

## Development

```
nix develop
cargo test
```

The integration tests spawn real tmux servers and git repositories, so they
need `tmux` and `git` on `PATH` — the nix package sets `doCheck = false`
because the build sandbox has neither.
