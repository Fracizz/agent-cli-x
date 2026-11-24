//! 入站防火墙规则。只在明确缺规则时动手，没有权限时只给出一条可执行的命令。
//! - Windows：netsh 按程序放行（规则名 agent-cli-x）：升级后路径不变，改端口也无需改规则；
//! - Linux：ufw / firewalld 按端口放行，需要 root。本程序加过的端口记在 `firewall.json`，
//!   换端口后回收；用户自己加的端口不动。

use crate::platform::{self, run_cmd};
use std::path::Path;

pub const RULE: &str = "agent-cli-x";

/// 设为 `off` 时 serve 启动不检查也不修改防火墙（测试、或由其他工具统一管理防火墙时）。
pub const ENV: &str = "AGENT_CLI_X_FIREWALL";

/// 记录本程序在 Linux 上放行过的端口。
const OWNED_FILE: &str = "firewall.json";

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    /// 规则按本程序路径放行（任意端口）
    Program,
    /// 规则按端口放行，且包含该端口
    Port,
    /// 规则存在，但既不是本程序也不含该端口
    OtherPort,
    /// 没有放行该端口的规则
    Missing,
    /// 刚自动添加了规则（以管理员 / root 身份运行时）
    Added,
    /// 没有放行规则，但 Windows 防火墙所有配置文件都已关闭（暂时不影响连接）
    Off,
    /// Linux 防火墙已启用，但非 root 读不到规则
    Unverified,
    /// 无法运行 netsh / ufw / firewall-cmd
    Unknown,
    /// 没有需要处理的防火墙（非 Windows 且未启用 ufw / firewalld）
    NotChecked,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Program => "program",
            Status::Port => "port",
            Status::OtherPort => "other_port",
            Status::Missing => "missing",
            Status::Added => "added",
            Status::Off => "off",
            Status::Unverified => "unverified",
            Status::Unknown => "unknown",
            Status::NotChecked => "not_checked",
        }
    }

    /// 明确知道没有放行。
    pub fn blocked(self) -> bool {
        matches!(self, Status::OtherPort | Status::Missing)
    }

    /// 需要提示用户执行 `firewall open`。
    pub fn needs_action(self) -> bool {
        self.blocked() || self == Status::Unverified
    }

    /// 已放行。
    pub fn allowed(self) -> bool {
        matches!(self, Status::Program | Status::Port | Status::Added)
    }
}

/// 本程序路径（规则按它放行）。
fn exe_path() -> Option<String> {
    let p = std::env::current_exe().ok()?;
    Some(dunce::simplified(&p).to_string_lossy().to_string())
}

/// 用户自己执行一次即可放行的命令。
pub fn open_cmd(port: u16) -> String {
    if cfg!(windows) {
        "agent-cli-x firewall open".into()
    } else {
        // sudo 会换掉 HOME，读不到本用户的 config.toml，所以显式带上端口
        format!(
            "sudo {} firewall open --port {port}",
            exe_path().unwrap_or_else(|| "agent-cli-x".into())
        )
    }
}

/// 一句话说明为什么需要放行（用于 serve 启动、doctor、set-port 的提示）。
pub fn describe(status: Status, port: u16) -> String {
    match status {
        Status::OtherPort => format!("firewall rule {RULE} does not allow port {port}"),
        Status::Missing if cfg!(windows) => format!("no inbound firewall rule {RULE}"),
        Status::Missing => format!("{} does not allow TCP {port}", backend_name()),
        Status::Off => "Windows firewall is off".into(),
        Status::Unverified => format!(
            "{} is enabled; its rules can only be read as root",
            backend_name()
        ),
        _ => format!("firewall: {}", status.name()),
    }
}

/// 自动添加规则后的日志说明。
pub fn added_msg(port: u16) -> String {
    if cfg!(windows) {
        format!("added inbound rule {RULE} for this program (any port)")
    } else {
        format!("allowed TCP {port} in {}", backend_name())
    }
}

/// 自动添加失败时，让用户手动执行的命令。
pub fn manual_cmd(port: u16) -> String {
    #[cfg(windows)]
    {
        let _ = port;
        format!("in an admin PowerShell: {}", program_rule_cmd())
    }
    #[cfg(not(windows))]
    open_cmd(port)
}

