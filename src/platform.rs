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
                    "unknown shell `{other}`; options: pwsh / powershell / cmd / sh / bash"
                )))
            }
        };
        if !cfg!(windows) && matches!(shell, Shell::PowerShell | Shell::Cmd) {
            return Err(AppError::bad_request(format!(
                "{} is only available on Windows",
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
pub fn apply_spawn_flags(c: &mut Command) {
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
                    "command too long (over ~30000 chars); write it to a script file with `write` first, then run it",
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
            // Job Object 并不总能管住子孙进程（例如 Microsoft Store 版 pwsh 的子进程不在 Job 里），
            // 先按父子关系找出整棵进程树逐个结束，Job Object 作为兜底
            kill_descendants(child.id());
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

/// 结束 `root` 的所有子孙进程（不含 `root` 本身）。按进程快照里的父子关系展开，
/// 并要求子进程的创建时间不早于父进程，避免父进程退出后 PID 被复用导致误杀。
#[cfg(windows)]
fn kill_descendants(root: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE,
    };
    // (pid, ppid)
    let mut procs: Vec<(u32, u32)> = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                procs.push((e.th32ProcessID, e.th32ParentProcessID));
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    let created = |pid: u32| -> Option<u64> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut t: [FILETIME; 4] = std::mem::zeroed();
            let [c, x, k, u] = &mut t;
            let ok = GetProcessTimes(h, c, x, k, u);
            CloseHandle(h);
            (ok != 0)
                .then(|| (u64::from(t[0].dwHighDateTime) << 32) | u64::from(t[0].dwLowDateTime))
        }
    };
    let mut tree: Vec<(u32, Option<u64>)> = vec![(root, created(root))];
    let mut i = 0;
    while i < tree.len() {
        let (parent, parent_created) = tree[i];
        for &(pid, ppid) in &procs {
            if ppid != parent || pid == parent || pid == 0 || tree.iter().any(|t| t.0 == pid) {
                continue;
            }
            let c = created(pid);
            if let (Some(p), Some(c)) = (parent_created, c) {
                if c < p {
                    continue;
                }
            }
            tree.push((pid, c));
        }
        i += 1;
    }
    for &(pid, _) in tree.iter().skip(1) {
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
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

/// 运行 serve（也就是 exec 命令）的用户名。
pub fn current_user() -> String {
    let from_env = || {
        std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()
            .filter(|s| !s.is_empty())
    };
    #[cfg(unix)]
    {
        // 以 sudo / systemd 运行时环境变量可能不准，root 以 euid 为准
        let uid = unsafe { libc::geteuid() };
        if uid == 0 {
            "root".into()
        } else {
            from_env().unwrap_or_else(|| format!("uid {uid}"))
        }
    }
    #[cfg(not(unix))]
    {
        from_env().unwrap_or_else(|| "?".into())
    }
}

/// 是否以管理员（Windows 提升权限）或 root 身份运行。
#[cfg(unix)]
pub fn is_elevated() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// 是否以管理员（Windows 提升权限）或 root 身份运行。
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elev: TOKEN_ELEVATION = std::mem::zeroed();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elev as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        ) != 0;
        CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

/// 物理内存总量（字节）；取不到时返回 None。
#[cfg(windows)]
pub fn total_memory_bytes() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut st: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let ok = unsafe { GlobalMemoryStatusEx(&mut st) } != 0;
    (ok && st.ullTotalPhys > 0).then_some(st.ullTotalPhys)
}

/// 物理内存总量（字节）；取不到时返回 None。
#[cfg(unix)]
pub fn total_memory_bytes() -> Option<u64> {
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (pages > 0 && size > 0).then(|| pages as u64 * size as u64)
}

/// 物理内存的 GB 数（四舍五入：系统保留部分内存，16 GB 机器常报告 15.8 GB）。
pub fn total_memory_gb() -> Option<u64> {
    const GB: u64 = 1 << 30;
    total_memory_bytes().map(|b| (b + GB / 2) / GB)
}

/// 运行外部命令，返回 (是否成功, 标准输出)。命令不存在时返回 None。
pub fn run_cmd(prog: &str, args: &[&str]) -> Option<(bool, String)> {
    let mut c = Command::new(prog);
    c.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    apply_spawn_flags(&mut c);
    let o = c.output().ok()?;
    // schtasks 等可能输出 UTF-16 或本地代码页；只关心 ASCII 标签和数字，去掉 NUL 即可
    let text = String::from_utf8_lossy(&o.stdout).replace('\0', "");
    Some((o.status.success(), text))
}

/// 启动一个脱离当前进程的子进程（重启后的新 serve、后台任务的执行进程）：
/// Windows 尝试脱离所在的 Job Object（不允许时退回普通方式）、不弹窗口；Unix 新建会话，
/// 不随当前进程组的信号一起退出。`build` 每次返回一个新的、未设置这些标志的命令。
pub fn spawn_detached(build: impl Fn() -> Command) -> std::io::Result<Child> {
    let make = |breakaway: bool| {
        let mut c = build();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // 真正脱离控制台，避免控制台宿主继续持有启动器的重定向管道。
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
            let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
            if breakaway {
                flags |= CREATE_BREAKAWAY_FROM_JOB;
            }
            c.creation_flags(flags);
        }
        #[cfg(unix)]
        {
            let _ = breakaway;
            use std::os::unix::process::CommandExt;
            unsafe {
                c.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        c
    };
    make(true).spawn().or_else(|_| make(false).spawn())
}

/// 对文件加排他锁。`wait` 为 false 时不等待：已被其他进程锁住返回 `Ok(false)`。
/// 锁随文件句柄关闭或进程退出（包括崩溃）自动释放，用来判断持锁进程是否还活着。
pub fn lock_exclusive(f: &std::fs::File, wait: bool) -> std::io::Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
        use windows_sys::Win32::Storage::FileSystem::{
            LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
        };
        let mut flags = LOCKFILE_EXCLUSIVE_LOCK;
        if !wait {
            flags |= LOCKFILE_FAIL_IMMEDIATELY;
        }
        let ok = unsafe {
            let mut ov: windows_sys::Win32::System::IO::OVERLAPPED = std::mem::zeroed();
            LockFileEx(f.as_raw_handle() as _, flags, 0, 1, 0, &mut ov)
        };
        if ok != 0 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            return Ok(false);
        }
        Err(e)
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let mut op = libc::LOCK_EX;
        if !wait {
            op |= libc::LOCK_NB;
        }
        if unsafe { libc::flock(f.as_raw_fd(), op) } == 0 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(false);
        }
        Err(e)
    }
}

