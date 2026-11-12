//! 配置：`config.toml`（人工编辑，带注释）与 `peers.toml`（由 `peer add` 维护）。

use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 47821;

/// 数据目录：优先 `AGENT_CLI_X_HOME`，否则 `~/.agent-cli-x`。
pub fn home_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("AGENT_CLI_X_HOME") {
        return PathBuf::from(p);
    }
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agent-cli-x")
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub token: String,
    /// 本机名称：对端 `info` / `peer list` 中显示，未配置时用主机名。
    #[serde(default)]
    pub node_name: Option<String>,
    /// 本机唯一 ID（6 位短 ID），init 时随机生成。
    #[serde(default)]
    pub node_id: Option<String>,
    /// 旧版本模板里的预留字段，保留以兼容已有配置。
    #[serde(default)]
    #[allow(dead_code)]
    pub ai_cmd: String,
    #[serde(default)]
    pub default_workspace: Option<String>,
    #[serde(default)]
    pub workspaces: BTreeMap<String, String>,
    #[serde(default)]
    pub policy: Policy,
}

/// 一条路径权限规则：`allow` 由 r（读取）c（新建）u（修改）d（删除）组成，空串表示无权限。
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
    pub path: String,
    pub allow: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub deny_paths: Vec<String>,
    /// 只读路径（兼容旧配置）：等价于 `[[policy.paths]]` 中 `allow = "r"` 的规则。
    #[serde(default)]
    pub readonly_paths: Vec<String>,
    /// 按 CRUD 细分的路径权限，最具体的规则优先。
    #[serde(default)]
    pub paths: Vec<PathRule>,
    #[serde(default)]
    pub deny_commands: Vec<String>,
    #[serde(default = "d_timeout")]
    pub default_timeout_secs: u64,
    #[serde(default = "d_max_timeout")]
    pub max_timeout_secs: u64,
    #[serde(default = "d_read_kb")]
    pub max_read_kb: u64,
    #[serde(default = "d_write_kb")]
    pub max_write_kb: u64,
    #[serde(default = "d_transfer_mb")]
    pub max_transfer_mb: u64,
    #[serde(default = "d_max_jobs")]
    pub max_jobs: usize,
    #[serde(default = "d_job_timeout")]
    pub job_timeout_secs: u64,
    /// rm 默认移入系统回收站；关闭后改为“备份到数据目录再删除”（同样可 rollback）。
    #[serde(default = "d_true")]
    pub rm_use_trash: bool,
    /// 预热的空闲 PowerShell 数量（仅默认 shell 为 pwsh / powershell 时生效），0 关闭。
    /// 不填时按内存自动决定，见 [`resolve_prewarm`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prewarm_shells: Option<usize>,
    /// 预热池连续多少秒无人取用就释放全部空闲进程（下一次 exec 冷启动并重新补满），0 表示常驻不释放。
    #[serde(default = "d_prewarm_idle")]
    pub prewarm_idle_secs: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            deny_paths: Vec::new(),
            readonly_paths: Vec::new(),
            paths: Vec::new(),
            deny_commands: Vec::new(),
            default_timeout_secs: d_timeout(),
            max_timeout_secs: d_max_timeout(),
            max_read_kb: d_read_kb(),
            max_write_kb: d_write_kb(),
            max_transfer_mb: d_transfer_mb(),
            max_jobs: d_max_jobs(),
            job_timeout_secs: d_job_timeout(),
            rm_use_trash: true,
            prewarm_shells: None,
            prewarm_idle_secs: d_prewarm_idle(),
        }
    }
}

