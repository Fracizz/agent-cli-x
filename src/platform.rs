//! 平台差异：shell 选择、进程树终止、输出回退编码、主机名。

use crate::error::{AppError, Result};
use base64::Engine;
use std::path::PathBuf;
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

pub fn build_command(shell: Shell, cmd: &str) -> Result<Command> {
    let mut c = match shell {
        Shell::Pwsh | Shell::PowerShell => {
            let script = PS_WRAPPER.replace("__AGENT_CLI_CMD__", cmd);
            let utf16: Vec<u8> = script
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect();
            let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
            if encoded.len() > 30_000 {
                return Err(AppError::bad_request(
                    "命令过长（超过约 10000 字符），请先用 write 写入脚本文件再执行",
                ));
            }
            let mut c = Command::new(if shell == Shell::Pwsh {
                "pwsh"
            } else {
                "powershell"
            });
            c.args(["-NoLogo", "-NoProfile", "-NonInteractive"]);
            if cfg!(windows) {
                c.args(["-ExecutionPolicy", "Bypass"]);
            }
            c.args(["-EncodedCommand", &encoded]);
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
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 独立进程组，超时时可一次杀掉整棵进程树
        c.process_group(0);
    }
    Ok(c)
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
