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
pckr renders it.

## Keys

NORMAL mode:

| Key | Action |
|---|---|
| `j` / `k` / arrows | Move selection |
| `enter` | Switch to selected session, exit |
| `x` | Kill selected session (+ worktree cleanup), refresh |
| `g` | Jump to root session of the current session, exit |
| `1`-`9` | Jump to and switch to the Nth visible row |
| `i` | Enter INSERT (filter) mode |
| `q` / `esc` | Quit |

INSERT mode:

| Key | Action |
|---|---|
| any printable char | Append to filter (case-insensitive subsequence match) |
| `backspace` | Delete last filter char |
| `enter` | Switch to selected match, exit |
| `esc` | Return to NORMAL, keeping the filter applied |

## Plugin contract

pckr renders the per-session tmux user options `@picker_status` and
`@picker_server` in the ATTN column, concatenated with no separator; any
tool may set them with `tmux set-option -t <session> @picker_status "❓"`.
pckr also runs the global `@picker_refresh_cmd` (via `sh -c`) before each
list build, initial and every refresh. These option names and semantics are
a frozen public contract other tools can depend on.

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
