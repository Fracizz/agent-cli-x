//! exec 输出呈现：头尾截断（给 AI 省上下文），完整输出落盘到 `<home>/logs/`。

use crate::config;
use crate::util;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

const HEAD_LINES: usize = 200;
const TAIL_LINES: usize = 200;
const HEAD_BYTES: usize = 64 * 1024;
const MAX_LINE_CHARS: usize = 4000;
const JSON_CAP: usize = 1024 * 1024;

pub struct ExecOutput {
    json: bool,
    full: bool,
    head_lines: usize,
    head_bytes: usize,
    head_done: bool,
    tail: VecDeque<(bool, String)>,
    cur: String,
    cur_err: bool,
    omitted: usize,
    log: Option<File>,
    log_path: PathBuf,
    json_out: String,
    json_err: String,
}

impl ExecOutput {
    pub fn new(json: bool, full: bool, peer: &str) -> Self {
        let dir = config::home_dir().join("logs");
        let _ = fs::create_dir_all(&dir);
        util::prune_older_than(&dir, 7);
        let log_path = dir.join(format!(
            "{}-{peer}-{}.log",
            util::compact_ts(),
            util::random_hex(2)
        ));
        let log = File::create(&log_path).ok();
        Self {
            json,
            full,
            head_lines: 0,
            head_bytes: 0,
            head_done: false,
            tail: VecDeque::new(),
            cur: String::new(),
            cur_err: false,
            omitted: 0,
            log,
            log_path,
            json_out: String::new(),
            json_err: String::new(),
        }
    }

    pub fn feed(&mut self, is_err: bool, text: &str) {
        if let Some(f) = self.log.as_mut() {
            let _ = f.write_all(text.as_bytes());
        }
        if self.json {
            let buf = if is_err {
                &mut self.json_err
            } else {
                &mut self.json_out
            };
            if buf.len() < JSON_CAP {
                buf.push_str(text);
            } else {
                self.omitted += text.matches('\n').count().max(1);
            }
            return;
        }
        if self.full {
            emit(is_err, text);
            return;
        }
        for piece in text.split_inclusive('\n') {
            if !self.head_done {
                emit(is_err, piece);
                self.head_bytes += piece.len();
                if piece.ends_with('\n') {
                    self.head_lines += 1;
                }
                if self.head_lines >= HEAD_LINES || self.head_bytes >= HEAD_BYTES {
                    self.head_done = true;
                }
                continue;
            }
            if !self.cur.is_empty() && self.cur_err != is_err {
                self.push_tail();
            }
            self.cur_err = is_err;
            self.cur.push_str(piece);
            if piece.ends_with('\n') {
                self.push_tail();
            }
        }
    }

    fn push_tail(&mut self) {
        let mut line = std::mem::take(&mut self.cur);
        if line.chars().count() > MAX_LINE_CHARS {
            let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
            line = format!("{cut}…[行过长已截断]\n");
        }
        self.tail.push_back((self.cur_err, line));
        if self.tail.len() > TAIL_LINES {
            self.tail.pop_front();
            self.omitted += 1;
        }
    }

    /// 打印剩余输出，返回进程应使用的退出码。
    pub fn finish(mut self, exit: &Value) -> i32 {
        if !self.cur.is_empty() {
            self.push_tail();
        }
        drop(self.log.take());
        let code = exit["code"].as_i64().unwrap_or(-1) as i32;
        let timed_out = exit["timed_out"].as_bool().unwrap_or(false);
        let keep_log = self.omitted > 0;
        if !keep_log {
            let _ = fs::remove_file(&self.log_path);
        }
        if self.json {
            let v = json!({
                "exit_code": code,
                "timed_out": timed_out,
                "ms": exit["ms"],
                "stdout": self.json_out,
                "stderr": self.json_err,
                "truncated": keep_log,
                "log": keep_log.then(|| self.log_path.to_string_lossy().to_string()),
            });
            println!("{v}");
        } else {
            if self.omitted > 0 {
                eprintln!(
                    "\n[agent-cli-x: 中间省略 {} 行，完整输出: {}]",
                    self.omitted,
                    self.log_path.display()
                );
            }
            for (is_err, line) in self.tail.drain(..) {
                emit(is_err, &line);
            }
            if timed_out {
                eprintln!("[agent-cli-x: 命令超时，已终止进程树]");
            }
        }
        let _ = std::io::stdout().flush();
        if timed_out {
            104
        } else {
            code
        }
    }
}

fn emit(is_err: bool, s: &str) {
    if is_err {
        let mut e = std::io::stderr().lock();
        let _ = e.write_all(s.as_bytes());
        let _ = e.flush();
    } else {
        let mut o = std::io::stdout().lock();
        let _ = o.write_all(s.as_bytes());
        let _ = o.flush();
    }
}
