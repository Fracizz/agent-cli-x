//! 后台任务（exec --detach）。每个任务由一个脱离 serve 的执行进程（`agent-cli-x job-run <目录>`）
//! 托管：它启动命令、转码输出写入 `output.log`、负责超时与终止，并在 `meta.json` 写回结果。
//! serve 只通过任务目录里的文件与它交互，所以 serve 重启或升级时任务继续运行。
//!
//! 任务目录 `<home>/jobs/<id>/`：
//! - `spec.json`：要执行的命令（serve 写，执行进程读）
//! - `meta.json`：状态（serve 写初始值，之后只由执行进程更新）
//! - `output.log`：输出（UTF-8）
//! - `runner.lock`：执行进程运行期间一直持有的排他锁；进程退出（包括崩溃）时系统自动释放，
//!   serve 据此判断任务是否已丢失（lost）
//! - `kill`：serve 请求终止时创建，执行进程看到后终止整棵进程树

use super::exec::{self, DRAIN_AFTER_EXIT, POLL};
use super::Ctx;
use crate::decode::StreamDecoder;
use crate::error::{AppError, Result};
use crate::platform::{self, KillHandle, Shell};
use crate::{http, util};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub const RETAIN_DAYS: u64 = 7;
const HEARTBEAT: Duration = Duration::from_secs(15);
/// job kill 等待任务真正结束的最长时间
const KILL_WAIT: Duration = Duration::from_secs(5);
/// 等待执行进程启动命令的最长时间
const START_WAIT: Duration = Duration::from_secs(15);

const SPEC: &str = "spec.json";
const META: &str = "meta.json";
const LOG: &str = "output.log";
const LOCK: &str = "runner.lock";
const KILL: &str = "kill";

pub struct Jobs {
    dir: PathBuf,
}

