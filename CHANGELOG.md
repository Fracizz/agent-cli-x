# Changelog

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
