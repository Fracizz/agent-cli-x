//! `agent-cli-x restart` 与 `agent-cli-x set-port`。

use super::upgrade::wait_back;
use super::Client;
use crate::config;
use crate::error::{AppError, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const CALL: Duration = Duration::from_secs(30);

/// 重启对端 serve（不替换二进制）。对端会先校验配置，有误则拒绝重启。
pub fn restart(client: &Client, force: bool) -> Result<Value> {
    let started = Instant::now();
    let before = client.call("/v1/info", &json!({}), CALL)?;
    let resp = client.call("/v1/restart", &json!({"force": force}), CALL)?;
    wait_back(client, &before, &format!("{} 已发出重启", client.name))?;
    Ok(json!({
        "peer": client.name,
        "restart": resp["restart"],
        "interrupted_jobs": resp["interrupted_jobs"],
        "kept_jobs": resp["kept_jobs"],
        "ms": started.elapsed().as_millis() as u64,
    }))
}

/// 更换对端监听端口：对端先在新端口上监听，本机经由新端口确认后才切换；
/// 新端口连不上就撤销，对端继续用旧端口。成功后更新本机登记的地址。
pub fn set_port(client: &Client, port: u16) -> Result<Value> {
    let old_addr = client.addr().to_string();
    let open = client.call("/v1/port", &json!({"action": "open", "port": port}), CALL)?;
    let new_addr = config::with_port(&old_addr, port);
    let commit = client.with_addr(&new_addr).call(
        "/v1/port",
        &json!({"action": "commit", "port": port}),
        CALL,
    );
    if let Err(e) = commit {
        let _ = client.call("/v1/port", &json!({"action": "abort"}), CALL);
        let hint = match open["firewall_cmd"].as_str() {
            Some(cmd) => format!(
                "对端 Windows 防火墙没有放行。在对端管理员 PowerShell 执行一次（按程序放行，以后改端口不用再改）: {cmd}"
            ),
            None => format!(
                "检查对端防火墙（Linux 如 `sudo ufw allow {port}/tcp`）和云安全组是否放行 TCP {port}"
            ),
        };
        return Err(AppError::new(
            &e.code,
            format!(
                "经由新地址 {new_addr} 确认失败（{}），已撤销，{} 仍监听原端口。{hint}",
                e.message, client.name
            ),
        ));
    }
    // 本机登记的、指向同一地址的对端（可能用不同 token 登记了多个名称）一并更新
    let home = config::home_dir();
    let mut peers = config::load_peers(&home)?;
    let mut updated = Vec::new();
    for (n, p) in peers.peers.iter_mut() {
        if p.addr == old_addr {
            p.addr = new_addr.clone();
            updated.push(n.clone());
        }
    }
    config::save_peers(&home, &peers)?;
    Ok(json!({
        "peer": client.name,
        "addr": new_addr,
        "updated_peers": updated,
        "firewall": open["firewall"],
    }))
}