/// 检查入站规则是否放行 `port`。
pub fn check(port: u16) -> Status {
    if cfg!(windows) {
        check_windows(port)
    } else {
        check_linux(port)
    }
}

/// serve 启动 / 换端口时调用：缺规则且有权限就加上，返回最终状态。
pub fn ensure(home: &Path, port: u16) -> Status {
    let st = check(port);
    // 防火墙关着时也顺手加上，日后打开防火墙不会断连
    let want = st.blocked() || st == Status::Off;
    if want && platform::is_elevated() && add(home, port) {
        Status::Added
    } else {
        st
    }
}

/// 放行 `port`（需要管理员 / root）。
pub fn add(home: &Path, port: u16) -> bool {
    if cfg!(windows) {
        return add_program_rule();
    }
    let ok = match linux_backend() {
        Some(Backend::Ufw) => run_ok("ufw", &["allow", &format!("{port}/tcp"), "comment", RULE]),
        Some(Backend::Firewalld) => {
            let arg = format!("--add-port={port}/tcp");
            // 永久规则 + 立即生效的运行时规则，不必 reload
            let permanent = run_ok("firewall-cmd", &["--permanent", &arg]);
            run_ok("firewall-cmd", &[&arg]) && permanent
        }
        None => false,
    };
    if ok {
        let mut ports = owned_ports(home);
        if !ports.contains(&port) {
            ports.push(port);
            save_owned(home, &ports);
        }
    }
    ok
}

/// 换端口后回收不再使用的 `port`。只删本程序加的规则，失败时静默（下次换端口还会再试）。
/// - Windows：已有按程序放行的规则时，删除同名、只针对旧端口的规则（旧文档建议过按端口放行）；
/// - Linux：只删记录在 firewall.json 里的端口。
pub fn release(home: &Path, port: u16, current: u16) -> bool {
    if port == current || !platform::is_elevated() {
        return false;
    }
    if cfg!(windows) {
        // 规则按程序放行时，任何端口都已覆盖，删掉旧端口规则不会断连
        if check_windows(current) != Status::Program {
            return false;
        }
        let name = format!("name={RULE}");
        let lp = format!("localport={port}");
        return run_ok(
            "netsh",
            &[
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &name,
                "protocol=tcp",
                &lp,
            ],
        );
    }
    let mut ports = owned_ports(home);
    if !ports.contains(&port) {
        return false;
    }
    let ok = remove_linux_port(port);
    if ok {
        ports.retain(|p| *p != port);
        save_owned(home, &ports);
    }
    ok
}

/// 删除本程序加的所有规则（`firewall close`）。返回删掉的条目说明。
pub fn remove_all(home: &Path) -> Vec<String> {
    if cfg!(windows) {
        let name = format!("name={RULE}");
        if run_ok(
            "netsh",
            &["advfirewall", "firewall", "delete", "rule", &name],
        ) {
            return vec![format!("rule {RULE}")];
        }
        return Vec::new();
    }
    let mut removed = Vec::new();
    let mut ports = owned_ports(home);
    ports.retain(|&p| {
        if remove_linux_port(p) {
            removed.push(format!("{p}/tcp"));
            false
        } else {
            true
        }
    });
    save_owned(home, &ports);
    removed
}

fn run_ok(prog: &str, args: &[&str]) -> bool {
    matches!(run_cmd(prog, args), Some((true, _)))
}

// ---------- Windows ----------

fn check_windows(port: u16) -> Status {
    let name = format!("name={RULE}");
    match run_cmd(
        "netsh",
        &["advfirewall", "firewall", "show", "rule", &name, "verbose"],
    ) {
        None => Status::Unknown,
        // 没有匹配的规则时 netsh 以非 0 退出
        Some((false, _)) if windows_firewall_off() => Status::Off,
        Some((false, _)) => Status::Missing,
        Some((true, out)) => match classify(&out, exe_path().as_deref(), port) {
            Status::OtherPort if windows_firewall_off() => Status::Off,
            s => s,
        },
    }
}

