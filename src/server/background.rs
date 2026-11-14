//! 无窗口后台启动；通过独立的 stdout 管道确认实际就绪，错误保持原退出码。

use crate::error::{AppError, Result};
use crate::platform;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub fn start(home: &Path, wait_port_secs: u64) -> Result<u32> {
    #[cfg(windows)]
    protect_launcher_pipes()?;
    let exe = std::env::current_exe()?;
    let mut child = platform::spawn_detached(|| {
        let mut c = Command::new(&exe);
        c.args(["serve", "--background-child", "--wait-port"])
            .arg(wait_port_secs.to_string())
            .env("AGENT_CLI_X_HOME", home)
            // 启动器已经退出，没有服务管理器负责重新拉起后台进程。
            .env("AGENT_CLI_X_RESTART", "spawn")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        c
    })?;
    let stdout = child.stdout.take().expect("startup pipe");
    let (tx, rx) = mpsc::sync_channel(1);
    let result = (|| {
        std::thread::Builder::new().spawn(move || {
            let mut line = String::new();
            // 限制内部握手的内存；只读一行，成功时不等待长期运行的子进程退出。
            let read = BufReader::new(stdout.take(64 * 1024)).read_line(&mut line);
            let _ = tx.send(read.map(|_| line));
        })?;
        let line = rx
            .recv_timeout(Duration::from_secs(wait_port_secs.saturating_add(30)))
            .map_err(|_| {
                AppError::new("timeout", "background server did not report readiness")
            })??;
        let status: Value = serde_json::from_str(&line)
            .map_err(|_| AppError::io("background server exited without reporting readiness"))?;
        if let (Some(code), Some(message)) = (status["error"].as_str(), status["message"].as_str())
        {
            return Err(AppError::new(code, message));
        }
        if status["pid"].as_u64() != Some(u64::from(child.id())) {
            return Err(AppError::io("invalid background server startup response"));
        }
        Ok(child.id())
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result.map_err(|e| {
        AppError::new(
            &e.code,
            format!(
                "{}; logs: {}; run `agent-cli-x doctor`",
                e.message,
                super::log::dir(home).display()
            ),
        )
    })
}

// Windows 会继承额外的可继承句柄；外层调用者的捕获管道必须在启动前去掉此标志，
// 否则启动器已退出，调用者仍要等常驻服务退出才能读到 EOF。
#[cfg(windows)]
fn protect_launcher_pipes() -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_PIPE};
    for handle in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        unsafe {
            if GetFileType(handle as _) == FILE_TYPE_PIPE
                && SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, 0) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
    }
    Ok(())
}

pub fn notify(result: Result<u32>) -> Result<()> {
    let status = match result {
        Ok(pid) => json!({"pid": pid}),
        Err(e) => json!({"error": e.code, "message": e.message}),
    };
    writeln!(std::io::stdout().lock(), "{status}")?;
    Ok(())
}