fn default_listen() -> String {
    format!("0.0.0.0:{DEFAULT_PORT}")
}
fn d_true() -> bool {
    true
}
fn d_timeout() -> u64 {
    600
}
fn d_max_timeout() -> u64 {
    3600
}
fn d_read_kb() -> u64 {
    512
}
fn d_write_kb() -> u64 {
    8192
}
fn d_transfer_mb() -> u64 {
    2048
}
fn d_max_jobs() -> usize {
    16
}
fn d_prewarm_idle() -> u64 {
    600
}
/// 自动预热数量：每多少 GB 内存一个，以及上下限。
const PREWARM_GB_PER_SHELL: u64 = 8;
const PREWARM_AUTO_MIN: usize = 1;
const PREWARM_AUTO_MAX: usize = 2;

/// 计算实际预热数量。`configured` 为配置值（None 表示自动），`mem_gb` 为物理内存 GB 数（取不到为 None）。
/// - 自动：每 8 GB 一个，限制在 1～2（16 GB 及以上为 2）；内存未知时取 1。
/// - 手动：按配置，但不超过内存 GB 数（至少允许 1 个）；内存未知时不做限制。
pub fn resolve_prewarm(configured: Option<usize>, mem_gb: Option<u64>) -> usize {
    match (configured, mem_gb) {
        (None, Some(gb)) => {
            ((gb / PREWARM_GB_PER_SHELL) as usize).clamp(PREWARM_AUTO_MIN, PREWARM_AUTO_MAX)
        }
        (None, None) => PREWARM_AUTO_MIN,
        (Some(n), Some(gb)) => n.min((gb as usize).max(1)),
        (Some(n), None) => n,
    }
}

