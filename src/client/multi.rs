//! 一条命令操作多个对端：`A,B,C` 或 `@all`。各对端并行执行，结果按对端分组输出。

use super::output::{self, emit, HeadTail};
use super::Client;
use crate::config;
use crate::error::{AppError, Result};
use crate::{http, util};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

pub const ALL: &str = "@all";
/// 每个对端最多在内存中保留的输出（超出部分只写入日志文件）。
const MEM_CAP: usize = 8 * 1024 * 1024;

/// 是否指定了多个对端（逗号分隔或 `@all`）。
pub fn is_multi(spec: &str) -> bool {
    spec == ALL || spec.contains(',')
}

/// 展开对端列表，并预先检查名称都已登记（避免执行到一半才发现拼错）。
pub fn resolve(spec: &str) -> Result<Vec<String>> {
    let peers = config::load_peers(&config::home_dir())?;
    let names: Vec<String> = if spec == ALL {
        peers.peers.keys().cloned().collect()
    } else {
        let mut v: Vec<String> = Vec::new();
        for n in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !v.iter().any(|x| x == n) {
                v.push(n.to_string());
            }
        }
        v
    };
    if names.is_empty() {
        return Err(AppError::config(
            "没有可用的对端。请先执行 `agent-cli-x peer add`",
        ));
    }
    let unknown: Vec<&String> = names
        .iter()
        .filter(|n| !peers.peers.contains_key(*n))
        .collect();
    if !unknown.is_empty() {
        let known: Vec<_> = peers.peers.keys().cloned().collect();
        return Err(AppError::config(format!(
            "未知对端 {}（已登记: {}）",
            unknown
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", "),
            known.join(", ")
        )));
    }
    Ok(names)
}

/// 并行对每个对端执行 f，结果保持输入顺序。
pub fn parallel<T: Send + 'static>(
    peers: &[String],
    f: impl Fn(&str) -> T + Send + Sync + Copy + 'static,
) -> Vec<T> {
    let handles: Vec<_> = peers
        .iter()
        .map(|p| {
            let p = p.clone();
            thread::spawn(move || f(&p))
        })
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().expect("工作线程异常退出"))
        .collect()
}

// ---------------- exec ----------------

struct ExecResult {
    peer: String,
    /// 服务端 exit 帧（code / timed_out / ms）
    exit: Option<Value>,
    chunks: Vec<(bool, String)>,
    kept: usize,
    dropped: bool,
    log: Option<std::path::PathBuf>,
    error: Option<AppError>,
    ms: u64,
}

impl ExecResult {
    fn code(&self) -> i32 {
        if let Some(e) = &self.error {
            return e.exit_code();
        }
        match &self.exit {
            Some(x) if x["timed_out"].as_bool() == Some(true) => 104,
            Some(x) => x["code"].as_i64().unwrap_or(-1) as i32,
            None => AppError::connect("").exit_code(),
        }
    }
}

fn exec_one(peer: &str, body: &Value, read_timeout: Duration) -> ExecResult {
    let started = Instant::now();
    let mut r = ExecResult {
        peer: peer.to_string(),
        exit: None,
        chunks: Vec::new(),
        kept: 0,
        dropped: false,
        log: None,
        error: None,
        ms: 0,
    };
    let dir = config::home_dir().join("logs");
    let _ = fs::create_dir_all(&dir);
    let log_path = dir.join(format!(
        "{}-{peer}-{}.log",
        util::compact_ts(),
        util::random_hex(2)
    ));
    let mut log = fs::File::create(&log_path).ok();
    let res = Client::load(peer).and_then(|c| {
        c.stream("/v1/exec", body, read_timeout, |v| {
            let is_err = match v["t"].as_str() {
                Some("out") => false,
                Some("err") => true,
                Some("exit") => {
                    r.exit = Some(v);
                    return Ok(());
                }
                _ => return Ok(()),
            };
            let d = v["d"].as_str().unwrap_or_default();
            if let Some(f) = log.as_mut() {
                let _ = f.write_all(d.as_bytes());
            }
            if r.kept + d.len() <= MEM_CAP {
                r.kept += d.len();
                r.chunks.push((is_err, d.to_string()));
            } else {
                r.dropped = true;
            }
            Ok(())
        })
    });
    drop(log);
    r.log = Some(log_path);
    r.ms = started.elapsed().as_millis() as u64;
    match res {
        Err(e) => r.error = Some(e),
        Ok(()) if r.exit.is_none() => {
            r.error = Some(AppError::connect(
                "连接中断，未收到命令退出码（命令可能仍在对端运行）",
            ))
        }
        Ok(()) => {}
    }
    r
}

