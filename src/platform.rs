//! 平台差异：shell 选择、进程树终止、输出回退编码、主机名。

use crate::error::{AppError, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum Shell {
    Pwsh,
    PowerShell,
    Cmd,
    Sh,
    Bash,
}

impl Shell {
    pub fn parse(s: Option<&str>) -> Result<Shell> {
        let v = s.unwrap_or("").trim().to_ascii_lowercase();
        let shell = match v.as_str() {
            "" | "default" => return Ok(default_shell()),
            "pwsh" => Shell::Pwsh,
            "powershell" => Shell::PowerShell,
            "cmd" => Shell::Cmd,
            "sh" => Shell::Sh,
            "bash" => Shell::Bash,
            other => {
                return Err(AppError::bad_request(format!(
                    "未知 shell `{other}`，可选: pwsh / powershell / cmd / sh / bash"
                )))
            }
        };
        if !cfg!(windows) && matches!(shell, Shell::PowerShell | Shell::Cmd) {
            return Err(AppError::bad_request(format!(
                "{} 仅在 Windows 上可用",
                shell.name()
            )));
        }
        Ok(shell)
    }

    pub fn name(&self) -> &'static str {
        match self {
            Shell::Pwsh => "pwsh",
            Shell::PowerShell => "powershell",
            Shell::Cmd => "cmd",
            Shell::Sh => "sh",
            Shell::Bash => "bash",
        }
    }
}

/// Windows：有 pwsh 用 pwsh，否则 Windows PowerShell 5.1；其他平台：/bin/sh。
pub fn default_shell() -> Shell {
    static CACHE: OnceLock<Shell> = OnceLock::new();
    *CACHE.get_or_init(|| {
        if cfg!(windows) {
            if which("pwsh.exe").is_some() {
                Shell::Pwsh
            } else {
                Shell::PowerShell
            }
        } else {
            Shell::Sh
        }
    })
}

pub fn which(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
}

/// PowerShell 包装：强制 UTF-8 输出，并把最后一条语句的成败转换为退出码。
const PS_WRAPPER: &str = "$ProgressPreference='SilentlyContinue'
try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch {}
$OutputEncoding=[System.Text.Encoding]::UTF8
__AGENT_CLI_CMD__
$__agentOk=$?
if ($__agentOk) { exit 0 }
if ($LASTEXITCODE) { exit $LASTEXITCODE }
exit 1
";

/// PowerShell 脚本经此环境变量传给子进程。
const PS_SCRIPT_ENV: &str = "AGENT_CLI_X_PS_SCRIPT";

impl Shell {
    pub fn is_powershell(&self) -> bool {
        matches!(self, Shell::Pwsh | Shell::PowerShell)
    }
}

/// PowerShell 进程：程序名与公共参数。
fn ps_command(shell: Shell) -> Command {
    let mut c = Command::new(if shell == Shell::Pwsh {
        "pwsh"
    } else {
        "powershell"
    });
    c.args(["-NoLogo", "-NoProfile", "-NonInteractive"]);
    if cfg!(windows) {
        c.args(["-ExecutionPolicy", "Bypass"]);
    }
    c
}

/// 所有子进程共用的创建参数：Windows 不弹窗口，Unix 独立进程组（超时可一次杀掉整棵进程树）。
fn apply_spawn_flags(c: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
}

pub fn build_command(shell: Shell, cmd: &str) -> Result<Command> {
    let mut c = match shell {
        Shell::Pwsh | Shell::PowerShell => {
            let script = PS_WRAPPER.replace("__AGENT_CLI_CMD__", cmd);
            // 不用 -EncodedCommand：Windows PowerShell 5.1 在该模式下会把 stderr
            // 序列化成 `#< CLIXML` 格式。改为经环境变量传入脚本、-Command 里 Invoke-Expression：
            // 错误输出保持纯文本，且报错回显的调用行很短；exit 退出码照常透传。
            // 环境变量值上限 32767 个 UTF-16 字符。
            if script.encode_utf16().count() > 30_000 {
                return Err(AppError::bad_request(
                    "命令过长（超过约 30000 字符），请先用 write 写入脚本文件再执行",
                ));
            }
            let mut c = ps_command(shell);
            c.env(PS_SCRIPT_ENV, &script);
            // 读出后立即删除该变量，避免泄漏给用户命令启动的子进程
            c.args([
                "-Command",
                &format!(
                    "$__s=$env:{PS_SCRIPT_ENV}; Remove-Item Env:{PS_SCRIPT_ENV}; Invoke-Expression $__s"
                ),
            ]);
            c
        }
        Shell::Cmd => {
            let mut c = Command::new("cmd.exe");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                c.raw_arg(format!("/d /s /c \"{cmd}\""));
            }
            #[cfg(not(windows))]
            c.arg(cmd);
            c
        }
        Shell::Sh => {
            let mut c = Command::new(if cfg!(windows) { "sh" } else { "/bin/sh" });
            c.arg("-c").arg(cmd);
            c
        }
        Shell::Bash => {
            let mut c = Command::new("bash");
            c.arg("-c").arg(cmd);
            c
        }
    };
    apply_spawn_flags(&mut c);
    Ok(c)
}