fn d_job_timeout() -> u64 {
    86_400
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

/// 上次 serve 成功启动时的配置副本：config.toml 改坏时 serve 回退到它，避免机器失联。
pub fn last_good_path(home: &Path) -> PathBuf {
    home.join("config.toml.last-good")
}

/// 把当前 config.toml 保存为 last-good（内容相同则不写）。失败不影响主流程。
pub fn save_last_good(home: &Path) {
    let Ok(text) = fs::read(config_path(home)) else {
        return;
    };
    let lg = last_good_path(home);
    if fs::read(&lg).ok().as_deref() == Some(text.as_slice()) {
        return;
    }
    if crate::util::atomic_write(&lg, &text).is_ok() {
        crate::util::restrict_permissions(&lg);
    }
}

pub fn load_config(home: &Path) -> Result<Config> {
    load_config_file(&config_path(home))
}

pub fn load_config_file(p: &Path) -> Result<Config> {
    let text = fs::read_to_string(p).map_err(|e| {
        AppError::config(format!(
            "failed to read {}: {e}. Run `agent-cli-x init` first",
            p.display()
        ))
    })?;
    let cfg: Config = toml::from_str(&text)
        .map_err(|e| AppError::config(format!("failed to parse {}: {e}", p.display())))?;
    if std::net::ToSocketAddrs::to_socket_addrs(&cfg.listen.as_str()).is_err() {
        return Err(AppError::config(format!(
            "{}: listen = `{}` is not a valid IP:port",
            p.display(),
            cfg.listen
        )));
    }
    if cfg.token.len() < 16 {
        return Err(AppError::config("token too short (min 16 characters)"));
    }
    for name in cfg.workspaces.keys() {
        validate_name(name, "workspace")?;
    }
    if let Some(n) = &cfg.node_name {
        validate_name(n, "machine")?;
    }
    Ok(cfg)
}

impl Config {
    pub fn node_name(&self) -> String {
        self.node_name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(crate::platform::hostname)
    }
}

/// 旧配置没有 node_id 时生成一个并写回文件顶部（顶层键必须位于第一个表之前）。
pub fn ensure_node_id(home: &Path) -> Result<()> {
    let cfg = load_config(home)?;
    if cfg.node_id.as_deref().is_some_and(|s| !s.is_empty()) {
        return Ok(());
    }
    let p = config_path(home);
    let text = fs::read_to_string(&p)?;
    let line = format!(
        "# Unique ID of this machine (auto-generated, usually leave as is)\nnode_id = {}\n\n",
        toml_str(&crate::util::short_id())
    );
    crate::util::atomic_write(&p, format!("{line}{text}").as_bytes())?;
    crate::util::restrict_permissions(&p);
    Ok(())
}

/// 机器名：只允许字母数字和 `_-.`，便于在命令行和 AI 提示中引用。
pub fn default_node_name() -> String {
    let h: String = crate::platform::hostname()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(64)
        .collect();
    if h.is_empty() {
        "node".to_string()
    } else {
        h
    }
}

/// 名称只允许字母数字和 `_-.`；工作区名至少 2 个字符（避免与 Windows 盘符 `C:` 混淆）。
pub fn validate_name(name: &str, kind: &str) -> Result<()> {
    let min = if kind == "workspace" { 2 } else { 1 };
    let ok = name.len() >= min
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(AppError::config(format!(
            "invalid {kind} name `{name}`: need {min}-64 chars, only letters, digits, _ - ."
        )))
    }
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// `init` 生成的带注释配置模板。
pub fn template(
    node_name: &str,
    node_id: &str,
    listen: &str,
    token: &str,
    workspaces: &[(String, String)],
) -> String {
    let mut ws = String::new();
    if workspaces.is_empty() {
        ws.push_str("# proj = \"D:/code/proj\"\n");
    }
    for (n, p) in workspaces {
        ws.push_str(&format!("{n} = {}\n", toml_str(p)));
    }
    let preset = policy_preset();
    format!(
        r#"# agent-cli-x server config (TOML). Restart `agent-cli-x serve` after editing.
# This file contains the token; do not commit or share it.

# Machine name shown to peers in `info` / `peer list`, so the AI can tell machines apart.
# Letters, digits, _ - . only; can be changed any time (restart serve to apply).
node_name = {node_name}

# Unique machine ID (6-char short ID generated by init). Usually leave as is; keeping it after an OS reinstall lets peers recognize this machine.
node_id = {node_id}

# Listen address. To listen on one NIC only, use that NIC's IP.
listen = {listen}

# Access token. Used when a peer registers this machine:
#   agent-cli-x peer add <name> <this-ip:port> --token <this value> --fingerprint <this fingerprint>
token = {token}

# Default workspace: used when a path has no `<workspace>:` prefix. Can be omitted with a single workspace.
# default_workspace = "proj"

# Workspaces peers may access (name = directory). Paths outside workspaces are always denied.
# Names: at least 2 characters; letters, digits, _ - . only
[workspaces]
{ws}
[policy]
# The path and command rules below are presets for this OS ({os_label}); add or remove as needed.
# Path permissions are split by CRUD (commands run by exec are not covered):
#   r read: ls / read / grep / get, and as exec working directory
#   c create: write / put to a non-existent file, create directories
#   u update: write over an existing file, edit, put overwrite
#   d delete: rm
# rollback checks by actual action: restoring content needs u, recreating deleted files/dirs needs c, removing newly created files/dirs needs d.
#
# Path rule syntax (shared by deny_paths and [[policy.paths]]):
#   - relative pattern: matched against the workspace root, e.g. "secrets", "**/.env", "**/*.pem"
#     ("secrets" matches only the top-level secrets; use "**/secrets" for any depth)
#   - "<workspace>:" prefix: applies to that workspace only, e.g. "proj:config/prod"
#   - absolute pattern: matched against the real path, e.g. "C:/", "C:/Users/me/.ssh", "/etc"
#   - a matched directory covers everything under it; supports * ? ** [abc] globs, case-insensitive on Windows
# Note: shell commands run by exec cannot be sandboxed (`cat secrets/x`, `rm x` in a command still work);
# for strong isolation run `agent-cli-x serve` under a dedicated low-privilege account and protect directories with file-system permissions.
# agent-cli-x's own data directory (token, private key) is always denied.

# Hard deny (no permissions): denied if the path or any parent matches; cannot be reopened by [[policy.paths]].
# Good for private keys, password vaults and other files that must never be touched.
deny_paths = [
{deny_paths}]

# Denied commands: matched on whole word boundaries after collapsing whitespace, case-insensitive,
# e.g. "rm -rf /" blocks `rm -rf /` but not `rm -rf /tmp/x`.
# This is a guard against mistakes, not a security boundary.
deny_commands = [
{deny_commands}]

# exec default timeout and maximum (seconds).
default_timeout_secs = 600
max_timeout_secs = 3600

# Max returned by one read / max written by one write (KB).
max_read_kb = 512
max_write_kb = 8192

# Max total size of one put / get transfer (MB).
max_transfer_mb = 2048

# Background jobs (exec --detach): max concurrent jobs, max run time per job (seconds).
max_jobs = 16
job_timeout_secs = 86400

# rm moves files to the system trash by default (Windows Recycle Bin / Linux ~/.local/share/Trash / macOS Trash);
# rollback restores from the trash (on macOS restore manually from Trash). If the trash is unavailable it falls back to
# "back up to the data directory, then delete" (limited by max_transfer_mb). Set false to always use backup.
# `rm --permanent` deletes for good: no trash, no backup, unrecoverable.
rm_use_trash = true

# Number of idle shells kept warm: only when the default shell is pwsh / powershell (sh / bash / cmd start fast anyway).
# Each command still runs in a fresh process, isolated; this only saves PowerShell cold start (pwsh ~0.3s, 5.1 ~2s).
# Each idle process uses ~25-55 MB; above this concurrency it falls back to cold start.
# If unset, chosen from physical memory: 1 below 16 GB, 2 at 16 GB or more.
# When set manually it may exceed 2 but not the memory size in GB (e.g. 64 GB allows up to 64); 0 disables.
# prewarm_shells = 2
# Release all idle warm shells after this many seconds without exec; the next exec cold-starts and refills. 0 = keep forever.
prewarm_idle_secs = 600

# Path permissions (CRUD): allow is made of r c u d, e.g. "r" read-only, "rc" read+create, "rcu" no delete, "" none.
# Paths matched by no rule have all permissions (rcud).
# When several rules match, the closest (most specific) level wins, e.g. "C:/" = r and "C:/Users/me/code" = rcud:
# create/modify/delete allowed under C:/Users/me/code, rest of C: read-only; several matches at one level are intersected.
# A workspace under a read-only directory is read-only as a whole; to write, add a more specific rule (see example below) or remove the preset.
# Note: keep this section at the end of [policy] (plain keys after [[policy.paths]] in TOML belong to the last rule).
{path_rules}"#,
        node_name = toml_str(node_name),
        node_id = toml_str(node_id),
        listen = toml_str(listen),
        token = toml_str(token),
        os_label = preset.label,
        deny_paths = toml_list(preset.deny_paths),
        path_rules = path_rules_toml(&preset),
        deny_commands = toml_list(preset.deny_commands),
    )
}

/// 按操作系统区分的默认策略预设（仅用于 `init` 生成配置模板）。
pub struct PolicyPreset {
    pub label: &'static str,
    pub deny_paths: &'static [&'static str],
    /// 预设为只读（allow = "r"）的系统目录
    pub readonly_paths: &'static [&'static str],
    pub deny_commands: &'static [&'static str],
    /// 模板里注释掉的“放开更具体目录”示例
    pub example_path: &'static str,
}

