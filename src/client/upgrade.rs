//! `agent-cli-x upgrade`：上传新二进制到对端，等待对端以新版本重新上线。

use super::Client;
use crate::error::{AppError, Result};
use crate::{frame, http, util};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// 等待重新上线的时间。Windows 任务计划的重启间隔最短 1 分钟：自行拉起新进程失败、
/// 退回由任务计划重启时，新进程要 60 多秒才出现，所以 Windows 对端多等一些。
const RESTART_WAIT: Duration = Duration::from_secs(60);
const RESTART_WAIT_WINDOWS: Duration = Duration::from_secs(150);

fn file_sha256(p: &Path) -> Result<(u64, String)> {
    let mut f =
        File::open(p).map_err(|e| AppError::not_found(format!("无法读取 {}: {e}", p.display())))?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 256 * 1024];
    let mut n_total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
        n_total += n as u64;
    }
    Ok((n_total, util::hex(ctx.finish().as_ref())))
}

/// 升级一个对端。`binary` 为空时使用本机正在运行的二进制（要求对端系统与架构相同）。
pub fn upgrade(client: &Client, binary: Option<&Path>, force: bool) -> Result<Value> {
    let started = Instant::now();
    let info = client.call("/v1/info", &json!({}), Duration::from_secs(30))?;
    let os = info["os"].as_str().unwrap_or("?").to_string();
    let arch = info["arch"].as_str().unwrap_or("?").to_string();
    let old_version = info["version"].as_str().unwrap_or("?").to_string();
    let bin: PathBuf = match binary {
        Some(p) => p.to_path_buf(),
        None => {
            if os != std::env::consts::OS || arch != std::env::consts::ARCH {
                return Err(AppError::bad_request(format!(
                    "对端 {} 是 {os}/{arch}，本机是 {}/{}，不能直接用本机二进制。\
请用 --binary 指定对端平台的 agent-cli-x（可从 releases 页面下载）",
                    client.name,
                    std::env::consts::OS,
                    std::env::consts::ARCH
                )));
            }
            std::env::current_exe()
                .map_err(|e| AppError::internal(format!("无法确定本机可执行文件: {e}")))?
        }
    };
    let jobs = info["running_jobs"].as_u64().unwrap_or(0);
    // 新版本服务端的后台任务由独立进程托管，重启不受影响；旧版本或 systemd 杀整个 cgroup 时才需要 --force
    if jobs > 0 && !force && info["jobs_survive_restart"] != true {
        return Err(AppError::conflict(format!(
            "{} 有 {jobs} 个后台任务正在运行，升级重启会中断它们（状态变为 lost）。等任务结束后再升级（job list 查看），或加 --force 强制升级",
            client.name
        )));
    }
    let (size, sha) = file_sha256(&bin)?;
    let header = frame::header_bytes(&json!({"size": size, "sha256": sha, "force": force}));
    let len = header.len() as u64 + size;
    let resp = client.upload("/v1/upgrade", len, UPLOAD_TIMEOUT, |s| {
        s.write_all(&header)?;
        let mut f = File::open(&bin)?;
        frame::copy_exact(&mut f, s, size)?;
        s.flush()?;
        Ok(())
    })?;
    let new_version = resp["new_version"].as_str().unwrap_or("?").to_string();

    let after = wait_back(
        client,
        &info,
        &format!("{} 已替换为 {new_version}", client.name),
    )?;
    let running = after["version"].as_str().unwrap_or("?").to_string();
    if running != new_version {
        return Err(AppError::internal(format!(
            "{} 已重启，但运行的版本是 {running}，不是刚上传的 {new_version}。\
可能服务管理器启动的是另一个路径的 agent-cli-x（新版本已写入 {}）",
            client.name,
            resp["path"].as_str().unwrap_or("?")
        )));
    }
    // 升级后顺带刷新本地缓存的对端系统信息（失败不影响升级结果）
    let _ = crate::config::remember_peer_infos(
        &crate::config::home_dir(),
        &[(client.name.as_str(), &after)],
    );
    Ok(json!({
        "peer": client.name,
        "old_version": old_version,
        "new_version": new_version,
        "local_version": http::VERSION,
        "restart": resp["restart"],
        "interrupted_jobs": resp["interrupted_jobs"],
        "kept_jobs": resp["kept_jobs"],
        "ms": started.elapsed().as_millis() as u64,
    }))
}

/// 等待对端以新进程重新上线（pid 与重启前的 `before` 不同）。超时后再复查一次，
/// 仍未确认则返回 timeout（104）：重启已发出，只是还没确认，稍后用 info 查看即可。
pub fn wait_back(client: &Client, before: &Value, done: &str) -> Result<Value> {
    let old_pid = &before["pid"];
    let wait = if before["os"] == "windows" {
        RESTART_WAIT_WINDOWS
    } else {
        RESTART_WAIT
    };
    let deadline = Instant::now() + wait;
    let probe = |timeout| {
        client
            .call("/v1/info", &json!({}), timeout)
            .ok()
            .filter(|v| v["pid"] != *old_pid)
    };
    loop {
        thread::sleep(Duration::from_millis(500));
        if let Some(v) = probe(Duration::from_secs(10)) {
            return Ok(v);
        }
        if Instant::now() > deadline {
            break;
        }
    }
    // 复查：慢的机器可能刚好在截止时间之后上线
    thread::sleep(Duration::from_secs(5));
    if let Some(v) = probe(Duration::from_secs(20)) {
        return Ok(v);
    }
    Err(AppError::new(
        "timeout",
        format!(
            "{done}，但 {} 秒内没有确认重新上线（可能仍在启动）。稍后执行 `agent-cli-x info {}` 确认；仍连不上时检查对端的开机自启/服务管理器与 ~/.agent-cli-x/serve-logs/ 下的日志",
            wait.as_secs() + 5,
            client.name
        ),
    ))
}