/// 预热 shell 的引导脚本：先完成启动和 Invoke-Expression 的首次初始化，
/// 再阻塞读取 stdin 的一行任务（Base64 编码的 UTF-8 脚本）并执行。
/// 服务端退出时管道关闭，ReadLine 返回空，进程自行退出，不会残留。
const PS_WARM_BOOT: &str = "$null=Invoke-Expression '$null=Get-Location'; $__l=[Console]::In.ReadLine(); if (-not $__l) { exit 0 }; $__s=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($__l)); Remove-Variable __l; Invoke-Expression $__s";

/// 构造一个预热 shell（仅 PowerShell；其他 shell 启动足够快，不需要预热）。
/// 返回的命令尚未设置 stdio，由调用方配置。
pub fn build_warm_command(shell: Shell) -> Option<Command> {
    if !shell.is_powershell() {
        return None;
    }
    let mut c = ps_command(shell);
    c.args(["-Command", PS_WARM_BOOT]);
    apply_spawn_flags(&mut c);
    Some(c)
}

/// 交给预热 shell 的任务：设置环境变量与工作目录后执行命令（与冷启动同一个包装脚本）。
/// 所有动态内容都经 Base64 传入，无需处理 PowerShell 引号转义。返回一行（含换行）。
pub fn warm_payload(cwd: &Path, env: &BTreeMap<String, String>, cmd: &str) -> String {
    let lit = |s: &str| {
        format!(
            "([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{}')))",
            crate::util::base64(s.as_bytes())
        )
    };
    let mut script = String::new();
    for (k, v) in env {
        script.push_str(&format!(
            "[Environment]::SetEnvironmentVariable({}, {})\n",
            lit(k),
            lit(v)
        ));
    }
    // Set-Location 影响 PowerShell 与其启动的外部程序；CurrentDirectory 影响 .NET 相对路径
    script.push_str(&format!(
        "Set-Location -LiteralPath {}\n[Environment]::CurrentDirectory=(Get-Location).ProviderPath\n",
        lit(&cwd.to_string_lossy())
    ));
    script.push_str(&PS_WRAPPER.replace("__AGENT_CLI_CMD__", cmd));
    let mut line = crate::util::base64(script.as_bytes());
    line.push('\n');
    line
}

/// 进程树终止句柄：Windows 用 Job Object，Unix 用进程组。
pub struct KillHandle {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(unix)]
    pgid: i32,
}

impl KillHandle {
    #[cfg(windows)]
    pub fn attach(child: &Child) -> Self {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Self { job };
            }
            if AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
                CloseHandle(job);
                return Self {
                    job: std::ptr::null_mut(),
                };
            }
            Self { job }
        }
    }

    #[cfg(unix)]
    pub fn attach(child: &Child) -> Self {
        Self {
            pgid: child.id() as i32,
        }
    }

    pub fn kill(&self, child: &mut Child) {
        #[cfg(windows)]
        unsafe {
            if !self.job.is_null() {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
            }
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.pgid, libc::SIGKILL);
        }
        let _ = child.kill();
    }
}

#[cfg(windows)]
impl Drop for KillHandle {
    fn drop(&mut self) {
        if !self.job.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.job);
            }
        }
    }
}

/// 输出不是 UTF-8 时使用的回退编码（Windows 控制台 OEM 代码页）。
pub fn fallback_encoding() -> Option<&'static encoding_rs::Encoding> {
    #[cfg(windows)]
    {
        let cp = unsafe { windows_sys::Win32::Globalization::GetOEMCP() };
        match cp {
            936 => Some(encoding_rs::GBK),
            950 => Some(encoding_rs::BIG5),
            932 => Some(encoding_rs::SHIFT_JIS),
            949 => Some(encoding_rs::EUC_KR),
            866 => Some(encoding_rs::IBM866),
            1252 | 437 | 850 => Some(encoding_rs::WINDOWS_1252),
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default()
}
