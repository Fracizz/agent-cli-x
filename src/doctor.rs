//! `agent-cli-x doctor`：检查本机配置、服务、开机自启、防火墙与各对端连通性。

use crate::client::{self, multi, Client};
use crate::config::{self, Peer};
use crate::error::Result;
use crate::platform::run_cmd;
use crate::{firewall, http, server, tls};
use serde_json::{json, Value};
use std::path::Path;
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
    const NAME: &str = "autostart";
    if cfg!(windows) {
        let Some((ok, xml)) = run_cmd("schtasks", &["/Query", "/TN", "agent-cli-x", "/XML"]) else {
            r.add(NAME, Level::Skip, "cannot run schtasks");
            return;
        };
        if !ok {
            r.add(
                NAME,
                Level::Warn,
                "Scheduled task agent-cli-x not found. Set it up per README \"Start at boot\", or peers cannot reach this machine after a reboot",
            );
            return;
        }
        let mut problems = Vec::new();
        if xml_tag(&xml, "ExecutionTimeLimit") != Some("PT0S") {
            problems.push("run-time limit not removed (stopped after 72h by default); need -ExecutionTimeLimit 0");
        }
        if xml_tag(&xml, "DisallowStartIfOnBatteries") != Some("false") {
            problems.push("will not start on battery; need -AllowStartIfOnBatteries");
        }
        if xml_tag(&xml, "StopIfGoingOnBatteries") != Some("false") {
            problems.push("stopped when switching to battery; need -DontStopIfGoingOnBatteries");
        }
        if let (Some(cmd), Ok(me)) = (xml_tag(&xml, "Command"), std::env::current_exe()) {
            let norm = |s: &str| s.trim_matches('"').replace('\\', "/").to_lowercase();
            let cmd = norm(cmd);
            let me = norm(&me.to_string_lossy());
            if cmd != me && cmd != "agent-cli-x" && cmd != "agent-cli-x.exe" {
                problems
                    .push("task program path differs from this agent-cli-x; an old version may keep running after upgrade");
            }
        }
        if problems.is_empty() {
            r.add(NAME, Level::Ok, "scheduled task agent-cli-x configured");
        } else {
            r.add(
                NAME,
                Level::Warn,
                format!("scheduled task agent-cli-x: {}", problems.join("; ")),
            );
        }
    } else if cfg!(target_os = "linux") {
        check_systemd(r);
    } else {
        r.add(NAME, Level::Skip, "not checked on this OS");
    }
}

/// Linux：检查 systemd 系统服务或用户服务 agent-cli-x（以 root 作为系统服务运行也很常见）。
fn check_systemd(r: &mut Report) {
    const NAME: &str = "autostart";
    let enabled = |user: bool| {
        let mut args = vec!["is-enabled", "agent-cli-x"];
        if user {
            args.insert(0, "--user");
        }
        run_cmd("systemctl", &args).map(|(_, s)| s.trim().to_string())
    };
    let system = enabled(false);
    let user = if system.as_deref() == Some("enabled") {
        None
    } else {
        enabled(true)
    };
    let user_scope = match (system.as_deref(), user.as_deref()) {
        (None, None) => {
            r.add(
                NAME,
                Level::Skip,
                "systemd not detected; configure autostart with your OS tools",
            );
            return;
        }
        (Some("enabled"), _) => {
            r.add(
                NAME,
                Level::Ok,
                "systemd system service agent-cli-x enabled",
            );
            false
        }
        (_, Some("enabled")) => {
            let me = std::env::var("USER").unwrap_or_default();
            let linger = run_cmd(
                "loginctl",
                &["show-user", &me, "--property=Linger", "--value"],
            )
            .map(|(_, s)| s.trim().to_string())
            .unwrap_or_default();
            if linger == "yes" {
                r.add(NAME, Level::Ok, "systemd user service enabled, linger on");
            } else {
                r.add(
                    NAME,
                    Level::Warn,
                    "systemd user service enabled but linger is off: it will not start at boot and stops after logout. Run `sudo loginctl enable-linger $USER`",
                );
            }
            true
        }
        _ => {
            r.add(
                NAME,
                Level::Warn,
                "systemd service agent-cli-x not enabled (neither system nor user service). Set it up per README \"Start at boot\"",
            );
            return;
        }
    };
    let mut args = vec!["show", "-p", "KillMode", "--value", "agent-cli-x"];
    if user_scope {
        args.insert(0, "--user");
    }
    let kill_mode = run_cmd("systemctl", &args)
        .map(|(_, s)| s.trim().to_string())
        .unwrap_or_default();
    if !kill_mode.is_empty() && kill_mode != "process" && kill_mode != "none" {
        let reload = if user_scope {
            "systemctl --user daemon-reload"
        } else {
            "sudo systemctl daemon-reload"
        };
        r.add(
            "background jobs",
            Level::Warn,
            format!(
                "KillMode={kill_mode}: systemd kills background jobs when serve restarts/upgrades. Add KillMode=process under [Service] in the unit, then run `{reload}`"
            ),
        );
    }
}