/// 渲染预设的 `[[policy.paths]]` 规则以及一条注释掉的示例。
fn path_rules_toml(preset: &PolicyPreset) -> String {
    let mut s = String::new();
    for p in preset.readonly_paths {
        s.push_str(&format!(
            "[[policy.paths]]\npath = {}\nallow = \"r\"\n\n",
            toml_str(p)
        ));
    }
    s.push_str(&format!(
        "# Example: allow create / modify (no delete) on a specific directory\n\
         # [[policy.paths]]\n# path = {}\n# allow = \"rcu\"\n",
        toml_str(preset.example_path)
    ));
    s
}

pub fn policy_preset() -> PolicyPreset {
    if cfg!(windows) {
        PolicyPreset {
            label: "Windows",
            deny_paths: &[
                "**/.env",
                "**/*.pem",
                "**/*.key",
                "**/id_rsa*",
                "**/id_ed25519*",
                // 注册表配置单元（含 SAM 账户数据库）
                "C:/Windows/System32/config",
            ],
            // 系统盘整体只读：工作区放在 C 盘时也只能读，需要写入请删掉此条或改成更具体的目录
            readonly_paths: &["C:/"],
            deny_commands: &[
                "format c:",
                "diskpart",
                "bcdedit",
                "vssadmin delete shadows",
                r"rd /s /q c:\",
                r"rmdir /s /q c:\",
                r"del /f /s /q c:\",
                r"del /s /q c:\",
                r"rd /s /q c:\windows",
                r"rmdir /s /q c:\windows",
                r"remove-item -recurse -force c:\",
                r"remove-item c:\ -recurse -force",
                r"remove-item -recurse -force c:\windows",
                "shutdown",
                "restart-computer",
                "stop-computer",
                "git push --force",
                "git push -f",
            ],
            example_path: "C:/Users/me/code",
        }
    } else if cfg!(target_os = "macos") {
        PolicyPreset {
            label: "macOS",
            deny_paths: &[
                "**/.env",
                "**/*.pem",
                "**/*.key",
                "**/id_rsa*",
                "**/id_ed25519*",
                "/private/etc/master.passwd",
                "/private/etc/sudoers",
                "/private/etc/sudoers.d",
            ],
            // /etc、/var 在 macOS 上是 /private 下的符号链接，规则按真实路径匹配
            readonly_paths: &[
                "/System",
                "/Library",
                "/bin",
                "/sbin",
                "/usr",
                "/private/etc",
                "/private/var/db",
                "/Applications",
            ],
            deny_commands: UNIX_DENY_COMMANDS,
            example_path: "/usr/local/etc",
        }
    } else {
        PolicyPreset {
            label: "Linux",
            deny_paths: &[
                "**/.env",
                "**/*.pem",
                "**/*.key",
                "**/id_rsa*",
                "**/id_ed25519*",
                "/etc/shadow",
                "/etc/gshadow",
                "/etc/sudoers",
                "/etc/sudoers.d",
                "/root/.ssh",
            ],
            readonly_paths: &[
                "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/usr", "/etc", "/boot", "/var/lib",
                "/proc", "/sys", "/dev",
            ],
            deny_commands: UNIX_DENY_COMMANDS,
            example_path: "/etc/nginx",
        }
    }
}