/// 当前进程所在 systemd 服务的 KillMode（不在 systemd 服务里时为 None）。
/// 默认的 control-group 会在服务停止/重启时杀掉 cgroup 内所有进程，包括后台任务。
pub fn systemd_kill_mode() -> Option<String> {
    let (unit, user) = systemd_unit()?;
    let mut args = vec!["show", "-p", "KillMode", "--value", unit.as_str()];
    if user {
        args.insert(0, "--user");
    }
    let (ok, out) = run_cmd("systemctl", &args)?;
    let mode = out.trim().to_string();
    (ok && !mode.is_empty()).then_some(mode)
}

/// 当前进程所在的 systemd 服务：(unit 名称, 是否用户级服务)。不在 systemd 服务里时为 None。
pub fn systemd_unit() -> Option<(String, bool)> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cg
        .lines()
        .find(|l| l.starts_with("0::") || l.contains("name=systemd:"))?
        .rsplit(':')
        .next()?
        .to_string();
    let unit = path.rsplit('/').find(|s| s.ends_with(".service"))?;
    Some((unit.to_string(), path.contains("/user@")))
}

/// 等待监听 socket 上有新连接，最多 `timeout_ms` 毫秒。返回 Ok(true) 表示可以 accept。
/// 用于让监听线程定期醒来检查停止标志，关闭监听器时不必依赖“连自己一次”去唤醒
/// （本机防火墙拦截回环连接时那样会唤醒失败，端口一直不释放）。
pub fn wait_readable(listener: &std::net::TcpListener, timeout_ms: i32) -> std::io::Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{WSAPoll, POLLRDNORM, WSAPOLLFD};
        let mut fd = WSAPOLLFD {
            fd: listener.as_raw_socket() as usize,
            events: POLLRDNORM,
            revents: 0,
        };
        let n = unsafe { WSAPoll(&mut fd, 1, timeout_ms) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n > 0)
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let mut fd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            // 被信号打断不算错误
            if e.kind() == std::io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e);
        }
        Ok(n > 0)
    }
}
