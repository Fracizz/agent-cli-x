# Changelog

## Unreleased

- `workspace add` keeps the previous sole workspace as the default (writes `default_workspace`) when a second one is added without `--default`.

## 0.0.18

- Workspaces work out of the box: with none configured, the user's home directory (`%USERPROFILE%` / `$HOME`) is served as workspace `home` (read/write), so a freshly initialized, authorized machine can be used immediately.
- `workspace add [peer] <name> <dir> [--default] [--allow rcud]` / `workspace remove [peer] <name>`: register or remove a workspace locally or, from any authorized caller, on a peer. `config.toml` is edited with comments kept, validated like `serve` does, and `serve` restarts to apply.
- `stop [--force]`: stop this machine's `serve` (local only); waits until the port is closed, exits 0 if it was not running, background jobs keep running.
- Windows PowerShell 5.1 is started from `%SystemRoot%\System32\WindowsPowerShell\v1.0` when it is missing from `PATH` (previously `exec` failed with `program not found`).
- Clearer errors when a machine has no workspace (exit 102 with the `workspace add` command, plus the config error when serve runs on the last good config).

## 0.0.17

- Pairing commands (`init`, `whoami --pair`, `token add/rotate`) no longer pick a VPN/proxy TUN address (sing-box, Clash, WireGuard, etc.) when the system route goes through it: local interfaces are ranked by gateway, adapter type and name. Other candidate IPs are printed on stderr (JSON: `other_ips`).

## 0.0.16

- Authorization codes: `token add <name> --code [--ttl 4h|8h]` issues a temporary full-access token (default 4h, optional 8h). Expired codes are rejected with exit code 101 and a renewal hint; `token rotate <name> [--ttl ...]` renews one. `token list` shows the expiry.

## 0.0.15

- `peer list [query...]` filters peers by case-insensitive substring of name, node name, IP:port, OS or remark (several terms must all match); exits 1 when nothing matches. Also applies to `--json`.

## 0.0.14

- Detect binary files that contain no NUL bytes (more than 5% unexpected control characters in the first 8KB), so `read` returns the `get` hint instead of garbage and `grep` skips them. Non-UTF-8 text such as GBK is still read as text.

## 0.0.13

- `edit` falls back to a tolerant match (trailing whitespace, smart quotes, dashes, special spaces) when the exact text is not found; only the matched span is replaced and the result carries `fuzzy: true`.
- `write`, `edit` and `rm` on the same file are serialized on the peer, so concurrent edits no longer overwrite each other.
- `write` always creates missing parent directories; `--mkdir` is kept as a hidden no-op.
- `read` accepts several paths (per-file range as `path#START:END`) sharing one 2000-line / 64KB window; `--json` returns `{files, skipped}`.
- `read` on an image downloads it to the local temp directory and prints the path; other binary files get a `get` command in the error.

## 0.0.12

- Add native Windows desktop observation with `desktop windows` and `desktop screenshot`, gated by `policy.desktop_capture` and reported by `info`.
- Support title filters, window IDs and physical-pixel regions; transfer PNG captures directly to a local file without remote capture scripts or image files.
- Bound window listings and screenshot sizes by default, with pagination and explicit `--full` support. Report locked or unavailable desktops with actionable errors.
- Keep Windows desktop pixel conversion compatible with strict Clippy checks on current Rust toolchains. The 0.0.11 release build was cancelled before publication.

## 0.0.10

- Add local peer remarks with `peer add --remark` and `peer set --remark`; show remarks in `peer list` and `info`, including JSON output.
- Allow editing remarks offline, preserve them when re-registering peers, and clear them with `--remark ""`. Remarks are limited to one line and 300 characters (1200 UTF-8 bytes).

## 0.0.8

- Make `serve` default to detached, windowless startup with readiness confirmation, file logs and the original startup error codes. Use `serve --foreground` for debugging and service managers; update existing Task Scheduler/systemd actions to include it. `--background` remains an explicit alias for the default.

## 0.0.7

Fixes for safe file recovery, process supervision and bounded agent output.

- Recheck real paths and CRUD permissions before rollback to reject parent symlink escapes.
- Identify recycle-bin entries by their system identity so repeated deletion of one path cannot restore or purge another version.
- Keep enforcing command timeouts and background-job termination after stdout and stderr close.
- Enforce the 64-connection limit before spawning workers, and authenticate before reading request bodies. Preserve authentication errors during large uploads.
- Retain complete logs for every output cut, including single-line cuts; apply one stdout/stderr budget to ordinary and JSON output.
- Preserve the actual tail of multi-peer output beyond 8MB, and finish rejected frame uploads promptly.
- Bound long-line reads without blocking later lines; provide download instructions for oversized explicit ranges.
- Add byte budgets and stable `--offset` pagination to `ls` and `grep`, with matching JSON continuation fields and `--full` overrides.
- Align the minimum Rust version with locked dependencies and check Rust 1.88 in CI.

Compatibility notes:

- Building from source now requires Rust 1.88 or newer.
- Legacy or ambiguous recycle-bin records return exit 105 and require manual handling in the system recycle bin. Backup metadata remains available.
- Default command output shares its budget across stdout and stderr. `ls`/`grep` pagination requires an upgraded peer; old peers retain basic command support.

Validation: 64 unit tests, 12 end-to-end tests and 6 runtime regressions pass on Linux, together with formatting, strict Clippy, the release build and a Rust 1.88 check of all targets.
