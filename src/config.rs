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
    /// 本机唯一 ID（UUID），init 时随机生成。
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

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub deny_paths: Vec<String>,
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
    /// 预热的空闲 PowerShell 数量（仅默认 shell 为 pwsh / powershell 时生效），0 关闭。
    #[serde(default = "d_prewarm")]
    pub prewarm_shells: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            deny_paths: Vec::new(),
            deny_commands: Vec::new(),
            default_timeout_secs: d_timeout(),
            max_timeout_secs: d_max_timeout(),
            max_read_kb: d_read_kb(),
            max_write_kb: d_write_kb(),
            max_transfer_mb: d_transfer_mb(),
            max_jobs: d_max_jobs(),
            job_timeout_secs: d_job_timeout(),
            prewarm_shells: d_prewarm(),
        }
    }
}

fn default_listen() -> String {
    format!("0.0.0.0:{DEFAULT_PORT}")
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
fn d_prewarm() -> usize {
    1
}
fn d_job_timeout() -> u64 {
    86_400
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

pub fn load_config(home: &Path) -> Result<Config> {
    let p = config_path(home);
    let text = fs::read_to_string(&p).map_err(|e| {
        AppError::config(format!(
            "读取 {} 失败: {e}。请先执行 `agent-cli-x init`",
            p.display()
        ))
    })?;
    let cfg: Config = toml::from_str(&text)
        .map_err(|e| AppError::config(format!("解析 {} 失败: {e}", p.display())))?;
    if cfg.token.len() < 16 {
        return Err(AppError::config("token 过短（至少 16 个字符）"));
    }
    for name in cfg.workspaces.keys() {
        validate_name(name, "工作区")?;
    }
    if let Some(n) = &cfg.node_name {
        validate_name(n, "机器")?;
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
        "# 本机唯一 ID（自动生成，一般不要修改）\nnode_id = {}\n\n",
        toml_str(&crate::util::uuid_v4())
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
    let min = if kind == "工作区" { 2 } else { 1 };
    let ok = name.len() >= min
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(AppError::config(format!(
            "{kind}名称 `{name}` 无效：需 {min}~64 个字符，仅限字母、数字、_ - ."
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
    format!(
        r#"# agent-cli-x 服务端配置（TOML）。修改后需重启 `agent-cli-x serve` 生效。
# 本文件包含 token，请勿提交到仓库或分享给他人。

# 本机名称：对端执行 `info` / `peer list` 时看到的机器名，方便 AI 分辨是哪台机器。
# 仅限字母、数字、_ - .，可随时修改（重启 serve 生效）。
node_name = {node_name}

# 本机唯一 ID（init 时随机生成的 UUID）。一般不要修改；重装系统后保留它可让对端认出是同一台机器。
node_id = {node_id}

# 监听地址。只想在某张网卡上监听时改成该网卡 IP。
listen = {listen}

# 访问令牌。对端登记本机时使用：
#   agent-cli-x peer add <名称> <本机IP:端口> --token <此值> --fingerprint <本机指纹>
token = {token}

# 默认工作区：路径没写 `工作区:` 前缀时使用。只有一个工作区时可省略。
# default_workspace = "proj"

# 允许对端访问的工作区（名称 = 目录）。工作区之外的路径一律拒绝。
# 名称至少 2 个字符，仅限字母、数字、_ - .
[workspaces]
{ws}
[policy]
# 黑名单路径（禁止访问）。规则：
#   - 相对模式：相对工作区根目录匹配，如 "secrets"、"**/.env"、"**/*.pem"
#     （"secrets" 只匹配根目录下的 secrets；任意层级请写 "**/secrets"）
#   - 加 "工作区:" 前缀：只作用于该工作区，如 "proj:config/prod"
#   - 绝对模式：按真实路径匹配，如 "C:/Users/me/.ssh"、"/etc"
#   - 命中目录时，目录下所有内容都禁止；支持 * ? ** [abc] 通配，Windows 不区分大小写
# 对 ls / read / write / edit / rollback 以及 exec 的工作目录强制生效。
# 注意：exec 运行的 shell 命令本身无法沙箱化（命令里写 `cat secrets/x` 仍能读到），
# 需要强隔离时请用专门的低权限系统账户运行 `agent-cli-x serve`，并用文件系统权限保护目录。
# agent-cli-x 自己的数据目录（含 token、私钥）始终禁止访问。
deny_paths = [
  "**/.env",
  "**/*.pem",
  "**/*.key",
  "**/id_rsa*",
  "**/id_ed25519*",
]

# 黑名单命令：合并空白、忽略大小写后按“完整词边界”匹配，
# 例如 "rm -rf /" 会拦截 `rm -rf /` 但不会拦截 `rm -rf /tmp/x`。
# 这是防误操作护栏，不是安全边界。
deny_commands = [
  "rm -rf /",
  "rm -rf /*",
  "rm -rf ~",
  "mkfs",
  "shutdown",
  "reboot",
  "format c:",
  "git push --force",
  "git push -f",
]

# exec 默认超时与上限（秒）。
default_timeout_secs = 600
max_timeout_secs = 3600

# read 单次最多返回 / write 单次最多写入（KB）。
max_read_kb = 512
max_write_kb = 8192

# put / get 单次传输总量上限（MB）。
max_transfer_mb = 2048

# 后台任务（exec --detach）：同时运行的最大数量、单个任务最长运行时间（秒）。
max_jobs = 16
job_timeout_secs = 86400

# 预热的空闲 shell 数量：仅当默认 shell 是 pwsh / powershell 时生效（sh / bash / cmd 启动本来就快）。
# 每条命令仍在全新进程中执行、互不影响，只是省掉 PowerShell 的冷启动时间（pwsh 约 0.3 秒，5.1 约 2 秒）。
# 每个空闲进程约占几十 MB 内存；并发超过此数量时自动退回冷启动。设为 0 关闭。
prewarm_shells = 1
"#,
        node_name = toml_str(node_name),
        node_id = toml_str(node_id),
        listen = toml_str(listen),
        token = toml_str(token),
    )
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
            .map_err(|e| AppError::config(format!("解析 {} 失败: {e}", p.display()))),
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
        format!("# 由 `agent-cli-x peer add/remove` 维护，包含对端 token，请勿分享。\n\n{body}");
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
}

pub fn tokens_path(home: &Path) -> PathBuf {
    home.join("tokens.toml")
}

pub fn load_tokens(home: &Path) -> Result<Tokens> {
    let p = tokens_path(home);
    match fs::read_to_string(&p) {
        Ok(t) => toml::from_str(&t)
            .map_err(|e| AppError::config(format!("解析 {} 失败: {e}", p.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Tokens::default()),
        Err(e) => Err(e.into()),
    }
}

pub fn save_tokens(home: &Path, tokens: &Tokens) -> Result<()> {
    fs::create_dir_all(home)?;
    let p = tokens_path(home);
    let body = toml::to_string(tokens).map_err(|e| AppError::internal(e.to_string()))?;
    let text = format!(
        "# 由 `agent-cli-x token add/remove/rotate` 维护：按调用方签发的访问令牌（只存 sha256）。\n\
# serve 运行中修改也会在几秒内生效，无需重启。\n\n{body}"
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
    let p = config_path(home);
    let text = fs::read_to_string(&p)?;
    let re = regex::Regex::new(r"(?m)^token\s*=.*$").expect("静态正则");
    if !re.is_match(&text) {
        return Err(AppError::config(format!(
            "{} 中没有找到 `token = ...` 行",
            p.display()
        )));
    }
    let new_line = format!("token = {}", toml_str(token));
    let out = re
        .replacen(&text, 1, regex::NoExpand(&new_line))
        .to_string();
    crate::util::atomic_write(&p, out.as_bytes())?;
    crate::util::restrict_permissions(&p);
    Ok(())
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
    }

    #[test]
    fn addr_normalize() {
        assert_eq!(normalize_addr("10.0.0.5"), "10.0.0.5:47821");
        assert_eq!(normalize_addr("10.0.0.5:9000"), "10.0.0.5:9000");
        assert_eq!(normalize_addr("[::1]:9000"), "[::1]:9000");
    }

    #[test]
    fn name_rules() {
        assert!(validate_name("proj", "工作区").is_ok());
        // 工作区名至少 2 个字符，避免与盘符混淆；对端名允许 1 个字符
        assert!(validate_name("C", "工作区").is_err());
        assert!(validate_name("B", "对端").is_ok());
        assert!(validate_name("a b", "对端").is_err());
    }
}
