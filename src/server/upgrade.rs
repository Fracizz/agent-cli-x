//! 远程升级：接收新二进制 → 校验 sha256 并试运行 `--version` → 替换当前可执行文件 → 重启自身。
//!
//! 请求体：一行 JSON 头 `{"size","sha256","force"}`，后跟 size 字节的二进制。
//! 重启方式：
//! - 在 systemd 下（存在 `INVOCATION_ID`）或 `AGENT_CLI_X_RESTART=exit`：以退出码 75 退出，
//!   由服务管理器按 `Restart=` 拉起（systemd 会在服务停止时杀掉整个 cgroup，自己拉起子进程无效）；
//! - 其他情况（Windows 任务计划、手动运行）：启动一个脱离当前进程的新 `serve --wait-port`，再退出。

use super::Ctx;
use crate::error::{AppError, Result};
use crate::{frame, http, platform, util};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MAX_BINARY: u64 = 512 * 1024 * 1024;
const TMP_PREFIX: &str = ".agent-cli-x-upgrade-";
/// systemd 等服务管理器据此判断为异常退出并重启。
pub const RESTART_EXIT_CODE: i32 = 75;

pub fn handle<S: Read + Write>(ctx: &Ctx, len: u64, conn: &mut BufReader<S>) {
    let mut body = conn.by_ref().take(len);
    let res = handle_inner(ctx, len, &mut body);
    if res.is_err() {
        // 读完剩余请求体再回错误：否则连接带着未读数据关闭，客户端只会看到连接被重置
        let _ = std::io::copy(&mut body, &mut std::io::sink());
    }
    let w = conn.get_mut();
    match res {
        Ok(v) => {
            ctx.st
                .audit
                .log(&ctx.ip, &ctx.caller, "upgrade", true, v.clone());
            crate::slog!(
                "已由 {}@{} 升级到 {}，即将重启（方式: {}）",
                ctx.caller,
                ctx.ip,
                v["new_version"].as_str().unwrap_or("?"),
                v["restart"].as_str().unwrap_or("?")
            );
            let _ = http::write_json(w, 200, &v);
            let _ = w.flush();
            schedule_restart();
        }
        Err(e) => {
            ctx.st.audit.log(
                &ctx.ip,
                &ctx.caller,
                "upgrade",
                false,
                json!({"error": e.to_string()}),
            );
            crate::slog!("升级失败（来自 {}@{}）: {e}", ctx.caller, ctx.ip);
            let _ = http::write_error(w, &e);
        }
    }
}

fn handle_inner<R: std::io::BufRead>(ctx: &Ctx, len: u64, body: &mut R) -> Result<Value> {
    if len > MAX_BINARY {
        return Err(AppError::bad_request("二进制文件过大（上限 512 MB）"));
    }
    let meta = frame::read_header(body)?;
    let size = meta["size"]
        .as_u64()
        .ok_or_else(|| AppError::bad_request("缺少 size"))?;
    let want_sha = meta["sha256"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let force = meta["force"].as_bool().unwrap_or(false);

    let (interrupted, kept) = jobs_on_restart(ctx, force, "升级")?;

    // 先确认当前配置能让新进程正常启动，否则替换后会失联
    super::admin::preflight(&ctx.st.home)?;

    let exe = current_exe()?;
    let dir = exe
        .parent()
        .ok_or_else(|| AppError::internal("无法确定可执行文件所在目录"))?;
    let tmp = dir.join(format!(
        "{TMP_PREFIX}{}{}",
        util::random_hex(4),
        std::env::consts::EXE_SUFFIX
    ));
    let res = (|| {
        receive(body, &tmp, size, &want_sha)?;
        let new_version = probe_version(&tmp)?;
        replace(&exe, &tmp)?;
        Ok(new_version)
    })();
    let new_version = match res {
        Ok(v) => v,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    };
    Ok(json!({
        "old_version": http::VERSION,
        "new_version": new_version,
        "path": exe.to_string_lossy(),
        "restart": restart_mode(),
        "interrupted_jobs": interrupted,
        "kept_jobs": kept,
    }))
}

/// 重启后后台任务能否继续运行。任务由脱离 serve 的执行进程托管，自行拉起新进程时不受影响；
/// 由服务管理器重启时，只有 systemd 的 KillMode 为 process/none 才不会连同任务一起杀掉。
pub fn jobs_survive_restart() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        restart_mode() == "spawn"
            || matches!(
                platform::systemd_kill_mode().as_deref(),
                Some("process" | "none")
            )
    })
}

/// 重启前检查后台任务，返回（会被中断的数量，会继续运行的数量）。
pub fn jobs_on_restart(ctx: &Ctx, force: bool, verb: &str) -> Result<(usize, usize)> {
    let running = ctx.st.jobs.running_count();
    if running == 0 {
        return Ok((0, 0));
    }
    if jobs_survive_restart() {
        return Ok((0, running));
    }
    if !force {
        return Err(AppError::conflict(format!(
            "对端有 {running} 个后台任务正在运行。该机器由服务管理器重启，且 systemd 的 KillMode 不是 process，\
{verb}会连同任务一起终止（状态变为 lost）。在 unit 的 [Service] 中加 KillMode=process 并执行 \
`systemctl --user daemon-reload` 后再试；或等任务结束（job list 查看）；或加 --force"
        )));
    }
    Ok((running, 0))
}

/// 当前可执行文件路径（Linux 上被替换过的文件路径会带 ` (deleted)` 后缀，需去掉）。
pub fn current_exe() -> Result<PathBuf> {
    let p = std::env::current_exe()
        .map_err(|e| AppError::internal(format!("无法确定可执行文件路径: {e}")))?;
    let s = p.to_string_lossy();
    Ok(match s.strip_suffix(" (deleted)") {
        Some(t) => PathBuf::from(t),
        None => p,
    })
}

