//! 后台任务（exec --detach）：输出写入 `<home>/jobs/<id>/output.log`，状态写入 `meta.json`。
//! serve 重启后仍在运行的任务无法再跟踪，标记为 `lost`。

use super::exec::{self, DRAIN_AFTER_EXIT, POLL};
use super::Ctx;
use crate::decode::StreamDecoder;
use crate::error::{AppError, Result};
use crate::platform::{self, KillHandle};
use crate::{http, util};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const RETAIN_DAYS: u64 = 7;
const HEARTBEAT: Duration = Duration::from_secs(15);
/// job kill 等待任务真正结束的最长时间
const KILL_WAIT: Duration = Duration::from_secs(5);

pub struct Jobs {
    dir: PathBuf,
    running: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

fn read_meta(p: &Path) -> Option<Value> {
    fs::read(p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_meta(p: &Path, v: &Value) {
    let _ = util::atomic_write(p, &serde_json::to_vec_pretty(v).unwrap_or_default());
}

impl Jobs {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        util::prune_older_than(&dir, RETAIN_DAYS);
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path().join("meta.json");
                if let Some(mut m) = read_meta(&p) {
                    if m["status"] == "running" {
                        m["status"] = json!("lost");
                        m["ended"] = json!(util::now_iso());
                        write_meta(&p, &m);
                    }
                }
            }
        }
        Self {
            dir,
            running: Mutex::new(HashMap::new()),
        }
    }

    fn job_dir(&self, id: &str) -> Result<PathBuf> {
        let ok = !id.is_empty()
            && id.len() <= 64
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        let d = self.dir.join(id);
        if !ok || !d.join("meta.json").is_file() {
            return Err(AppError::not_found(format!("找不到任务 `{id}`")));
        }
        Ok(d)
    }

    fn running_flag(&self, id: &str) -> Option<Arc<AtomicBool>> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// 正在运行的后台任务数。
    pub fn running_count(&self) -> usize {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[derive(Deserialize)]
struct IdReq {
    id: String,
}

pub fn start(ctx: &Ctx, body: Value) -> Result<Value> {
    let policy = &ctx.st.cfg.policy;
    let jobs = &ctx.st.jobs;
    let n = jobs.running.lock().unwrap_or_else(|e| e.into_inner()).len();
    if n >= policy.max_jobs {
        return Err(AppError::new(
            "busy",
            format!(
                "后台任务已达上限 {}，请等待结束或 job kill",
                policy.max_jobs
            ),
        ));
    }
    let mut p = exec::prepare(ctx, body, policy.job_timeout_secs, policy.job_timeout_secs)?;
    let id = format!("{}-{}", util::compact_ts(), util::random_hex(3));
    let dir = jobs.dir.join(&id);
    fs::create_dir_all(&dir)?;
    let log = File::create(dir.join("output.log"))?;
    let (mut child, _) = exec::spawn(ctx, &mut p).map_err(|e| {
        let _ = fs::remove_dir_all(&dir);
        AppError::io(format!("启动 {} 失败: {e}", p.shell.name()))
    })?;
    let meta_path = dir.join("meta.json");
    write_meta(
        &meta_path,
        &json!({
            "id": id,
            "cmd": p.cmd,
            "cwd": p.cwd_display,
            "shell": p.shell.name(),
            "status": "running",
            "pid": child.id(),
            "started": util::now_iso(),
            "timeout_secs": p.timeout.as_secs(),
        }),
    );
    let flag = Arc::new(AtomicBool::new(false));
    jobs.running
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone(), Arc::clone(&flag));
    ctx.st.audit.log(
        &ctx.ip,
        &ctx.caller,
        "job_start",
        true,
        json!({"id": id, "cmd": p.cmd, "cwd": p.cwd_display}),
    );

    let st = Arc::clone(&ctx.st);
    let ip = ctx.ip.clone();
    let caller = ctx.caller.clone();
    let job_id = id.clone();
    let timeout = p.timeout;
    thread::spawn(move || {
        let killer = KillHandle::attach(&child);
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
        let mut log = log;
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
                if flag.load(Ordering::SeqCst) {
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
        st.jobs
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&job_id);
        st.audit.log(
            &ip,
            &caller,
            "job_end",
            outcome == "exited" && code == 0,
            json!({"id": job_id, "status": outcome, "code": code}),
        );
    });
    Ok(
        json!({"job_id": id, "cwd": p.cwd_display, "shell": p.shell.name(), "timeout_secs": timeout.as_secs()}),
    )
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
                .filter_map(|e| read_meta(&e.path().join("meta.json")))
                .collect()
        })
        .unwrap_or_default();
    // id 以时间戳开头，倒序即最新在前
    metas.sort_by(|a, b| b["id"].as_str().cmp(&a["id"].as_str()));
    metas.truncate(req.limit.clamp(1, 500));
    Ok(json!({"jobs": metas}))
}

pub fn kill(ctx: &Ctx, body: Value) -> Result<Value> {
    let req: IdReq =
        serde_json::from_value(body).map_err(|e| AppError::bad_request(e.to_string()))?;
    let dir = ctx.st.jobs.job_dir(&req.id)?;
    match ctx.st.jobs.running_flag(&req.id) {
        Some(flag) => {
            flag.store(true, Ordering::SeqCst);
            ctx.st.audit.log(
                &ctx.ip,
                &ctx.caller,
                "job_kill",
                true,
                json!({"id": req.id}),
            );
            // 等任务线程真正终止进程并写回状态，避免紧接着的 job logs / list 仍显示 running
            let deadline = Instant::now() + KILL_WAIT;
            while ctx.st.jobs.running_flag(&req.id).is_some() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
            let status = read_meta(&dir.join("meta.json"))
                .and_then(|m| m["status"].as_str().map(|s| s.to_string()))
                .filter(|s| s != "running")
                .unwrap_or_else(|| "killing".into());
            Ok(json!({"id": req.id, "status": status}))
        }
        None => {
            let status = read_meta(&dir.join("meta.json"))
                .and_then(|m| m["status"].as_str().map(|s| s.to_string()))
                .unwrap_or_default();
            Err(AppError::conflict(format!(
                "任务 {} 已不在运行（状态: {status}）",
                req.id
            )))
        }
    }
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
    let mut file = match File::open(dir.join("output.log")) {
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
        let meta = read_meta(&dir.join("meta.json")).unwrap_or_else(|| json!({}));
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
