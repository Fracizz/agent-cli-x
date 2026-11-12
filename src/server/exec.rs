//! 命令执行：非交互、流式回传（NDJSON）、超时与断连时杀掉整棵进程树。

use super::Ctx;
use crate::decode::StreamDecoder;
use crate::error::{AppError, Result};
use crate::http;
use crate::platform::{self, KillHandle, Shell};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Deserialize)]
struct ExecReq {
    cmd: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// 主进程退出后，等待后台孙进程释放输出管道的最长时间。
pub(super) const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);
pub(super) const POLL: Duration = Duration::from_millis(200);

fn collapse_lower(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn is_boundary(c: Option<char>) -> bool {
    match c {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '`' | '"' | '\''),
    }
}

/// 黑名单命令：合并空白、忽略大小写后按词边界匹配。
pub fn denied_command(cmd: &str, rules: &[String]) -> Option<String> {
    let text = collapse_lower(cmd);
    for rule in rules {
        let pat = collapse_lower(rule);
        if pat.is_empty() {
            continue;
        }
        let mut from = 0;
        while let Some(pos) = text[from..].find(&pat) {
            let s = from + pos;
            let e = s + pat.len();
            if is_boundary(text[..s].chars().last()) && is_boundary(text[e..].chars().next()) {
                return Some(rule.clone());
            }
            from = s + pat.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
    }
    None
}

pub(super) struct Prepared {
    pub cmd: String,
    pub cwd_display: String,
    pub shell: Shell,
    pub timeout: Duration,
    /// 冷启动命令（未取到预热 shell 时使用）
    pub command: Command,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// 所有命令共用的环境：尽量让工具以非交互、无颜色、无分页的方式运行。
pub(super) fn apply_base_env(c: &mut Command) {
    c.env("CI", "1")
        .env("NO_COLOR", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("AGENT_CLI_X", "1")
        // 不继承 serve 自身环境里的借道标记，是否设置只由本次调用方决定
        .env_remove(crate::config::NO_RELAY_ENV);
    if cfg!(unix) {
        c.env("TERM", "dumb");
    }
}

/// 启动命令：优先使用预热 shell，否则冷启动。返回子进程以及是否用了预热 shell。
pub(super) fn spawn(ctx: &Ctx, p: &mut Prepared) -> std::io::Result<(Child, bool)> {
    if let Some(mut child) = ctx.st.warm.take(p.shell) {
        let payload = platform::warm_payload(&p.cwd, &p.env, &p.cmd);
        let sent = child
            .stdin
            .take()
            .is_some_and(|mut si| si.write_all(payload.as_bytes()).is_ok());
        // stdin 随即关闭：命令里读取 stdin 的程序得到 EOF，与冷启动的 Stdio::null 一致
        if sent {
            return Ok((child, true));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    p.command.spawn().map(|c| (c, false))
}

/// 校验并构造命令。前台 exec 与后台任务共用，超时默认值与上限由调用方给出。
pub(super) fn prepare(
    ctx: &Ctx,
    body: Value,
    default_timeout: u64,
    max_timeout: u64,
) -> Result<Prepared> {
    let req: ExecReq = serde_json::from_value(body)
        .map_err(|e| AppError::bad_request(format!("bad params: {e}")))?;
    if req.cmd.trim().is_empty() {
        return Err(AppError::bad_request("empty command"));
    }
    let policy = &ctx.st.cfg.policy;
    if let Some(rule) = denied_command(&req.cmd, &policy.deny_commands) {
        return Err(AppError::denied(format!(
            "command matches denylist rule `{rule}`"
        )));
    }
    let spec = req.cwd.unwrap_or_default();
    if spec.is_empty() && ctx.st.ws.default_name().is_none() {
        return Err(AppError::bad_request(
            "no --cwd given and server has no default workspace",
        ));
    }
    let cwd = ctx.st.ws.resolve(&spec)?;
    if !cwd.abs.is_dir() {
        return Err(AppError::not_found(format!(
            "working dir not found: {}",
            cwd.display()
        )));
    }
    let shell = Shell::parse(req.shell.as_deref())?;
    let timeout = req
        .timeout_secs
        .unwrap_or(default_timeout)
        .clamp(1, max_timeout.max(1));
    let mut command = platform::build_command(shell, &req.cmd)?;
    command
        .current_dir(&cwd.abs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_base_env(&mut command);
    let mut env = req.env;
    // 禁止借道的调用方：放在用户 --env 之后，不能被覆盖或清空；同时进入预热 shell 的载荷
    env.retain(|k, _| !k.eq_ignore_ascii_case(crate::config::NO_RELAY_ENV));
    if ctx.no_relay {
        env.insert(crate::config::NO_RELAY_ENV.to_string(), ctx.caller.clone());
    }
    for (k, v) in &env {
        command.env(k, v);
    }
    Ok(Prepared {
        cmd: req.cmd,
        cwd_display: cwd.display(),
        shell,
        timeout: Duration::from_secs(timeout),
        command,
        cwd: cwd.abs.clone(),
        env,
    })
}

pub(super) fn spawn_reader<R: Read + Send + 'static>(
    mut r: R,
    kind: u8,
    tx: mpsc::Sender<(u8, Option<Vec<u8>>)>,
) {
    thread::spawn(move || {
        let mut buf = vec![0u8; 8192];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send((kind, Some(buf[..n].to_vec()))).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = tx.send((kind, None));
    });
}

fn send<W: Write>(w: &mut W, v: Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(&v).unwrap_or_default();
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

fn send_text<W: Write>(w: &mut W, kind: u8, text: String) -> std::io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    send(
        w,
        json!({"t": if kind == 0 { "out" } else { "err" }, "d": text}),
    )
}

pub fn run<W: Write>(ctx: &Ctx, body: Value, w: &mut W) {
    let started = Instant::now();
    let policy = &ctx.st.cfg.policy;
    let mut p = match prepare(
        ctx,
        body,
        policy.default_timeout_secs,
        policy.max_timeout_secs,
    ) {
        Ok(p) => p,
        Err(e) => {
            ctx.st.audit.log(
                &ctx.ip,
                &ctx.caller,
                "exec",
                false,
                json!({"error": e.to_string()}),
            );
            let _ = http::write_error(w, &e);
            return;
        }
    };
    let (mut child, warm): (Child, bool) = match spawn(ctx, &mut p) {
        Ok(c) => c,
        Err(e) => {
            let err = AppError::io(format!("failed to start {}: {e}", p.shell.name()));
            ctx.st.audit.log(
                &ctx.ip,
                &ctx.caller,
                "exec",
                false,
                json!({"cmd": p.cmd, "cwd": p.cwd_display, "error": err.message}),
            );
            let _ = http::write_error(w, &err);
            return;
        }
    };
    let killer = KillHandle::attach(&child);
    let (tx, rx) = mpsc::channel();
    if let Some(out) = child.stdout.take() {
        spawn_reader(out, 0, tx.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_reader(err, 1, tx.clone());
    }
    drop(tx);

    let fallback = platform::fallback_encoding();
    let mut decoders = [StreamDecoder::new(fallback), StreamDecoder::new(fallback)];
    let mut open = 2;
    let mut exited = None;
    let mut drain_deadline: Option<Instant> = None;
    let mut timed_out = false;
    let mut client_gone = false;
    let deadline = started + p.timeout;

    let head_ok = http::write_head(w, 200, "application/x-ndjson").and_then(|_| w.flush());
    if head_ok.is_err() {
        client_gone = true;
    }

    while !client_gone {
        // 输出管道关闭不代表子进程已退出；继续轮询超时，且不在已断开的 channel 上忙等。
        let res = if open == 0 {
            thread::sleep(POLL);
            Ok(())
        } else {
            match rx.recv_timeout(POLL) {
                Ok((k, Some(bytes))) => {
                    let text = decoders[k as usize].push(&bytes);
                    send_text(w, k, text)
                }
                Ok((k, None)) => {
                    open -= 1;
                    let text = decoders[k as usize].finish();
                    send_text(w, k, text)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let a = decoders[0].flush_partial();
                    let b = decoders[1].flush_partial();
                    send_text(w, 0, a).and_then(|_| send_text(w, 1, b))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    open = 0;
                    Ok(())
                }
            }
        };
        if res.is_err() {
            client_gone = true;
            break;
        }
        if exited.is_none() {
            if let Ok(Some(status)) = child.try_wait() {
                exited = Some(status);
                drain_deadline = Some(Instant::now() + DRAIN_AFTER_EXIT);
            }
        }
        if exited.is_some() && open == 0 {
            break;
        }
        if drain_deadline.is_some_and(|d| Instant::now() > d) {
            break;
        }
        if exited.is_none() && Instant::now() > deadline {
            timed_out = true;
            killer.kill(&mut child);
            break;
        }
    }
    if client_gone && exited.is_none() {
        // 调用方断开：不留孤儿进程
        killer.kill(&mut child);
    }
    let status = match exited {
        Some(s) => Some(s),
        None => child.wait().ok(),
    };
    let code = status.map(exit_code).unwrap_or(-1);
    let code = if timed_out { 124 } else { code };
    let ms = started.elapsed().as_millis() as u64;
    if !client_gone {
        let tail0 = decoders[0].finish();
        let tail1 = decoders[1].finish();
        let _ = send_text(w, 0, tail0);
        let _ = send_text(w, 1, tail1);
        let _ = send(
            w,
            json!({"t": "exit", "code": code, "ms": ms, "timed_out": timed_out, "shell": p.shell.name(), "warm": warm}),
        );
    }
    ctx.st.audit.log(
        &ctx.ip, &ctx.caller,
        "exec",
        !timed_out && code == 0,
        json!({"cmd": p.cmd, "cwd": p.cwd_display, "shell": p.shell.name(), "code": code, "ms": ms, "timed_out": timed_out, "client_gone": client_gone, "warm": warm}),
    );
}

pub(super) fn exit_code(s: std::process::ExitStatus) -> i32 {
    if let Some(c) = s.code() {
        return c;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            return 128 + sig;
        }
    }
    -1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn preset_blocks_system_deletes() {
        let rules: Vec<String> = crate::config::policy_preset()
            .deny_commands
            .iter()
            .map(|s| s.to_string())
            .collect();
        if cfg!(windows) {
            assert!(denied_command(r"rd /s /q C:\", &rules).is_some());
            assert!(denied_command(r"Remove-Item -Recurse -Force C:\Windows", &rules).is_some());
            assert!(denied_command(r"rd /s /q C:\Users\me\tmp", &rules).is_none());
        } else {
            assert!(denied_command("rm -rf /etc", &rules).is_some());
            assert!(denied_command("rm -rf /etc/x.conf.bak", &rules).is_none());
        }
        assert!(denied_command("cargo build", &rules).is_none());
    }

    #[test]
    fn deny_command_word_boundary() {
        let r = rules(&["rm -rf /", "git push --force", "shutdown"]);
        assert!(denied_command("rm  -rf   /", &r).is_some());
        assert!(denied_command("cd x && RM -RF /", &r).is_some());
        assert!(denied_command("rm -rf /tmp/x", &r).is_none());
        assert!(denied_command("git push --force origin main", &r).is_some());
        assert!(denied_command("git push --force-with-lease", &r).is_none());
        assert!(denied_command("echo hi; shutdown", &r).is_some());
        assert!(denied_command("cargo build", &r).is_none());
    }
}
