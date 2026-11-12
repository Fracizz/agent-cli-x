//! exec 输出呈现：头尾截断（给 AI 省上下文），完整输出落盘到 `<home>/logs/`。
//!
//! 截断同时按行数和字节数计：头部最多 `head_lines` 行 / `head_bytes` 字节，
//! 尾部最多 `tail_lines` 行 / `tail_bytes` 字节，单行超过 `MAX_LINE_CHARS` 字符也会被截断。
//! `--json` 与普通输出使用同一套上限；只有 `--full` 才输出全部内容。

use crate::config;
use crate::util;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

/// 单行最多保留的字符数（头部、尾部都适用）。
pub const MAX_LINE_CHARS: usize = 2000;
const LINE_CUT_MARK: &str = "…[line truncated]";
/// `--full --json` 时每个流在内存中最多保留的字节数。
const JSON_FULL_CAP: usize = 1024 * 1024;

/// 头尾截断的上限。
#[derive(Clone, Copy)]
pub struct Limits {
    pub head_lines: usize,
    pub head_bytes: usize,
    pub tail_lines: usize,
    pub tail_bytes: usize,
}

/// 单对端 exec / job logs：头尾各 100 行、各 16KB，总量约 32KB。
pub const SINGLE: Limits = Limits {
    head_lines: 100,
    head_bytes: 16 * 1024,
    tail_lines: 100,
    tail_bytes: 16 * 1024,
};

/// 多对端 exec 的每个对端：头尾各 50 行、各 8KB。
pub const PER_PEER: Limits = Limits {
    head_lines: 50,
    head_bytes: 8 * 1024,
    tail_lines: 50,
    tail_bytes: 8 * 1024,
};

/// 头尾截断器。头部内容一到就交给 sink（可以边执行边输出），尾部只保留最后若干行，
/// 由 `finish` 返回。`omitted` 为中间丢弃的行数。
pub struct HeadTail {
    lim: Limits,
    head_lines: usize,
    head_bytes: usize,
    head_done: bool,
    /// 头部当前行已输出的字符数 / 是否已截断
    line_chars: usize,
    line_cut: bool,
    tail: VecDeque<(bool, String)>,
    tail_bytes: usize,
    cur: String,
    cur_err: bool,
    pub omitted: usize,
}

impl HeadTail {
    pub fn new(lim: Limits) -> Self {
        Self {
            lim,
            head_lines: 0,
            head_bytes: 0,
            head_done: false,
            line_chars: 0,
            line_cut: false,
            tail: VecDeque::new(),
            tail_bytes: 0,
            cur: String::new(),
            cur_err: false,
            omitted: 0,
        }
    }

    pub fn feed(&mut self, is_err: bool, text: &str, sink: &mut impl FnMut(bool, &str)) {
        for piece in text.split_inclusive('\n') {
            if !self.head_done {
                self.feed_head(is_err, piece, sink);
                continue;
            }
            if !self.cur.is_empty() && self.cur_err != is_err {
                self.push_tail();
            }
            self.cur_err = is_err;
            // 尾部当前行过长时不再继续累积，避免单行撑爆内存
            if self.cur.len() < MAX_LINE_CHARS * 4 + 8 {
                self.cur.push_str(piece);
            } else if piece.ends_with('\n') {
                self.cur.push('\n');
            }
            if piece.ends_with('\n') {
                self.push_tail();
            }
        }
    }

    fn feed_head(&mut self, is_err: bool, piece: &str, sink: &mut impl FnMut(bool, &str)) {
        let nl = piece.ends_with('\n');
        let body = if nl { &piece[..piece.len() - 1] } else { piece };
        if !self.line_cut && !body.is_empty() {
            let room = MAX_LINE_CHARS.saturating_sub(self.line_chars);
            let n = body.chars().count();
            if n <= room {
                sink(is_err, body);
                self.line_chars += n;
                self.head_bytes += body.len();
            } else {
                let cut = prefix_chars(body, room);
                sink(is_err, cut);
                sink(is_err, LINE_CUT_MARK);
                self.line_cut = true;
                self.head_bytes += cut.len() + LINE_CUT_MARK.len();
            }
        }
        if nl {
            sink(is_err, "\n");
            self.head_bytes += 1;
            self.head_lines += 1;
            self.line_chars = 0;
            self.line_cut = false;
            if self.head_lines >= self.lim.head_lines || self.head_bytes >= self.lim.head_bytes {
                self.head_done = true;
            }
        }
    }

