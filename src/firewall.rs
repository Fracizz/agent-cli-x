//! Windows 防火墙入站规则（netsh）。推荐按程序放行：升级后路径不变，改端口也无需改规则。
//! 其他系统不检查（ufw/firewalld 等需要 root，由用户自行处理）。

use crate::platform::run_cmd;

pub const RULE: &str = "agent-cli-x";

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Status {
    /// 规则按本程序路径放行（任意端口）
    Program,
    /// 规则按端口放行，且包含该端口
    Port,
    /// 规则存在，但既不是本程序也不含该端口
    OtherPort,
    /// 没有名为 agent-cli-x 的规则
    Missing,
    /// 刚由 serve 自动添加了按程序放行的规则（serve 以管理员身份运行时）
    Added,
    /// 无法运行 netsh
    Unknown,
    /// 非 Windows，不检查
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
            Status::Unknown => "unknown",
            Status::NotChecked => "not_checked",
        }
    }

    /// 明确知道没有放行。
    pub fn blocked(self) -> bool {
        matches!(self, Status::OtherPort | Status::Missing)
    }
}

/// 本程序路径（规则按它放行）。
fn exe_path() -> Option<String> {
    let p = std::env::current_exe().ok()?;
    Some(dunce::simplified(&p).to_string_lossy().to_string())
}

/// 检查入站规则是否放行 `port`。
pub fn check(port: u16) -> Status {
    if !cfg!(windows) {
        return Status::NotChecked;
    }
    let name = format!("name={RULE}");
    match run_cmd(
        "netsh",
        &["advfirewall", "firewall", "show", "rule", &name, "verbose"],
    ) {
        None => Status::Unknown,
        // 没有匹配的规则时 netsh 以非 0 退出
        Some((false, _)) => Status::Missing,
        Some((true, out)) => classify(&out, exe_path().as_deref(), port),
    }
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

/// 按程序放行的 netsh 命令（需管理员执行一次）。
pub fn program_rule_cmd() -> String {
    format!(
        "netsh advfirewall firewall add rule name=\"{RULE}\" dir=in action=allow program=\"{}\" enable=yes",
        exe_path().unwrap_or_else(|| "<agent-cli-x.exe 完整路径>".into())
    )
}

/// 尝试添加按程序放行的规则（只有管理员身份才会成功）。
pub fn add_program_rule() -> bool {
    let Some(exe) = exe_path() else { return false };
    let name = format!("name={RULE}");
    let program = format!("program={exe}");
    matches!(
        run_cmd(
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
        ),
        Some((true, _))
    )
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
}
