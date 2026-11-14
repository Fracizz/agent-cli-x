---
name: agent-cli-x
description: Run commands, search, read/edit and transfer files on another registered machine (peer) through agent-cli-x. Use when a task needs to build, test, inspect logs, or change code on a remote Windows/Linux server instead of the local one.
---

# agent-cli-x

Remote commands/files over pinned TLS. Put `agent-cli-x` on `PATH`.

```bash
agent-cli-x peer list          # cached name/id, OS/arch, shell
agent-cli-x info <peer>        # live permissions, workspaces, jobs; @all = online peers
```

Paths: `workspace:relative/path` (`proj:src/main.rs`, root `proj:`); omit the prefix for a default workspace. Absolute paths, `..`, denied paths and missing CRUD rights (`info`: `path=rcud`) fail with 102. Never bypass with `exec`.

## Commands

```bash
agent-cli-x exec B --cwd proj:web --timeout 1200 "pnpm build"  # options BEFORE the command; exit code passed through
agent-cli-x exec B --script ./check.sh        # multi-line / tricky quoting: write a local file, no nested escaping
agent-cli-x exec B --detach "cargo test"      # long jobs; then: job logs B <id> --follow | job list B | job kill B <id>
agent-cli-x exec B,C "git pull"               # or @all; parallel, grouped per peer, exit 0 only if all succeed
agent-cli-x grep B "fn main" proj:src --glob "*.rs"   # -i, -F; exit 1 = no match
agent-cli-x ls   B proj:src --depth 2
agent-cli-x read B proj:src/main.rs --lines 100:200 -n
agent-cli-x edit B proj:src/main.rs --old "a" --new "b"   # --old-file/--new-file for multi-line; --all
agent-cli-x write B proj:notes.md --from ./notes.md       # or --content / stdin
agent-cli-x put B ./dist proj:web/dist --exclude node_modules ; agent-cli-x get B proj:logs ./logs
agent-cli-x cp A dl:pkg B home:               # peer → peer, streamed, nothing kept locally
agent-cli-x rm B proj:build/tmp -r            # to recycle bin; rollback B <op_id> undoes write/edit/put/rm
agent-cli-x upgrade B                         # on a `版本不一致` hint
agent-cli-x restart B ; agent-cli-x set-port B 48000   # use these, not manual config edits
```

## Keep output small

- Bounded output (also JSON): `exec`/`job logs` share head + tail 100 lines / 16KB each (multi-peer: 50 / 8KB each); any cut retains a full local log. `read` without an end: ≤2000 lines / 64KB, 2000 chars/line; follow `next_start`/the `--lines N:` hint, use the printed `get` command for clipped lines. `ls` ≤500 entries, `grep` ≤200 matches / 300 chars; pages ≤60KB of entries. Continue with `--offset` / `next_offset`; `--max` changes the count, `--full` removes byte/line clipping.
- Locate with `grep` first, then `read --lines` around the hit. Avoid reading whole large files or `ls --depth` on big trees.
- `--json` for fields (`sha256`, `op_id`, job id); `--full` for essential trimmed text. Full JSON caps at 1MB (multi-peer full: 8MB/peer); remainder stays in the log.

## Rules

- Non-interactive (empty stdin, `CI=1`): pass `-y`/`--yes`/`--no-edit`, or it hangs until timeout.
- Fresh shell per `exec`: use `--cwd`/`--env` or chain steps. Default: Windows `pwsh`, Linux `sh` (`--shell cmd|bash|...`).
- Prefer file commands: they respect .gitignore, deny rules, permissions and support rollback.
- `read` before `edit`; `--old` must match exactly once. For concurrent safety pass `--expect-sha` from `read --json`.
- `rm --permanent` only when the user asks; to free space after `rm`, use `purge <peer> <op_id>`.
- Timeout kills the process tree (exit 104); raise `--timeout` or use `--detach` for anything over a few minutes.
- `exec` runs with the full rights of the `serve` user. Do not use it to change read-only/denied paths unless the user asked for exactly that, and say so.
- No relaying: don't run `agent-cli-x` inside a peer's `exec` unless asked, never read a peer's `~/.agent-cli-x`. On `禁止借道` stop and tell the user.
- Unreachable peer (100): run `doctor`. `serve` defaults to background (windowless, returns once ready); use `serve --foreground` for service managers/debugging. Autostart and legacy trash recovery: see README.

## Exit codes

other = remote command's code · 100 cannot connect · 101 bad token · 102 denied by policy · 103 not found · 104 timeout (upgrade/restart: not back yet, check `info` later) · 105 conflict (edit match / sha / jobs running on upgrade) · 106 cert fingerprint mismatch: ask a human, never re-register · 107 I/O · 108 internal · 109 config (unknown peer) · 110 bad arguments · 111 busy · 137 job killed