    fn push_tail(&mut self) {
        let mut line = std::mem::take(&mut self.cur);
        if line.chars().count() > MAX_LINE_CHARS {
            let nl = line.ends_with('\n');
            line = format!(
                "{}{LINE_CUT_MARK}{}",
                prefix_chars(&line, MAX_LINE_CHARS),
                if nl { "\n" } else { "" }
            );
        }
        self.tail_bytes += line.len();
        self.tail.push_back((self.cur_err, line));
        while self.tail.len() > self.lim.tail_lines
            || (self.tail_bytes > self.lim.tail_bytes && self.tail.len() > 1)
        {
            if let Some((_, l)) = self.tail.pop_front() {
                self.tail_bytes -= l.len();
                self.omitted += 1;
            }
        }
    }

    /// 结束输入，返回尾部各行（是否 stderr, 内容）。
    pub fn finish(&mut self) -> Vec<(bool, String)> {
        if !self.cur.is_empty() {
            self.push_tail();
        }
        self.tail.drain(..).collect()
    }
}

fn prefix_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// 对一个完整的流做头尾截断，返回截断后的文本与省略的行数（JSON 输出用）。
pub fn clip_text(chunks: &[&str], lim: Limits, log: Option<&str>) -> (String, usize) {
    let mut ht = HeadTail::new(lim);
    let mut out = String::new();
    for c in chunks {
        ht.feed(false, c, &mut |_, s| out.push_str(s));
    }
    let tail = ht.finish();
    if ht.omitted > 0 {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&omit_note(ht.omitted, log));
        out.push('\n');
    }
    for (_, l) in tail {
        out.push_str(&l);
    }
    (out, ht.omitted)
}

pub fn omit_note(omitted: usize, log: Option<&str>) -> String {
    match log {
        Some(p) => format!("[agent-cli-x: {omitted} lines omitted, full output: {p}]"),
        None => format!("[agent-cli-x: {omitted} lines omitted]"),
    }
}