/// 三个配置文件（域、专用、公用）的防火墙是否都已关闭。读注册表（`netsh` 的输出随系统语言变化），
/// 组策略设置优先于本地设置。
fn windows_firewall_off() -> bool {
    let value = |key: &str| -> Option<bool> {
        let (ok, out) = run_cmd("reg", &["query", key, "/v", "EnableFirewall"])?;
        if !ok {
            return None;
        }
        let v = out.lines().find(|l| l.contains("EnableFirewall"))?;
        Some(v.split_whitespace().last()? != "0x0")
    };
    ["DomainProfile", "StandardProfile", "PublicProfile"]
        .iter()
        .all(|p| {
            let policy = format!(r"HKLM\SOFTWARE\Policies\Microsoft\WindowsFirewall\{p}");
            let local = format!(
                r"HKLM\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\{p}"
            );
            // 读不到时按开启处理，宁可多提示
            value(&policy).or_else(|| value(&local)) == Some(false)
        })
}

/// 解析 `netsh ... show rule verbose` 的输出。标签随系统语言变化，只看路径与数字。
fn classify(out: &str, exe: Option<&str>, port: u16) -> Status {
    let norm = |s: &str| s.to_lowercase().replace('/', "\\");
    if let Some(exe) = exe {
        if norm(out).contains(&norm(exe)) {
            return Status::Program;
        }
    }
    let want = port.to_string();
    if out.split(|c: char| !c.is_ascii_digit()).any(|t| t == want) {
        return Status::Port;
    }
    Status::OtherPort
}

/// 按程序放行的 netsh 命令（在管理员 PowerShell 里执行一次）。
#[cfg(windows)]
pub fn program_rule_cmd() -> String {
    format!(
        "netsh advfirewall firewall add rule name=\"{RULE}\" dir=in action=allow program=\"{}\" enable=yes",
        exe_path().unwrap_or_else(|| "<full path of agent-cli-x.exe>".into())
    )
}

/// 添加按程序放行的规则（只有管理员身份才会成功）。
fn add_program_rule() -> bool {
    let Some(exe) = exe_path() else { return false };
    let name = format!("name={RULE}");
    let program = format!("program={exe}");
    run_ok(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &name,
            "dir=in",
            "action=allow",
            &program,
            "enable=yes",
        ],
    )
}

/// 以管理员身份重新运行本程序（弹出 UAC），等待其结束并返回退出码。
/// 用户拒绝时返回 `Err(1223)`（ERROR_CANCELLED）。
#[cfg(windows)]
pub fn run_elevated(args: &str) -> std::result::Result<u32, u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, WaitForSingleObject, INFINITE,
    };
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let exe = wide(&exe_path().ok_or(0u32)?);
    let verb = wide("runas");
    let params = wide(args);
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_HIDE;
        if ShellExecuteExW(&mut info) == 0 {
            return Err(GetLastError());
        }
        if info.hProcess.is_null() {
            return Err(0);
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        Ok(code)
    }
}

// ---------- Linux ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Backend {
    Ufw,
    Firewalld,
}

fn linux_backend() -> Option<Backend> {
    if cfg!(windows) {
        return None;
    }
    // --state 不需要 root
    if run_cmd("firewall-cmd", &["--state"]).is_some_and(|(ok, out)| ok && out.trim() == "running")
    {
        return Some(Backend::Firewalld);
    }
    // ufw status 需要 root，但 ufw.conf 所有用户可读
    if std::fs::read_to_string("/etc/ufw/ufw.conf").is_ok_and(|s| ufw_enabled(&s)) {
        return Some(Backend::Ufw);
    }
    None
}

fn backend_name() -> &'static str {
    match linux_backend() {
        Some(Backend::Ufw) => "ufw",
        Some(Backend::Firewalld) => "firewalld",
        None => "firewall",
    }
}

fn check_linux(port: u16) -> Status {
    let Some(b) = linux_backend() else {
        return Status::NotChecked;
    };
    if !platform::is_elevated() {
        return Status::Unverified;
    }
    match b {
        Backend::Ufw => match run_cmd("ufw", &["status"]) {
            Some((true, out)) if ufw_allows(&out, port) => Status::Port,
            Some((true, _)) => Status::Missing,
            _ => Status::Unknown,
        },
        Backend::Firewalld => {
            match run_cmd("firewall-cmd", &[&format!("--query-port={port}/tcp")]) {
                None => Status::Unknown,
                Some((true, _)) => Status::Port,
                Some((false, _)) => Status::Missing,
            }
        }
    }
}

