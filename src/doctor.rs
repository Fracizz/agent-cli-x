//! `agent-cli-x doctor`：检查本机配置、服务、开机自启、防火墙与各对端连通性。

use crate::client::{self, multi, Client};
use crate::config::{self, Peer};
use crate::error::Result;
use crate::{http, platform, server, tls};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Level {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Ok => "[OK]  ",
            Level::Warn => "[WARN]",
            Level::Fail => "[FAIL]",
            Level::Skip => "[SKIP]",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::Skip => "skip",
        }
    }
}

struct Report {
    items: Vec<(String, Level, String)>,
}

impl Report {
    fn add(&mut self, name: &str, level: Level, detail: impl Into<String>) {
        self.items.push((name.to_string(), level, detail.into()));
    }
}

/// 运行外部命令，返回 (是否成功, 标准输出)。命令不存在时返回 None。
fn run_cmd(prog: &str, args: &[&str]) -> Option<(bool, String)> {
    let mut c = Command::new(prog);
    c.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    platform::apply_spawn_flags(&mut c);
    let o = c.output().ok()?;
    // schtasks 等可能输出 UTF-16 或本地代码页；只关心 ASCII 标签和数字，去掉 NUL 即可
    let text = String::from_utf8_lossy(&o.stdout).replace('\0', "");
    Some((o.status.success(), text))
}

fn listen_port(listen: &str) -> String {
    listen.rsplit(':').next().unwrap_or_default().to_string()
}

/// 把监听地址换成本机可连接的地址（0.0.0.0 → 127.0.0.1）。
fn local_connect_addr(listen: &str) -> String {
    let port = listen_port(listen);
    let host = &listen[..listen.len() - port.len()].trim_end_matches(':');
    match *host {
        "0.0.0.0" | "" => format!("127.0.0.1:{port}"),
        "[::]" => format!("[::1]:{port}"),
        h => format!("{h}:{port}"),
    }
}

fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = xml.find(&open)? + open.len();
    let e = xml[s..].find(&close)? + s;
    Some(xml[s..e].trim())
}

fn check_autostart(r: &mut Report) {
    const NAME: &str = "开机自启";
    if cfg!(windows) {
        let Some((ok, xml)) = run_cmd("schtasks", &["/Query", "/TN", "agent-cli-x", "/XML"]) else {
            r.add(NAME, Level::Skip, "无法运行 schtasks");
            return;
        };
        if !ok {
            r.add(
                NAME,
                Level::Warn,
                "未找到名为 agent-cli-x 的计划任务。按 README「Start at boot」配置，否则重启后对端无法访问本机",
            );
            return;
        }
        let mut problems = Vec::new();
        if xml_tag(&xml, "ExecutionTimeLimit") != Some("PT0S") {
            problems.push("未取消运行时长限制（默认 72 小时后被停止），需 -ExecutionTimeLimit 0");
        }
        if xml_tag(&xml, "DisallowStartIfOnBatteries") != Some("false") {
            problems.push("使用电池时不会启动，需 -AllowStartIfOnBatteries");
        }
        if xml_tag(&xml, "StopIfGoingOnBatteries") != Some("false") {
            problems.push("切换到电池时会被停止，需 -DontStopIfGoingOnBatteries");
        }
        if let (Some(cmd), Ok(me)) = (xml_tag(&xml, "Command"), std::env::current_exe()) {
            let norm = |s: &str| s.trim_matches('"').replace('\\', "/").to_lowercase();
            let cmd = norm(cmd);
            let me = norm(&me.to_string_lossy());
            if cmd != me && cmd != "agent-cli-x" && cmd != "agent-cli-x.exe" {
                problems
                    .push("计划任务启动的程序路径与当前 agent-cli-x 不同，升级后可能仍运行旧版本");
            }
        }
        if problems.is_empty() {
            r.add(NAME, Level::Ok, "计划任务 agent-cli-x 已配置");
        } else {
            r.add(
                NAME,
                Level::Warn,
                format!("计划任务 agent-cli-x：{}", problems.join("；")),
            );
        }
    } else if cfg!(target_os = "linux") {
        let Some((_, state)) = run_cmd("systemctl", &["--user", "is-enabled", "agent-cli-x"])
        else {
            r.add(
                NAME,
                Level::Skip,
                "未检测到 systemd，请用系统自己的方式配置开机自启",
            );
            return;
        };
        if state.trim() != "enabled" {
            r.add(
                NAME,
                Level::Warn,
                format!(
                    "systemd 用户服务 agent-cli-x 未启用（{}）。按 README「Start at boot」配置",
                    if state.trim().is_empty() {
                        "不存在"
                    } else {
                        state.trim()
                    }
                ),
            );
            return;
        }
        let user = std::env::var("USER").unwrap_or_default();
        let linger = run_cmd(
            "loginctl",
            &["show-user", &user, "--property=Linger", "--value"],
        )
        .map(|(_, s)| s.trim().to_string())
        .unwrap_or_default();
        if linger == "yes" {
            r.add(NAME, Level::Ok, "systemd 用户服务已启用，linger 已开启");
        } else {
            r.add(
                NAME,
                Level::Warn,
                "systemd 用户服务已启用，但未开启 linger：开机不会自动启动、退出登录后会停止。执行 `sudo loginctl enable-linger $USER`",
            );
        }
    } else {
        r.add(NAME, Level::Skip, "当前系统不检查");
    }
}