fn check_firewall(r: &mut Report, port: &str) {
    const NAME: &str = "firewall";
    let Ok(port) = port.parse::<u16>() else {
        return;
    };
    let fix = || {
        format!(
            "Run once in admin PowerShell: {}",
            firewall::program_rule_cmd()
        )
    };
    use firewall::Status as S;
    match firewall::check(port) {
        S::NotChecked => {}
        S::Program | S::Added => r.add(
            NAME,
            Level::Ok,
            "inbound rule agent-cli-x allows the program",
        ),
        S::Port => r.add(
            NAME,
            Level::Ok,
            format!("inbound rule agent-cli-x allows port {port}"),
        ),
        S::OtherPort => r.add(
            NAME,
            Level::Warn,
            format!(
                "inbound rule agent-cli-x exists but does not allow port {port}. {}",
                fix()
            ),
        ),
        S::Missing => r.add(
            NAME,
            Level::Warn,
            format!(
                "inbound rule agent-cli-x not found; other machines may fail to connect. {}",
                fix()
            ),
        ),
        S::Unknown => r.add(NAME, Level::Skip, "cannot run netsh"),
    }
}

pub fn run(json: bool, check_peers: bool) -> Result<i32> {
    client::disable_version_warning();
    let home = config::home_dir();
    let mut r = Report { items: Vec::new() };

    match config::load_config(&home) {
        Err(e) => {
            let lg = config::last_good_path(&home);
            let extra = if config::config_path(&home).exists() && lg.exists() {
                format!(
                    " (serve falls back to the last good config {} on start; restart serve after fixing)",
                    lg.display()
                )
            } else {
                String::new()
            };
            r.add("config", Level::Fail, format!("{}{extra}", e.message))
        }
        Ok(cfg) => {
            r.add(
                "config",
                Level::Ok,
                format!(
                    "{} ({}, listening on {})",
                    config::config_path(&home).display(),
                    cfg.node_name(),
                    cfg.listen
                ),
            );
            if cfg.workspaces.is_empty() {
                r.add(
                    "workspace",
                    Level::Warn,
                    "no workspaces configured; peers cannot access files or run commands",
                );
            }
            for (n, p) in &cfg.workspaces {
                if Path::new(p).is_dir() {
                    r.add("workspace", Level::Ok, format!("{n} = {p}"));
                } else {
                    r.add(
                        "workspace",
                        Level::Fail,
                        format!("{n} = {p} does not exist or is not a directory"),
                    );
                }
            }
            let fp = match tls::local_fingerprint(&home) {
                Ok(fp) => {
                    r.add("cert", Level::Ok, fp.clone());
                    Some(fp)
                }
                Err(e) => {
                    r.add("cert", Level::Fail, e.message);
                    None
                }
            };
            match config::load_tokens(&home) {
                Ok(t) => r.add(
                    "token",
                    Level::Ok,
                    format!("default token + {} caller token(s)", t.tokens.len()),
                ),
                Err(e) => r.add("token", Level::Fail, e.message),
            }
            if let Some(fp) = fp {
                let addr = local_connect_addr(&cfg.listen);
                let me = Client::from_peer(
                    "local",
                    Peer {
                        addr: addr.clone(),
                        token: cfg.token.clone(),
                        fingerprint: fp,
                        ..Default::default()
                    },
                );
                match me.call("/v1/info", &json!({}), Duration::from_secs(10)) {
                    Ok(v) => {
                        let ver = v["version"].as_str().unwrap_or("?");
                        if ver == http::VERSION {
                            r.add(
                                "local serve",
                                Level::Ok,
                                format!(
                                    "serve running ({addr}, pid {}, started {})",
                                    v["pid"],
                                    v["started"].as_str().unwrap_or("?")
                                ),
                            );
                        } else {
                            r.add(
                                "local serve",
                                Level::Warn,
                                format!(
                                    "running serve is v{ver}, this binary is v{}; restart serve",
                                    http::VERSION
                                ),
                            );
                        }
                    }
                    Err(e) if e.code == "connect" => r.add(
                        "local serve",
                        Level::Fail,
                        format!("no serve on {addr}. Run `agent-cli-x serve` or check autostart"),
                    ),
                    Err(e) => r.add(
                        "local serve",
                        Level::Fail,
                        format!("something listens on {addr} but verification failed (port may be used by another program): {e}"),
                    ),
                }
            }
            check_autostart(&mut r);
            check_firewall(&mut r, &listen_port(&cfg.listen));
            let logs = server::log::dir(&home);
            r.add("logs", Level::Ok, logs.display().to_string());
        }
    }

    if check_peers {
        let peers: Vec<String> = config::load_peers(&home)
            .map(|p| p.peers.keys().cloned().collect())
            .unwrap_or_default();
        if peers.is_empty() {
            r.add("peers", Level::Skip, "no peers registered");
        }
        for (p, res) in multi::parallel(&peers, |p| (p.to_string(), multi::probe(p))) {
            match res {
                Ok((v, ms)) => {
                    let ver = v["version"].as_str().unwrap_or("?");
                    let name = format!("peer {p}");
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
                                "version v{ver} differs from local v{}. Run `agent-cli-x upgrade {p}`",
                                http::VERSION
                            ),
                        );
                    }
                }
                Err(e) => r.add(&format!("peer {p}"), Level::Fail, e.to_string()),
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
        println!("agent-cli-x {} doctor", http::VERSION);
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