pub struct ExecOutput {
    json: bool,
    full: bool,
    /// 普通输出：stdout/stderr 交错，共用一个截断器
    ht: HeadTail,
    /// JSON 输出：stdout / stderr 分开截断
    ht_out: HeadTail,
    ht_err: HeadTail,
    json_out: String,
    json_err: String,
    /// `--full --json` 超过内存上限时丢弃的行数
    dropped: usize,
    log: Option<File>,
    log_path: PathBuf,
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
            ht: HeadTail::new(SINGLE),
            ht_out: HeadTail::new(SINGLE),
            ht_err: HeadTail::new(SINGLE),
            json_out: String::new(),
            json_err: String::new(),
            dropped: 0,
            log,
            log_path,
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
            if self.full {
                if buf.len() < JSON_FULL_CAP {
                    buf.push_str(text);
                } else {
                    self.dropped += text.matches('\n').count().max(1);
                }
            } else {
                let ht = if is_err {
                    &mut self.ht_err
                } else {
                    &mut self.ht_out
                };
                ht.feed(is_err, text, &mut |_, s| buf.push_str(s));
            }
            return;
        }
        if self.full {
            emit(is_err, text);
            return;
        }
        self.ht.feed(is_err, text, &mut emit);
    }

    /// 打印剩余输出，返回进程应使用的退出码。
    pub fn finish(mut self, exit: &Value) -> i32 {
        drop(self.log.take());
        let code = exit["code"].as_i64().unwrap_or(-1) as i32;
        let timed_out = exit["timed_out"].as_bool().unwrap_or(false);
        let log_str = self.log_path.to_string_lossy().to_string();
        if self.json {
            let mut omitted = self.dropped;
            if !self.full {
                for (ht, buf) in [
                    (&mut self.ht_out, &mut self.json_out),
                    (&mut self.ht_err, &mut self.json_err),
                ] {
                    let tail = ht.finish();
                    if ht.omitted > 0 {
                        if !buf.is_empty() && !buf.ends_with('\n') {
                            buf.push('\n');
                        }
                        buf.push_str(&omit_note(ht.omitted, Some(&log_str)));
                        buf.push('\n');
                        omitted += ht.omitted;
                    }
                    for (_, l) in tail {
                        buf.push_str(&l);
                    }
                }
            }
            let keep_log = omitted > 0;
            if !keep_log {
                let _ = fs::remove_file(&self.log_path);
            }
            let v = json!({
                "exit_code": code,
                "timed_out": timed_out,
                "ms": exit["ms"],
                "stdout": self.json_out,
                "stderr": self.json_err,
                "truncated": keep_log,
                "omitted_lines": omitted,
                "log": keep_log.then_some(log_str),
            });
            println!("{v}");
        } else {
            let tail = self.ht.finish();
            if self.ht.omitted > 0 {
                eprintln!("\n{}", omit_note(self.ht.omitted, Some(&log_str)));
            } else {
                let _ = fs::remove_file(&self.log_path);
            }
            for (is_err, line) in tail {
                emit(is_err, &line);
            }
            if timed_out {
                eprintln!("[agent-cli-x: command timed out, process tree killed]");
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

pub fn emit(is_err: bool, s: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lim: Limits, text: &str) -> (String, usize) {
        clip_text(&[text], lim, None)
    }

    const SMALL: Limits = Limits {
        head_lines: 3,
        head_bytes: 1024,
        tail_lines: 2,
        tail_bytes: 1024,
    };

    #[test]
    fn short_output_untouched() {
        let (s, n) = run(SMALL, "a\nb\nc\n");
        assert_eq!(s, "a\nb\nc\n");
        assert_eq!(n, 0);
    }

    #[test]
    fn keeps_head_and_tail() {
        let text: String = (1..=10).map(|i| format!("{i}\n")).collect();
        let (s, n) = run(SMALL, &text);
        assert_eq!(n, 5);
        assert!(s.starts_with("1\n2\n3\n[agent-cli-x: 5 lines omitted]\n"));
        assert!(s.ends_with("9\n10\n"));
    }

    #[test]
    fn byte_budget_limits_tail() {
        let lim = Limits {
            head_lines: 1,
            head_bytes: 1024,
            tail_lines: 100,
            tail_bytes: 10,
        };
        let text: String = (0..20).map(|_| "xxxx\n").collect();
        let (s, n) = run(lim, &text);
        // 尾部 10 字节只容得下 2 行 "xxxx\n"
        assert_eq!(n, 17);
        assert!(s.ends_with("]\nxxxx\nxxxx\n"));
    }

    #[test]
    fn long_line_cut_in_head_and_tail() {
        let long = "y".repeat(MAX_LINE_CHARS + 50);
        let text = format!("{long}\nm1\nm2\nm3\nm4\n{long}\n");
        let (s, _) = run(SMALL, &text);
        let lines: Vec<&str> = s.lines().collect();
        assert!(lines[0].ends_with(LINE_CUT_MARK));
        assert_eq!(
            lines[0].chars().count(),
            MAX_LINE_CHARS + LINE_CUT_MARK.chars().count()
        );
        assert!(lines.last().unwrap().ends_with(LINE_CUT_MARK));
    }

    #[test]
    fn long_line_split_across_chunks() {
        let a = "z".repeat(MAX_LINE_CHARS - 10);
        let b = "z".repeat(100);
        let (s, _) = clip_text(&[&a, &b, "\nok\n"], SMALL, None);
        let first = s.lines().next().unwrap();
        assert_eq!(
            first.chars().count(),
            MAX_LINE_CHARS + LINE_CUT_MARK.chars().count()
        );
        assert!(s.contains("\nok\n"));
    }
}
