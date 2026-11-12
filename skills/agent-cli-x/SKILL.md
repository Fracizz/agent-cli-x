---
name: agent-cli-x
description: Run commands, search, read/edit and transfer files on another registered machine (peer) through agent-cli-x. Use when a task needs to build, test, inspect logs, or change code on a remote Windows/Linux server instead of the local one.
---

# agent-cli-x

Run commands and edit files on a **peer** over pinned TLS. `agent-cli-x` must be on `PATH`.

```bash
agent-cli-x peer list          # peers: node name/id, OS/arch, default shell (cached, no network)
agent-cli-x info <peer>        # live: workspaces, path permissions, deny rules, jobs; `info @all` = who is online
agent-cli-x doctor             # local service / firewall / peers health
```

Paths are `workspace:relative/path` (`proj:src/main.rs`, root `proj:`); the prefix may be omitted for a single/default workspace. Absolute paths, `..`, deny-listed paths and missing CRUD rights (`info` shows `path=rcud`) are refused with exit 102. Do not work around them via `exec`.

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
```

## Keep output small

- Output is already bounded: `exec`/`job logs` keep head + tail 100 lines (full log path printed to stderr, read it locally only if needed); `read` without an end line returns ≤2000 lines and prints the next `--lines` range; `ls` ≤500 entries; `grep` ≤200 matches.
- Locate with `grep` first, then `read --lines` around the hit. Avoid reading whole large files or `ls --depth` on big trees.
- Use `--json` only when you need a field (e.g. `sha256`, `op_id`, job id), not by default. Use `--full` only when the trimmed middle really matters.

## Rules

- Non-interactive (empty stdin, `CI=1`): pass `-y`/`--yes`/`--no-edit`, or it hangs until timeout.
- Each `exec` is a fresh shell: use `--cwd`/`--env` or chain steps. Default shell: `pwsh` on Windows, `sh` on Linux (`--shell cmd|bash|...`).
- Prefer `grep`/`rm`/file commands over doing the same through `exec`: they respect .gitignore, deny rules, permissions and can be undone.
- `read` before `edit`; `--old` must match exactly once. For concurrent safety pass `--expect-sha` from `read --json`.
- `rm --permanent` only when the user asks; to free space after `rm`, use `purge <peer> <op_id>`.
- Timeout kills the process tree (exit 104); raise `--timeout` or use `--detach` for anything over a few minutes.
- `exec` runs with the full rights of the `serve` user. Do not use it to change read-only/denied paths unless the user asked for exactly that, and say so.
- No relaying: don't run `agent-cli-x` inside a peer's `exec` unless asked, never read a peer's `~/.agent-cli-x`. On `禁止借道` stop and tell the user.
- Unreachable peer (100): run `doctor`. Setting up start at boot: follow README "Start at boot".

## Exit codes

other = remote command's code · 100 cannot connect · 101 bad token · 102 denied by policy · 103 not found · 104 timeout · 105 conflict (edit match / sha / jobs running on upgrade) · 106 cert fingerprint mismatch: ask a human, never re-register · 107 I/O · 108 internal · 109 config (unknown peer) · 110 bad arguments · 111 busy · 137 job killed
