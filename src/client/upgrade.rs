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
const RESTART_WAIT: Duration = Duration::from_secs(60);

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
    if jobs > 0 && !force {
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

    // 等待对端以新进程重新上线（pid 变化即说明已重启）
    let old_pid = info["pid"].clone();
    let deadline = Instant::now() + RESTART_WAIT;
    let after = loop {
        thread::sleep(Duration::from_millis(500));
        if let Ok(v) = client.call("/v1/info", &json!({}), Duration::from_secs(10)) {
            if v["pid"] != old_pid {
                break v;
            }
        }
        if Instant::now() > deadline {
            return Err(AppError::connect(format!(
                "{} 已替换为 {new_version}，但服务 {} 秒内没有重新上线。\
请检查对端的开机自启/服务管理器配置，以及对端 ~/.agent-cli-x/serve-logs/ 下的日志",
                client.name,
                RESTART_WAIT.as_secs()
            )));
        }
    };
    let running = after["version"].as_str().unwrap_or("?").to_string();
    if running != new_version {
        return Err(AppError::internal(format!(
            "{} 已重启，但运行的版本是 {running}，不是刚上传的 {new_version}。\
可能服务管理器启动的是另一个路径的 agent-cli-x（新版本已写入 {}）",
            client.name,
            resp["path"].as_str().unwrap_or("?")
        )));
    }
    Ok(json!({
        "peer": client.name,
        "old_version": old_version,
        "new_version": new_version,
        "local_version": http::VERSION,
        "restart": resp["restart"],
        "interrupted_jobs": resp["interrupted_jobs"],
        "ms": started.elapsed().as_millis() as u64,
    }))
}