fn remove_linux_port(port: u16) -> bool {
    match linux_backend() {
        Some(Backend::Ufw) => run_ok("ufw", &["delete", "allow", &format!("{port}/tcp")]),
        Some(Backend::Firewalld) => {
            let arg = format!("--remove-port={port}/tcp");
            let permanent = run_ok("firewall-cmd", &["--permanent", &arg]);
            run_ok("firewall-cmd", &[&arg]) && permanent
        }
        // 防火墙已停用：规则不再生效，记录可以直接丢掉
        None => true,
    }
}

/// `/etc/ufw/ufw.conf` 里 `ENABLED=yes` 表示 ufw 已启用。
fn ufw_enabled(conf: &str) -> bool {
    conf.lines().any(|l| {
        let l = l.trim();
        l.strip_prefix("ENABLED=").is_some_and(|v| {
            v.trim_matches(|c| c == '"' || c == '\'')
                .eq_ignore_ascii_case("yes")
        })
    })
}

/// `ufw status` 是否有放行 `port`（`48000/tcp ALLOW ...` 或 `48000 ALLOW ...`）的规则。
fn ufw_allows(out: &str, port: u16) -> bool {
    let tcp = format!("{port}/tcp");
    let any = port.to_string();
    out.lines().any(|l| {
        let mut it = l.split_whitespace();
        it.next().is_some_and(|t| t == tcp || t == any) && it.any(|t| t == "ALLOW")
    })
}

fn owned_ports(home: &Path) -> Vec<u16> {
    std::fs::read_to_string(home.join(OWNED_FILE))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| serde_json::from_value(v["ports"].clone()).ok())
        .unwrap_or_default()
}

fn save_owned(home: &Path, ports: &[u16]) {
    let path = home.join(OWNED_FILE);
    let _ = std::fs::create_dir_all(home);
    if ports.is_empty() {
        let _ = std::fs::remove_file(path);
    } else {
        let _ = std::fs::write(path, serde_json::json!({"ports": ports}).to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_rules() {
        let exe = r"C:\Tools\agent-cli-x.exe";
        let prog = "规则名称: agent-cli-x\n程序: C:\\tools\\agent-cli-x.exe\n本地端口: 任何\n";
        assert_eq!(classify(prog, Some(exe), 48000), Status::Program);
        let port = "Rule Name: agent-cli-x\nLocalPort: 47821\nRemoteIP: 10.0.0.5\n";
        assert_eq!(classify(port, Some(exe), 47821), Status::Port);
        // 47821 里包含 4782，但端口要整段匹配
        assert_eq!(classify(port, Some(exe), 4782), Status::OtherPort);
        assert_eq!(classify(port, None, 48000), Status::OtherPort);
    }

    #[test]
    fn ufw_parsing() {
        assert!(ufw_enabled("# comment\nENABLED=yes\nLOGLEVEL=low\n"));
        assert!(ufw_enabled("ENABLED=\"yes\""));
        assert!(!ufw_enabled("ENABLED=no\n"));
        let status = "Status: active\n\nTo                         Action      From\n--                         ------      ----\n\
22/tcp                     ALLOW       Anywhere\n\
47821/tcp                  ALLOW       Anywhere                   # agent-cli-x\n\
8080                       DENY        Anywhere\n\
9000                       ALLOW       Anywhere\n\
47821/tcp (v6)             ALLOW       Anywhere (v6)\n";
        assert!(ufw_allows(status, 47821));
        assert!(ufw_allows(status, 9000));
        assert!(!ufw_allows(status, 8080));
        assert!(!ufw_allows(status, 4782));
    }

    #[test]
    fn owned_ports_roundtrip() {
        let dir = std::env::temp_dir().join(format!("acx-fw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(owned_ports(&dir).is_empty());
        save_owned(&dir, &[47821, 48000]);
        assert_eq!(owned_ports(&dir), vec![47821, 48000]);
        save_owned(&dir, &[]);
        assert!(!dir.join(OWNED_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