const UNIX_DENY_COMMANDS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "rm -fr /",
    "rm -rf ~",
    "--no-preserve-root",
    "rm -rf /etc",
    "rm -rf /usr",
    "rm -rf /bin",
    "rm -rf /sbin",
    "rm -rf /lib",
    "rm -rf /boot",
    "rm -rf /var",
    "rm -rf /home",
    "chmod -r 777 /",
    "mkfs",
    "shutdown",
    "reboot",
    "halt",
    "poweroff",
    "init 0",
    "init 6",
    "git push --force",
    "git push -f",
];

/// 渲染成 TOML 数组的多行内容（每项一行、两空格缩进、末尾逗号）。
fn toml_list(items: &[&str]) -> String {
    items
        .iter()
        .map(|i| format!("  {},\n", toml_str(i)))
        .collect()
}

// ---------------- peers.toml ----------------

#[derive(Serialize, Deserialize, Default)]
pub struct Peers {
    #[serde(default)]
    pub peers: BTreeMap<String, Peer>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Peer {
    pub addr: String,
    pub token: String,
    pub fingerprint: String,
    /// 登记时对端报告的机器名与 ID（仅用于展示与核对）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// 对端系统、架构与默认 shell 的缓存，让调用方不必每次先请求 info。
    /// 在 `peer add`、`info`、`upgrade` 成功后刷新；旧版登记的对端可能缺失。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// `peer list` 上次尝试补全上面缓存的时间（Unix 秒）。补不全（离线或对端太旧）时
    /// 一段时间内不再重试，避免每次 `peer list` 都卡在连接超时上。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_tried: Option<i64>,
}

impl Peer {
    /// 用对端 `/v1/info` 的结果更新缓存字段，返回是否有变化。
    /// info 中缺失的字段（如旧版本对端）保留原值，不会被清空。
    pub fn absorb_info(&mut self, info: &serde_json::Value) -> bool {
        let mut changed = false;
        for (slot, key) in [
            (&mut self.node_name, "node_name"),
            (&mut self.node_id, "node_id"),
            (&mut self.os, "os"),
            (&mut self.arch, "arch"),
            (&mut self.shell, "shell"),
        ] {
            if let Some(v) = info[key].as_str() {
                if slot.as_deref() != Some(v) {
                    *slot = Some(v.to_string());
                    changed = true;
                }
            }
        }
        changed
    }
}

pub fn peers_path(home: &Path) -> PathBuf {
    home.join("peers.toml")
}

pub fn load_peers(home: &Path) -> Result<Peers> {
    let p = peers_path(home);
    match fs::read_to_string(&p) {
        Ok(t) => toml::from_str(&t)
            .map_err(|e| AppError::config(format!("failed to parse {}: {e}", p.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Peers::default()),
        Err(e) => Err(e.into()),
    }
}

/// 把若干对端最新的 info 写回 `peers.toml`（只在有变化时写盘）。
/// 属于顺带刷新缓存，调用方应忽略其错误，不影响主流程。
pub fn remember_peer_infos(home: &Path, items: &[(&str, &serde_json::Value)]) -> Result<()> {
    let mut peers = load_peers(home)?;
    let mut changed = false;
    for (name, info) in items {
        if let Some(p) = peers.peers.get_mut(*name) {
            changed |= p.absorb_info(info);
        }
    }
    if changed {
        save_peers(home, &peers)?;
    }
    Ok(())
}

pub fn save_peers(home: &Path, peers: &Peers) -> Result<()> {
    fs::create_dir_all(home)?;
    let p = peers_path(home);
    let body = toml::to_string(peers).map_err(|e| AppError::internal(e.to_string()))?;
    let text =
        format!("# Maintained by `agent-cli-x peer add/remove`. Contains peer tokens; do not share.\n\n{body}");
    crate::util::atomic_write(&p, text.as_bytes())?;
    crate::util::restrict_permissions(&p);
    Ok(())
}

// ---------------- tokens.toml ----------------

/// config.toml 里 `token` 的名称；`tokens.toml` 中不能使用。
pub const DEFAULT_TOKEN_NAME: &str = "default";

/// 按调用方分别签发的 token（`agent-cli-x token add`）。只保存 sha256，不保存明文。
#[derive(Serialize, Deserialize, Default)]
pub struct Tokens {
    #[serde(default)]
    pub tokens: BTreeMap<String, TokenEntry>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TokenEntry {
    pub sha256: String,
    #[serde(default)]
    pub created: String,
    /// 禁止借道：此调用方发起的 exec / 后台任务里不能再调用 agent-cli-x 访问其他节点
    /// 或管理本机凭据（防 AI 误操作，不是安全边界）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_relay: bool,
}

/// exec 子进程中的标记环境变量：值为被禁止借道的调用方名称。
/// 客户端检测到它时只允许不联网、不涉及凭据的命令。
pub const NO_RELAY_ENV: &str = "AGENT_CLI_X_NO_RELAY";

pub fn tokens_path(home: &Path) -> PathBuf {
    home.join("tokens.toml")
}

pub fn load_tokens(home: &Path) -> Result<Tokens> {
    let p = tokens_path(home);
    match fs::read_to_string(&p) {
        Ok(t) => toml::from_str(&t)
            .map_err(|e| AppError::config(format!("failed to parse {}: {e}", p.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Tokens::default()),
        Err(e) => Err(e.into()),
    }
}

pub fn save_tokens(home: &Path, tokens: &Tokens) -> Result<()> {
    fs::create_dir_all(home)?;
    let p = tokens_path(home);
    let body = toml::to_string(tokens).map_err(|e| AppError::internal(e.to_string()))?;
    let text = format!(
        "# Maintained by `agent-cli-x token add/remove/rotate`: per-caller access tokens (sha256 only).\n\
# Changes while serve is running apply within seconds; no restart needed.\n\n{body}"
    );
    crate::util::atomic_write(&p, text.as_bytes())?;
    crate::util::restrict_permissions(&p);
    Ok(())
}

/// 只读取 config.toml 里的 token（serve 热加载用，不做完整校验）。
pub fn read_config_token(home: &Path) -> Option<String> {
    let text = fs::read_to_string(config_path(home)).ok()?;
    let v: toml::Value = toml::from_str(&text).ok()?;
    v.get("token")?
        .as_str()
        .filter(|t| t.len() >= 16)
        .map(|s| s.to_string())
}

/// 替换 config.toml 中顶层 `token = ...` 这一行，保留其余内容与注释。
pub fn replace_config_token(home: &Path, token: &str) -> Result<()> {
    set_top_level(home, "token", &toml_str(token))
}

/// 修改 config.toml 中的 `listen`（保留注释），写入后校验能解析，否则还原。
pub fn replace_config_listen(home: &Path, listen: &str) -> Result<()> {
    let p = config_path(home);
    let before = fs::read_to_string(&p)?;
    set_top_level(home, "listen", &toml_str(listen))?;
    if let Err(e) = load_config(home) {
        let _ = crate::util::atomic_write(&p, before.as_bytes());
        return Err(e);
    }
    Ok(())
}

/// 替换顶层 `key = ...` 这一行；没有时插入到第一个表之前（顶层键必须位于表之前）。
fn set_top_level(home: &Path, key: &str, value: &str) -> Result<()> {
    let p = config_path(home);
    let text = fs::read_to_string(&p)?;
    let out = set_top_level_text(&text, key, value);
    crate::util::atomic_write(&p, out.as_bytes())?;
    crate::util::restrict_permissions(&p);
    Ok(())
}

fn set_top_level_text(text: &str, key: &str, value: &str) -> String {
    let new_line = format!("{key} = {value}");
    let re =
        regex::Regex::new(&format!(r"(?m)^{}\s*=.*$", regex::escape(key))).expect("static regex");
    // 只在第一个表之前查找，避免误改某个表里的同名键
    let table = regex::Regex::new(r"(?m)^\s*\[").expect("static regex");
    let head_end = table.find(text).map_or(text.len(), |m| m.start());
    let (head, tail) = text.split_at(head_end);
    if re.is_match(head) {
        let head = re.replacen(head, 1, regex::NoExpand(&new_line));
        return format!("{head}{tail}");
    }
    let sep = if head.is_empty() || head.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{head}{sep}{new_line}\n\n{tail}")
}

/// 把地址中的端口换成 `port`：`10.0.0.5:47821` → `10.0.0.5:48000`，`[::]:1` → `[::]:48000`。
pub fn with_port(addr: &str, port: u16) -> String {
    let host = match addr.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
        _ => addr,
    };
    format!("{host}:{port}")
}

/// `10.0.0.5` → `10.0.0.5:47821`
pub fn normalize_addr(addr: &str) -> String {
    let a = addr.trim();
    let has_port = if let Some(rest) = a.strip_prefix('[') {
        rest.contains("]:")
    } else {
        a.matches(':').count() == 1
    };
    if has_port {
        a.to_string()
    } else {
        format!("{a}:{DEFAULT_PORT}")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn prewarm_auto_and_cap() {
        use super::resolve_prewarm as r;
        // 自动：每 8 GB 一个，1～2
        assert_eq!(r(None, Some(2)), 1);
        assert_eq!(r(None, Some(8)), 1);
        assert_eq!(r(None, Some(15)), 1);
        assert_eq!(r(None, Some(16)), 2);
        assert_eq!(r(None, Some(64)), 2);
        assert_eq!(r(None, None), 1);
        // 手动：可超过自动上限，但不超过内存 GB 数
        assert_eq!(r(Some(0), Some(64)), 0);
        assert_eq!(r(Some(10), Some(64)), 10);
        assert_eq!(r(Some(100), Some(64)), 64);
        assert_eq!(r(Some(3), Some(0)), 1);
        assert_eq!(r(Some(7), None), 7);
    }

    use super::*;

    #[test]
    fn template_parses() {
        let t = template(
            "dev-01",
            "00000000-0000-4000-8000-000000000000",
            "0.0.0.0:47821",
            "0123456789abcdef0123",
            &[("proj".into(), r"D:\code\x".into())],
        );
        let cfg: Config = toml::from_str(&t).unwrap();
        assert_eq!(cfg.workspaces["proj"], r"D:\code\x");
        assert!(cfg.policy.deny_paths.iter().any(|p| p == "**/.env"));
        // 预设与模板渲染结果一致
        let preset = policy_preset();
        let ro: Vec<&str> = cfg.policy.paths.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(ro, preset.readonly_paths);
        assert!(cfg.policy.paths.iter().all(|r| r.allow == "r"));
        assert_eq!(cfg.policy.deny_commands, preset.deny_commands);
        // [[policy.paths]] 之后的键不能误入规则表（deny_unknown_fields 会报错），这里确认其他字段仍在 [policy]
        assert_eq!(cfg.policy.max_jobs, 16);
    }

    #[test]
    fn top_level_keys() {
        let t = "# c\nlisten = \"0.0.0.0:1\" # x\ntoken = \"t\"\n\n[policy]\nlisten = \"keep\"\n";
        let o = set_top_level_text(t, "listen", "\"0.0.0.0:2\"");
        assert_eq!(o, t.replacen("\"0.0.0.0:1\" # x", "\"0.0.0.0:2\"", 1));
        // 缺失时插到第一个表之前，表里的同名键不动
        let t2 = "token = \"t\"\n[policy]\nlisten = \"keep\"\n";
        let o = set_top_level_text(t2, "listen", "\"a:3\"");
        assert_eq!(
            o,
            "token = \"t\"\nlisten = \"a:3\"\n\n[policy]\nlisten = \"keep\"\n"
        );
        let cfg = template("n", "id", "0.0.0.0:47821", "0123456789abcdef0123", &[]);
        let o = set_top_level_text(&cfg, "listen", &toml_str("0.0.0.0:48000"));
        assert_eq!(
            toml::from_str::<Config>(&o).unwrap().listen,
            "0.0.0.0:48000"
        );
    }

    #[test]
    fn port_swap() {
        assert_eq!(with_port("10.0.0.5:47821", 48000), "10.0.0.5:48000");
        assert_eq!(with_port("[::]:47821", 9), "[::]:9");
        assert_eq!(with_port("host", 9), "host:9");
    }

    #[test]
    fn addr_normalize() {
        assert_eq!(normalize_addr("10.0.0.5"), "10.0.0.5:47821");
        assert_eq!(normalize_addr("10.0.0.5:9000"), "10.0.0.5:9000");
        assert_eq!(normalize_addr("[::1]:9000"), "[::1]:9000");
    }

    #[test]
    fn name_rules() {
        assert!(validate_name("proj", "workspace").is_ok());
        // 工作区名至少 2 个字符，避免与盘符混淆；对端名允许 1 个字符
        assert!(validate_name("C", "workspace").is_err());
        assert!(validate_name("B", "peer").is_ok());
        assert!(validate_name("a b", "peer").is_err());
    }
}