fn read_meta(p: &Path) -> Option<Value> {
    fs::read(p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_meta(p: &Path, v: &Value) {
    let _ = util::atomic_write(p, &serde_json::to_vec_pretty(v).unwrap_or_default());
}

/// 执行进程是否仍在运行：它持有 runner.lock 的排他锁，能拿到锁说明它已经退出。
fn runner_alive(dir: &Path) -> bool {
    let Ok(f) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(LOCK))
    else {
        // 旧版本创建的任务没有锁文件，serve 重启后已无法跟踪
        return false;
    };
    matches!(platform::lock_exclusive(&f, false), Ok(false))
}

/// 读取任务状态：记录为 running 但执行进程已不在时视为 lost（不写回，执行进程仍可能补写结果）。
fn load_status(dir: &Path) -> Option<Value> {
    let mut m = read_meta(&dir.join(META))?;
    if m["status"] == "running" && !runner_alive(dir) {
        m["status"] = json!("lost");
    }
    Some(m)
}

impl Jobs {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        util::prune_older_than(&dir, RETAIN_DAYS);
        Self { dir }
    }

    fn job_dir(&self, id: &str) -> Result<PathBuf> {
        let ok = !id.is_empty()
            && id.len() <= 64
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        let d = self.dir.join(id);
        if !ok || !d.join(META).is_file() {
            return Err(AppError::not_found(format!("找不到任务 `{id}`")));
        }
        Ok(d)
    }

    /// 正在运行的后台任务数（包括 serve 重启前启动、仍在运行的）。
    pub fn running_count(&self) -> usize {
        fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| {
                        let d = e.path();
                        read_meta(&d.join(META)).is_some_and(|m| m["status"] == "running")
                            && runner_alive(&d)
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}

#[derive(Deserialize)]
struct IdReq {
    id: String,
}

pub fn start(ctx: &Ctx, body: Value) -> Result<Value> {
    let policy = &ctx.st.cfg.policy;
    let jobs = &ctx.st.jobs;
    if jobs.running_count() >= policy.max_jobs {
        return Err(AppError::new(
            "busy",
            format!(
                "后台任务已达上限 {}，请等待结束或 job kill",
                policy.max_jobs
            ),
        ));
    }
    let p = exec::prepare(ctx, body, policy.job_timeout_secs, policy.job_timeout_secs)?;
    let id = format!("{}-{}", util::compact_ts(), util::random_hex(3));
    let dir = jobs.dir.join(&id);
    let res = (|| {
        fs::create_dir_all(&dir)?;
        File::create(dir.join(LOG))?;
        File::create(dir.join(LOCK))?;
        let spec = json!({
            "id": id,
            "cmd": p.cmd,
            "shell": p.shell.name(),
            "cwd": p.cwd.to_string_lossy(),
            "env": p.env,
            "timeout_secs": p.timeout.as_secs(),
            "home": ctx.st.home.to_string_lossy(),
            "ip": ctx.ip,
            "caller": ctx.caller,
        });
        util::atomic_write(
            &dir.join(SPEC),
            &serde_json::to_vec_pretty(&spec).unwrap_or_default(),
        )?;
        write_meta(
            &dir.join(META),
            &json!({
                "id": id,
                "cmd": p.cmd,
                "cwd": p.cwd_display,
                "shell": p.shell.name(),
                "status": "running",
                "started": util::now_iso(),
                "timeout_secs": p.timeout.as_secs(),
            }),
        );
        spawn_runner(&dir)?;
        wait_started(&dir)
    })();
    if let Err(e) = res {
        let _ = fs::write(dir.join(KILL), b"");
        let _ = fs::remove_dir_all(&dir);
        ctx.st.audit.log(
            &ctx.ip,
            &ctx.caller,
            "job_start",
            false,
            json!({"cmd": p.cmd, "cwd": p.cwd_display, "error": e.message}),
        );
        return Err(e);
    }
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "job_start",
        true,
        json!({"id": id, "cmd": p.cmd, "cwd": p.cwd_display}),
    );
    Ok(json!({
        "job_id": id,
        "cwd": p.cwd_display,
        "shell": p.shell.name(),
        "timeout_secs": p.timeout.as_secs(),
    }))
}

/// 启动脱离 serve 的执行进程。
fn spawn_runner(dir: &Path) -> Result<()> {
    let exe = super::upgrade::current_exe()?;
    platform::spawn_detached(|| {
        let mut c = Command::new(&exe);
        c.arg("job-run")
            .arg(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // 借道标记只按调用方写进 spec，执行进程自身不带
            .env_remove(crate::config::NO_RELAY_ENV);
        c
    })
    .map(|_| ())
    .map_err(|e| AppError::io(format!("启动后台任务执行进程失败: {e}")))
}

/// 等执行进程报告命令已启动（meta 中出现 pid），把启动失败同步返回给调用方。
fn wait_started(dir: &Path) -> Result<()> {
    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Some(m) = read_meta(&dir.join(META)) {
            if m["status"] == "failed" {
                return Err(AppError::io(
                    m["error"].as_str().unwrap_or("启动失败").to_string(),
                ));
            }
            if !m["pid"].is_null() {
                return Ok(());
            }
        }
        if Instant::now() > deadline {
            return Err(AppError::io(format!(
                "后台任务执行进程 {} 秒内没有启动命令",
                START_WAIT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(30));
    }
}

#[derive(Deserialize)]
struct Spec {
    cmd: String,
    shell: String,
    cwd: PathBuf,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
    timeout_secs: u64,
    home: PathBuf,
    #[serde(default)]
    id: String,
    #[serde(default)]
    ip: String,
    #[serde(default)]
    caller: String,
}

/// 执行进程入口（`agent-cli-x job-run <目录>`）：运行到任务结束，结果写入 meta.json。
pub fn run(dir: &Path) -> Result<i32> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK))?;
    // serve 检查存活时会短暂拿锁，这里等它释放
    platform::lock_exclusive(&lock, true)?;
    let meta_path = dir.join(META);
    let fail = |msg: String| {
        let mut m = read_meta(&meta_path).unwrap_or_else(|| json!({}));
        m["status"] = json!("failed");
        m["error"] = json!(msg);
        m["ended"] = json!(util::now_iso());
        write_meta(&meta_path, &m);
        AppError::io(msg)
    };
    let spec: Spec = fs::read(dir.join(SPEC))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| fail("spec.json 无效".into()))?;
    let shell = Shell::parse(Some(&spec.shell)).map_err(|e| fail(e.message))?;
    let mut command = platform::build_command(shell, &spec.cmd).map_err(|e| fail(e.message))?;
    command
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    exec::apply_base_env(&mut command);
    for (k, v) in &spec.env {
        command.env(k, v);
    }
    let mut child = command
        .spawn()
        .map_err(|e| fail(format!("启动 {} 失败: {e}", shell.name())))?;
    let killer = KillHandle::attach(&child);
    let mut m = read_meta(&meta_path).unwrap_or_else(|| json!({}));
    m["pid"] = json!(child.id());
    m["runner_pid"] = json!(std::process::id());
    write_meta(&meta_path, &m);

    let mut log = OpenOptions::new().append(true).open(dir.join(LOG))?;
    let (tx, rx) = mpsc::channel();
    if let Some(o) = child.stdout.take() {
        exec::spawn_reader(o, 0, tx.clone());
    }
    if let Some(e) = child.stderr.take() {
        exec::spawn_reader(e, 1, tx.clone());
    }
    drop(tx);
    let fallback = platform::fallback_encoding();
    let mut dec = [StreamDecoder::new(fallback), StreamDecoder::new(fallback)];
    let timeout = Duration::from_secs(spec.timeout_secs.max(1));
    let kill_flag = dir.join(KILL);
    let started = Instant::now();
    let mut open = 2;
    let mut exited = None;
    let mut drain: Option<Instant> = None;
    let mut outcome = "exited";
    loop {
        let text = match rx.recv_timeout(POLL) {
            Ok((k, Some(b))) => dec[k as usize].push(&b),
            Ok((k, None)) => {
                open -= 1;
                dec[k as usize].finish()
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut t = dec[0].flush_partial();
                t.push_str(&dec[1].flush_partial());
                t
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                open = 0;
                String::new()
            }
        };
        if !text.is_empty() {
            let _ = log.write_all(text.as_bytes());
            let _ = log.flush();
        }
        if open == 0 {
            break;
        }
        if exited.is_none() {
            if let Ok(Some(s)) = child.try_wait() {
                exited = Some(s);
                drain = Some(Instant::now() + DRAIN_AFTER_EXIT);
            }
        }
        if drain.is_some_and(|d| Instant::now() > d) {
            break;
        }
        if exited.is_none() {
            if kill_flag.exists() {
                outcome = "killed";
                killer.kill(&mut child);
                break;
            }
            if started.elapsed() > timeout {
                outcome = "timed_out";
                killer.kill(&mut child);
                break;
            }
        }
    }
    let rest = format!("{}{}", dec[0].finish(), dec[1].finish());
    let _ = log.write_all(rest.as_bytes());
    drop(log);
    let status = match exited {
        Some(s) => Some(s),
        None => child.wait().ok(),
    };
    let code = match outcome {
        "timed_out" => 124,
        "killed" => 137,
        _ => status.map(exec::exit_code).unwrap_or(-1),
    };
    let mut m = read_meta(&meta_path).unwrap_or_else(|| json!({}));
    m["status"] = json!(outcome);
    m["code"] = json!(code);
    m["ended"] = json!(util::now_iso());
    m["ms"] = json!(started.elapsed().as_millis() as u64);
    write_meta(&meta_path, &m);
    super::audit::Audit::new(spec.home.join("audit")).log(
        &spec.ip,
        &spec.caller,
        "job_end",
        outcome == "exited" && code == 0,
        json!({"id": spec.id, "status": outcome, "code": code}),
    );
    // 结果写完再释放锁：serve 看到锁释放时 meta 已是最终状态
    drop(lock);
    Ok(0)
}

