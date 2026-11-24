//! `agent-cli-x restart`、`agent-cli-x set-port` 与本机的 `agent-cli-x stop`。

use super::upgrade::wait_back;
use super::Client;
use crate::config::{self, Config, Peer};
use crate::error::{AppError, Result};
use crate::tls;
use serde_json::{json, Value};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const CALL: Duration = Duration::from_secs(30);

/// 重启对端 serve（不替换二进制）。对端会先校验配置，有误则拒绝重启。
pub fn restart(client: &Client, force: bool) -> Result<Value> {
    let started = Instant::now();
    let before = client.call("/v1/info", &json!({}), CALL)?;
    let resp = client.call("/v1/restart", &json!({"force": force}), CALL)?;
    wait_back(client, &before, &format!("{} restart issued", client.name))?;
    Ok(json!({
        "peer": client.name,
        "restart": resp["restart"],
        "interrupted_jobs": resp["interrupted_jobs"],
        "kept_jobs": resp["kept_jobs"],
        "ms": started.elapsed().as_millis() as u64,
    }))
}

/// 远程登记 / 删除对端工作区：对端校验并改写自己的 config.toml 后重启，本机等它重新上线。
pub fn workspace(client: &Client, body: &Value) -> Result<Value> {
    let before = client.call("/v1/info", &json!({}), CALL)?;
    let mut resp = client.call("/v1/workspace", body, CALL)?;
    wait_back(
        client,
        &before,
        &format!("{} workspace change saved, restart issued", client.name),
    )?;
    resp["peer"] = json!(client.name);
    resp["serve"] = json!("restarted");
    Ok(resp)
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
                "The peer firewall blocks it and serve there lacks admin/root rights to open it. Run once on the peer: {cmd}"
            ),
            None => format!(
                "Check that the network / cloud security group allows TCP {port} to the peer"
            ),
        };
        return Err(AppError::new(
            &e.code,
            format!(
                "Confirmation via new address {new_addr} failed ({}); rolled back, {} still listens on the old port. {hint}",
                // 只取底层错误的第一句，详细的握手提示与这里的防火墙提示重复
                e.message.split(". ").next().unwrap_or_default(),
                client.name
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

/// 把监听地址换成本机可连接的地址（0.0.0.0 → 127.0.0.1）。
pub fn local_connect_addr(listen: &str) -> String {
    let port = listen.rsplit(':').next().unwrap_or_default();
    let host = listen[..listen.len() - port.len()].trim_end_matches(':');
    match host {
        "0.0.0.0" | "" => format!("127.0.0.1:{port}"),
        "[::]" => format!("[::1]:{port}"),
        h => format!("{h}:{port}"),
    }
}

/// 用本机的默认 token 与证书指纹连接本机 serve。
pub fn local_client(cfg: &Config, fingerprint: String) -> Client {
    Client::from_peer(
        "local",
        Peer {
            addr: local_connect_addr(&cfg.listen),
            token: cfg.token.clone(),
            fingerprint,
            ..Default::default()
        },
    )
}

/// 停止本机 serve：经本机连接请求它退出，再确认端口已关闭。没有在运行时不算错误。
pub fn stop(force: bool) -> Result<Value> {
    super::disable_version_warning();
    let home = config::home_dir();
    // config.toml 改坏时 serve 以 last-good 运行，token 也从那里取
    let cfg = config::load_config(&home)
        .or_else(|e| config::load_config_file(&config::last_good_path(&home)).map_err(|_| e))?;
    let me = local_client(&cfg, tls::local_fingerprint(&home)?);
    let addr = me.addr().to_string();
    let resp = match me.call("/v1/stop", &json!({"force": force}), CALL) {
        Ok(v) => v,
        Err(e) if e.code == "connect" => return Ok(json!({"addr": addr, "running": false})),
        Err(e) if e.code == "not_found" => {
            // 正在运行的是不认识 stop 的旧版本
            let info = me.info(CALL)?;
            let pid = &info["pid"];
            let kill = if cfg!(windows) {
                format!("Stop-Process -Id {pid}")
            } else {
                format!("kill {pid}")
            };
            return Err(AppError::not_found(format!(
                "running serve v{} (pid {pid}) predates `stop`; end it with `{kill}`",
                info["version"].as_str().unwrap_or("?")
            )));
        }
        Err(e) => return Err(e),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while accepts(&addr) {
        if Instant::now() > deadline {
            return Err(AppError::new(
                "timeout",
                format!(
                    "serve (pid {}) accepted stop but {addr} is still open after 10s; check with `agent-cli-x doctor`",
                    resp["pid"]
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(json!({
        "addr": addr,
        "running": false,
        "pid": resp["pid"],
        "interrupted_jobs": resp["interrupted_jobs"],
        "kept_jobs": resp["kept_jobs"],
    }))
}

/// 本机 serve 是否在监听（只探测端口，不握手）。
pub fn local_serve_running(cfg: &Config) -> bool {
    accepts(&local_connect_addr(&cfg.listen))
}

fn accepts(addr: &str) -> bool {
    addr.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_secs(1)).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_addr() {
        assert_eq!(local_connect_addr("0.0.0.0:47821"), "127.0.0.1:47821");
        assert_eq!(local_connect_addr("[::]:9000"), "[::1]:9000");
        assert_eq!(local_connect_addr("10.0.0.5:9000"), "10.0.0.5:9000");
    }
}