/// 打印一个对端的输出（头尾截断），返回是否保留了完整日志。
fn print_output(r: &ExecResult, full: bool) -> bool {
    let log = r
        .log
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ends_nl = r
        .chunks
        .iter()
        .rev()
        .find(|(_, t)| !t.is_empty())
        .map(|(_, t)| t.ends_with('\n'))
        .unwrap_or(true);
    let mut truncated = false;
    if full {
        for (e, t) in &r.chunks {
            emit(*e, t);
        }
    } else {
        let mut ht = HeadTail::new(output::PER_PEER);
        for (e, t) in &r.chunks {
            ht.feed(*e, t, &mut emit);
        }
        let tail = ht.finish();
        if ht.omitted > 0 {
            truncated = true;
            eprintln!("{}", output::omit_note(ht.omitted, Some(&log)));
        }
        for (e, l) in &tail {
            emit(*e, l);
        }
    }
    if !ends_nl {
        println!();
    }
    if r.dropped {
        eprintln!(
            "[agent-cli-x: 输出过大，内存中只保留前 {} MB，完整输出: {}]",
            MEM_CAP / 1024 / 1024,
            r.log
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
    }
    truncated || r.dropped
}

/// 在多个对端并行执行同一条命令。退出码：全部为 0 时为 0，否则为第一个失败对端的退出码。
pub fn exec_many(
    peers: &[String],
    body: &Value,
    timeout: Option<u64>,
    full: bool,
    json: bool,
) -> i32 {
    let read_timeout = Duration::from_secs(timeout.unwrap_or(3600) + 60);
    let body = body.clone();
    let results: Vec<ExecResult> = {
        let handles: Vec<_> = peers
            .iter()
            .map(|p| {
                let p = p.clone();
                let b = body.clone();
                thread::spawn(move || exec_one(&p, &b, read_timeout))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("工作线程异常退出"))
            .collect()
    };
    let overall = results
        .iter()
        .map(|r| r.code())
        .find(|c| *c != 0)
        .unwrap_or(0);

    if json {
        let list: Vec<Value> = results
            .iter()
            .map(|r| {
                let log = r.log.as_ref().map(|p| p.to_string_lossy().to_string());
                let pick = |want_err: bool| -> Vec<&str> {
                    r.chunks
                        .iter()
                        .filter(|(e, _)| *e == want_err)
                        .map(|(_, t)| t.as_str())
                        .collect()
                };
                let (out, err, omitted) = if full {
                    (pick(false).concat(), pick(true).concat(), 0)
                } else {
                    let (o, no) = output::clip_text(&pick(false), output::PER_PEER, log.as_deref());
                    let (e, ne) = output::clip_text(&pick(true), output::PER_PEER, log.as_deref());
                    (o, e, no + ne)
                };
                let keep_log = r.dropped || omitted > 0;
                if !keep_log {
                    if let Some(p) = &r.log {
                        let _ = fs::remove_file(p);
                    }
                }
                json!({
                    "peer": r.peer,
                    "exit_code": r.code(),
                    "timed_out": r.exit.as_ref().and_then(|x| x["timed_out"].as_bool()).unwrap_or(false),
                    "ms": r.ms,
                    "stdout": out,
                    "stderr": err,
                    "truncated": keep_log,
                    "omitted_lines": omitted,
                    "log": if keep_log { log } else { None },
                    "error": r.error.as_ref().map(http::error_json),
                })
            })
            .collect();
        println!("{}", json!({"exit_code": overall, "results": list}));
        return overall;
    }

    let mut failed = Vec::new();
    for r in &results {
        let status = match &r.error {
            Some(e) => format!("失败 {e}"),
            None if r.code() == 104 => "超时".to_string(),
            None => format!("退出码 {}", r.code()),
        };
        println!(
            "===== {} · {status} · {:.1}s =====",
            r.peer,
            r.ms as f64 / 1000.0
        );
        let keep = print_output(r, full);
        if !keep {
            if let Some(p) = &r.log {
                let _ = fs::remove_file(p);
            }
        }
        if r.code() != 0 {
            failed.push(format!("{}({})", r.peer, r.code()));
        }
    }
    eprintln!(
        "[agent-cli-x: 共 {} 个对端，成功 {}，失败 {}{}]",
        results.len(),
        results.len() - failed.len(),
        failed.len(),
        if failed.is_empty() {
            String::new()
        } else {
            format!("：{}", failed.join(", "))
        }
    );
    overall
}

// ---------------- info ----------------

/// 探测一个对端：返回 info 与往返耗时（毫秒）。
pub fn probe(peer: &str) -> Result<(Value, u64)> {
    let started = Instant::now();
    let v = Client::load(peer)?.call("/v1/info", &json!({}), Duration::from_secs(15))?;
    Ok((v, started.elapsed().as_millis() as u64))
}

/// 多个对端的概览：在线状态、机器、版本、系统、延迟。
pub fn info_many(peers: &[String], json: bool) -> i32 {
    super::disable_version_warning();
    let results = parallel(peers, |p| (p.to_string(), probe(p)));
    // 并行探测结束后一次性刷新缓存，避免并发读写 peers.toml
    let fresh: Vec<(&str, &Value)> = results
        .iter()
        .filter_map(|(p, r)| r.as_ref().ok().map(|(v, _)| (p.as_str(), v)))
        .collect();
    let _ = config::remember_peer_infos(&config::home_dir(), &fresh);
    let overall = results
        .iter()
        .find_map(|(_, r)| r.as_ref().err().map(|e| e.exit_code()))
        .unwrap_or(0);
    if json {
        let list: Vec<Value> = results
            .iter()
            .map(|(p, r)| match r {
                Ok((v, ms)) => json!({"peer": p, "ok": true, "ms": ms, "info": v}),
                Err(e) => json!({"peer": p, "ok": false, "error": http::error_json(e)}),
            })
            .collect();
        println!("{}", Value::Array(list));
        return overall;
    }
    for (p, r) in &results {
        match r {
            Ok((v, ms)) => {
                let ver = v["version"].as_str().unwrap_or("?");
                let warn = if v["config_error"].is_string() {
                    format!("\t配置异常，见 `agent-cli-x info {p}`")
                } else {
                    String::new()
                };
                println!(
                    "{p}\t在线\t{} ({})\t{}/{}\tv{ver}{}\t{ms}ms{warn}",
                    v["node_name"].as_str().unwrap_or("?"),
                    v["node_id"].as_str().unwrap_or("-"),
                    v["os"].as_str().unwrap_or("?"),
                    v["arch"].as_str().unwrap_or("?"),
                    if ver != http::VERSION {
                        format!("（本机 v{}）", http::VERSION)
                    } else {
                        String::new()
                    }
                );
            }
            Err(e) => println!("{p}\t不可用\t{e}"),
        }
    }
    overall
}