#[derive(Deserialize)]
struct ListReq {
    #[serde(default = "list_limit")]
    limit: usize,
}
fn list_limit() -> usize {
    20
}

pub fn list(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: ListReq =
        serde_json::from_value(body).map_err(|e| AppError::bad_request(e.to_string()))?;
    let mut metas: Vec<Value> = fs::read_dir(&ctx.st.jobs.dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| load_status(&e.path()))
                .collect()
        })
        .unwrap_or_default();
    // id 以时间戳开头，倒序即最新在前
    metas.sort_by(|a, b| b["id"].as_str().cmp(&a["id"].as_str()));
    metas.truncate(req.limit.clamp(1, 500));
    for m in &mut metas {
        // 内部字段，不占调用方的 token
        if let Some(o) = m.as_object_mut() {
            o.remove("runner_pid");
        }
    }
    Ok(json!({"jobs": metas}))
}

pub fn kill(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: IdReq =
        serde_json::from_value(body).map_err(|e| AppError::bad_request(e.to_string()))?;
    let dir = ctx.st.jobs.job_dir(&req.id)?;
    let status = |d: &Path| {
        load_status(d)
            .and_then(|m| m["status"].as_str().map(|s| s.to_string()))
            .unwrap_or_default()
    };
    let now = status(&dir);
    if now != "running" {
        return Err(AppError::conflict(format!(
            "任务 {} 已不在运行（状态: {now}）",
            req.id
        )));
    }
    fs::write(dir.join(KILL), b"")?;
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "job_kill",
        true,
        json!({"id": req.id}),
    );
    // 等执行进程真正终止进程并写回状态，避免紧接着的 job logs / list 仍显示 running
    let deadline = Instant::now() + KILL_WAIT;
    let mut st = status(&dir);
    while st == "running" && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
        st = status(&dir);
    }
    let st = if st == "running" {
        "killing".into()
    } else {
        st
    };
    Ok(json!({"id": req.id, "status": st}))
}