fn check_firewall(r: &mut Report, port: &str) {
    if !cfg!(windows) {
        return;
    }
    const NAME: &str = "防火墙";
    match run_cmd("netsh", &["advfirewall", "firewall", "show", "rule", "name=agent-cli-x"]) {
        Some((true, out)) if out.contains(port) => {
            r.add(NAME, Level::Ok, format!("入站规则 agent-cli-x 已放行端口 {port}"))
        }
        Some((true, _)) => r.add(
            NAME,
            Level::Warn,
            format!("入站规则 agent-cli-x 存在，但没有包含端口 {port}"),
        ),
        _ => r.add(
            NAME,
            Level::Warn,
            format!(
                "未找到入站规则 agent-cli-x，其他机器可能连不上。管理员 PowerShell 执行: \
netsh advfirewall firewall add rule name=\"agent-cli-x\" dir=in action=allow protocol=TCP localport={port}"
            ),
        ),
    }
}

pub fn run(json: bool, check_peers: bool) -> Result<i32> {
    client::disable_version_warning();
    let home = config::home_dir();
    let mut r = Report { items: Vec::new() };

    match config::load_config(&home) {
        Err(e) => r.add("配置", Level::Fail, e.message),
        Ok(cfg) => {
            r.add(
                "配置",
                Level::Ok,
                format!(
                    "{}（{}，监听 {}）",
                    config::config_path(&home).display(),
                    cfg.node_name(),
                    cfg.listen
                ),
            );
            if cfg.workspaces.is_empty() {
                r.add(
                    "工作区",
                    Level::Warn,
                    "未配置任何工作区，对端无法访问文件或执行命令",
                );
            }
            for (n, p) in &cfg.workspaces {
                if Path::new(p).is_dir() {
                    r.add("工作区", Level::Ok, format!("{n} = {p}"));
                } else {
                    r.add("工作区", Level::Fail, format!("{n} = {p} 不存在或不是目录"));
                }
            }
            let fp = match tls::local_fingerprint(&home) {
                Ok(fp) => {
                    r.add("证书", Level::Ok, fp.clone());
                    Some(fp)
                }
                Err(e) => {
                    r.add("证书", Level::Fail, e.message);
                    None
                }
            };
            match config::load_tokens(&home) {
                Ok(t) => r.add(
                    "token",
                    Level::Ok,
                    format!("默认 token + {} 个调用方 token", t.tokens.len()),
                ),
                Err(e) => r.add("token", Level::Fail, e.message),
            }
            if let Some(fp) = fp {
                let addr = local_connect_addr(&cfg.listen);
                let me = Client::from_peer(
                    "本机",
                    Peer {
                        addr: addr.clone(),
                        token: cfg.token.clone(),
                        fingerprint: fp,
                        node_name: None,
                        node_id: None,
                    },
                );
                match me.call("/v1/info", &json!({}), Duration::from_secs(10)) {
                    Ok(v) => {
                        let ver = v["version"].as_str().unwrap_or("?");
                        if ver == http::VERSION {
                            r.add(
                                "本机服务",
                                Level::Ok,
                                format!(
                                    "serve 运行中（{addr}，pid {}，启动于 {}）",
                                    v["pid"],
                                    v["started"].as_str().unwrap_or("?")
                                ),
                            );
                        } else {
                            r.add(
                                "本机服务",
                                Level::Warn,
                                format!(
                                    "运行中的 serve 是 v{ver}，与本程序 v{} 不同，需要重启 serve",
                                    http::VERSION
                                ),
                            );
                        }
                    }
                    Err(e) if e.code == "connect" => r.add(
                        "本机服务",
                        Level::Fail,
                        format!("{addr} 上没有运行 serve。执行 `agent-cli-x serve` 或检查开机自启"),
                    ),
                    Err(e) => r.add(
                        "本机服务",
                        Level::Fail,
                        format!("{addr} 有服务在监听，但校验失败（可能是其他程序占用了端口）: {e}"),
                    ),
                }
            }
            check_autostart(&mut r);
            check_firewall(&mut r, &listen_port(&cfg.listen));
            let logs = server::log::dir(&home);
            r.add("日志", Level::Ok, logs.display().to_string());
        }
    }

    if check_peers {
        let peers: Vec<String> = config::load_peers(&home)
            .map(|p| p.peers.keys().cloned().collect())
            .unwrap_or_default();
        if peers.is_empty() {
            r.add("对端", Level::Skip, "尚未登记对端");
        }
        for (p, res) in multi::parallel(&peers, |p| (p.to_string(), multi::probe(p))) {
            match res {
                Ok((v, ms)) => {
                    let ver = v["version"].as_str().unwrap_or("?");
                    let name = format!("对端 {p}");
                    if ver == http::VERSION {
                        r.add(
                            &name,
                            Level::Ok,
                            format!("{} v{ver}，{ms}ms", v["node_name"].as_str().unwrap_or("?")),
                        );
                    } else {
                        r.add(
                            &name,
                            Level::Warn,
                            format!(
                                "版本 v{ver} 与本机 v{} 不同。执行 `agent-cli-x upgrade {p}`",
                                http::VERSION
                            ),
                        );
                    }
                }
                Err(e) => r.add(&format!("对端 {p}"), Level::Fail, e.to_string()),
            }
        }
    }

    let failed = r.items.iter().any(|(_, l, _)| *l == Level::Fail);
    if json {
        let list: Vec<Value> = r
            .items
            .iter()
            .map(|(n, l, d)| json!({"check": n, "status": l.name(), "detail": d}))
            .collect();
        println!(
            "{}",
            json!({"ok": !failed, "version": http::VERSION, "checks": list})
        );
    } else {
        println!("agent-cli-x {} 自检", http::VERSION);
        for (n, l, d) in &r.items {
            println!("{} {n}: {d}", l.tag());
        }
    }
    Ok(if failed { 1 } else { 0 })
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

    #[test]
    fn xml() {
        let x = "<a><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></a>";
        assert_eq!(xml_tag(x, "ExecutionTimeLimit"), Some("PT0S"));
        assert_eq!(xml_tag(x, "Command"), None);
    }
}