fn receive<R: Read>(body: &mut R, tmp: &Path, size: u64, want_sha: &str) -> Result<()> {
    let mut f = File::create(tmp).map_err(|e| {
        AppError::io(format!(
            "无法在 {} 写入新版本: {e}（检查目录权限）",
            tmp.parent().unwrap_or(tmp).display()
        ))
    })?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 256 * 1024];
    let mut left = size;
    while left > 0 {
        let want = buf.len().min(left as usize);
        let n = body.read(&mut buf[..want])?;
        if n == 0 {
            return Err(AppError::io(format!(
                "传输中断：期望 {size} 字节，实际 {} 字节",
                size - left
            )));
        }
        ctx.update(&buf[..n]);
        f.write_all(&buf[..n])?;
        left -= n as u64;
    }
    f.sync_all()?;
    drop(f);
    let got = util::hex(ctx.finish().as_ref());
    if !want_sha.is_empty() && got != want_sha {
        return Err(AppError::io(format!(
            "sha256 不一致（期望 {want_sha}，实际 {got}），传输可能损坏"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(tmp, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// 试运行新二进制的 `--version`：既确认能在本机运行（平台/架构正确），也拿到新版本号。
fn probe_version(bin: &Path) -> Result<String> {
    let mut c = Command::new(bin);
    c.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    platform::apply_spawn_flags(&mut c);
    // Linux 上其他线程同时 fork 时可能短暂继承写句柄，执行刚写完的文件会报 ETXTBSY，稍后重试即可
    let mut tries = 0;
    let spawned = loop {
        match c.spawn() {
            Err(e) if e.raw_os_error() == Some(26) && cfg!(unix) && tries < 10 => {
                tries += 1;
                thread::sleep(Duration::from_millis(100));
            }
            r => break r,
        }
    };
    let mut child = spawned.map_err(|e| {
        AppError::bad_request(format!(
            "新二进制无法在对端运行: {e}（确认与对端的系统/架构一致，`agent-cli-x info` 可查看）"
        ))
    })?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AppError::bad_request("新二进制运行 --version 超时"));
        }
        thread::sleep(Duration::from_millis(50));
    };
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    let ver = out
        .trim()
        .strip_prefix("agent-cli-x ")
        .map(|v| v.trim().to_string());
    match ver {
        Some(v) if status.success() && !v.is_empty() => Ok(v),
        _ => Err(AppError::bad_request(format!(
            "上传的文件不是有效的 agent-cli-x 二进制（--version 输出: {}）",
            out.trim()
        ))),
    }
}

/// 用新文件替换当前可执行文件。Windows 上运行中的 exe 不能覆盖但可以改名，先挪开旧文件。
fn replace(exe: &Path, new: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let stem = exe
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "agent-cli-x".into());
        let old = exe.with_file_name(format!("{stem}.old-{}.exe", util::random_hex(4)));
        fs::rename(exe, &old)
            .map_err(|e| AppError::io(format!("无法挪开旧版本 {}: {e}", exe.display())))?;
        if let Err(e) = fs::rename(new, exe) {
            let _ = fs::rename(&old, exe);
            return Err(AppError::io(format!("无法放入新版本: {e}")));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(new, exe).map_err(|e| AppError::io(format!("无法替换 {}: {e}", exe.display())))
    }
}

/// 稍后重启自身：留出时间让响应发送完、连接正常关闭。
pub fn schedule_restart() {
    thread::spawn(|| {
        thread::sleep(Duration::from_millis(500));
        restart();
    });
}

pub fn restart_mode() -> &'static str {
    let forced = std::env::var("AGENT_CLI_X_RESTART").unwrap_or_default();
    if forced == "exit" || (forced != "spawn" && std::env::var_os("INVOCATION_ID").is_some()) {
        "exit"
    } else {
        "spawn"
    }
}

fn restart() -> ! {
    if restart_mode() == "spawn" {
        match spawn_detached() {
            Ok(pid) => {
                crate::slog!("已启动新进程 pid {pid}，本进程退出");
                std::process::exit(0);
            }
            Err(e) => crate::slog!(
                "启动新进程失败: {e}；本进程以 {RESTART_EXIT_CODE} 退出，等待服务管理器重启"
            ),
        }
    }
    crate::slog!("以退出码 {RESTART_EXIT_CODE} 退出，由服务管理器重启");
    std::process::exit(RESTART_EXIT_CODE);
}

fn spawn_detached() -> Result<u32> {
    let exe = current_exe()?;
    let child = platform::spawn_detached(|| {
        let mut c = Command::new(&exe);
        c.args(["serve", "--wait-port", "30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c
    })
    .map_err(|e| AppError::io(format!("启动新进程失败: {e}")))?;
    Ok(child.id())
}

/// 清理上次升级留下的旧版本与临时文件（Windows 上旧 exe 可能仍被退出中的进程占用，稍后重试）。
pub fn cleanup_leftovers() {
    let Ok(exe) = current_exe() else { return };
    let Some(dir) = exe.parent().map(Path::to_path_buf) else {
        return;
    };
    let stem = exe
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let old_prefix = format!("{stem}.old-");
    thread::spawn(move || {
        for _ in 0..10 {
            let mut left = 0;
            for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if (name.starts_with(TMP_PREFIX) || name.starts_with(&old_prefix))
                    && fs::remove_file(e.path()).is_err()
                {
                    left += 1;
                }
            }
            if left == 0 {
                return;
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}