#[derive(Deserialize)]
struct LogReq {
    id: String,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    follow: bool,
}

fn send<W: Write>(w: &mut W, v: Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(&v).unwrap_or_default();
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// 以 NDJSON 流返回日志；follow 时持续推送直到任务结束。
pub fn log<W: Write>(ctx: &Ctx, body: Value, w: &mut W) {
    let prep = (|| -> Result<(LogReq, PathBuf)> {
        let req: LogReq =
            serde_json::from_value(body).map_err(|e| AppError::bad_request(e.to_string()))?;
        let dir = ctx.st.jobs.job_dir(&req.id)?;
        Ok((req, dir))
    })();
    let (req, dir) = match prep {
        Ok(x) => x,
        Err(e) => {
            let _ = http::write_error(w, &e);
            return;
        }
    };
    let mut file = match File::open(dir.join(LOG)) {
        Ok(f) => f,
        Err(e) => {
            let _ = http::write_error(w, &AppError::from(e));
            return;
        }
    };
    if http::write_head(w, 200, "application/x-ndjson")
        .and_then(|_| w.flush())
        .is_err()
    {
        return;
    }
    let mut offset = req.offset;
    let _ = file.seek(SeekFrom::Start(offset));
    let mut pending: Vec<u8> = Vec::new();
    let mut last_pass = false;
    let mut last_sent = Instant::now();
    loop {
        let mut buf = Vec::new();
        let _ = file.read_to_end(&mut buf);
        if !buf.is_empty() {
            pending.extend_from_slice(&buf);
            let valid = match std::str::from_utf8(&pending) {
                Ok(_) => pending.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => pending.len(),
            };
            if valid > 0 {
                let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
                pending.drain(..valid);
                offset += valid as u64;
                if send(w, json!({"t": "out", "d": text})).is_err() {
                    return;
                }
                last_sent = Instant::now();
            }
        }
        let meta = load_status(&dir).unwrap_or_else(|| json!({}));
        let running = meta["status"] == "running";
        if !running && !last_pass {
            // 再读一次，确保拿到任务结束前最后写入的输出
            last_pass = true;
            continue;
        }
        if !running || !req.follow {
            let _ = send(
                w,
                json!({
                    "t": "end",
                    "status": meta["status"],
                    "code": meta["code"],
                    "timed_out": meta["status"] == "timed_out",
                    "offset": offset,
                }),
            );
            return;
        }
        if last_sent.elapsed() > HEARTBEAT {
            if send(w, json!({"t": "ping"})).is_err() {
                return;
            }
            last_sent = Instant::now();
        }
        thread::sleep(Duration::from_millis(300));
    }
}
