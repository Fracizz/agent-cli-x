//! Bounded command output. Complete decoded output is kept locally whenever anything is omitted.

use crate::config;
use crate::util;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

pub const MAX_LINE_CHARS: usize = 2000;
const LINE_CUT_MARK: &str = "…[line truncated]";
/// Explicit --full JSON still has a bounded in-memory representation; the full log remains available.
const JSON_FULL_CAP: usize = 1024 * 1024;

#[derive(Clone, Copy)]
pub struct Limits {
    pub head_lines: usize,
    pub head_bytes: usize,
    pub tail_lines: usize,
    pub tail_bytes: usize,
}

pub const SINGLE: Limits = Limits {
    head_lines: 100,
    head_bytes: 16 * 1024,
    tail_lines: 100,
    tail_bytes: 16 * 1024,
};
pub const PER_PEER: Limits = Limits {
    head_lines: 50,
    head_bytes: 8 * 1024,
    tail_lines: 50,
    tail_bytes: 8 * 1024,
};

fn prefix_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

fn prefix_bytes(s: &str, n: usize) -> &str {
    let mut end = n.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Clip a line, preserving its newline when the byte budget permits it. The omitted count is characters.
pub fn clip_line_bytes(text: &str, max_chars: usize, max_bytes: usize) -> (String, usize) {
    let newline = text.ends_with('\n');
    let body = if newline {
        &text[..text.len() - 1]
    } else {
        text
    };
    let chars = body.chars().count();
    if chars <= max_chars && text.len() <= max_bytes {
        return (text.to_string(), 0);
    }
    let nl = if newline && max_bytes > 0 { "\n" } else { "" };
    let mark = if max_bytes >= LINE_CUT_MARK.len() + nl.len() {
        LINE_CUT_MARK
    } else {
        ""
    };
    let room = max_bytes.saturating_sub(mark.len() + nl.len());
    let prefix = prefix_bytes(prefix_chars(body, max_chars), room);
    let omitted = chars - prefix.chars().count() + usize::from(newline && nl.is_empty());
    (format!("{prefix}{mark}{nl}"), omitted)
}

pub fn clip_line(text: &str, max_chars: usize) -> (String, usize) {
    clip_line_bytes(text, max_chars, usize::MAX)
}

struct Line {
    chunks: Vec<(bool, String)>,
    bytes: usize,
    kept_source_bytes: usize,
}

/// A bounded line buffer plus a rolling head/tail. stdout and stderr share one budget in every mode.
/// The sink receives completed head lines; finish flushes an unterminated line and returns the tail.
pub struct HeadTail {
    lim: Limits,
    head_lines: usize,
    head_bytes: usize,
    head_done: bool,
    tail: VecDeque<Line>,
    tail_bytes: usize,
    cur: Vec<(bool, String)>,
    cur_chars: usize,
    cur_source_bytes: usize,
    cur_kept_bytes: usize,
    cur_err: bool,
    pub omitted: usize,
    pub omitted_bytes: usize,
}

impl HeadTail {
    pub fn new(lim: Limits) -> Self {
        Self {
            lim,
            head_lines: 0,
            head_bytes: 0,
            head_done: false,
            tail: VecDeque::new(),
            tail_bytes: 0,
            cur: Vec::new(),
            cur_chars: 0,
            cur_source_bytes: 0,
            cur_kept_bytes: 0,
            cur_err: false,
            omitted: 0,
            omitted_bytes: 0,
        }
    }

    pub fn truncated(&self) -> bool {
        self.omitted_bytes > 0 || self.omitted > 0
    }

    fn append(&mut self, is_err: bool, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some((err, last)) = self.cur.last_mut() {
            if *err == is_err {
                last.push_str(text);
                return;
            }
        }
        self.cur.push((is_err, text.to_string()));
    }

    pub fn feed(&mut self, is_err: bool, text: &str, sink: &mut impl FnMut(bool, &str)) {
        for piece in text.split_inclusive('\n') {
            let newline = piece.ends_with('\n');
            let body = if newline {
                &piece[..piece.len() - 1]
            } else {
                piece
            };
            self.cur_err = is_err;
            self.cur_source_bytes += body.len();
            let keep = prefix_chars(body, MAX_LINE_CHARS.saturating_sub(self.cur_chars));
            self.cur_chars += keep.chars().count();
            self.cur_kept_bytes += keep.len();
            self.append(is_err, keep);
            if newline {
                self.push_line(true, sink);
            }
        }
    }

    fn push_line(&mut self, newline: bool, sink: &mut impl FnMut(bool, &str)) {
        let cut = self.cur_source_bytes > self.cur_kept_bytes;
        let mut source_kept = self.cur_kept_bytes + usize::from(newline);
        let source_total = self.cur_source_bytes + usize::from(newline);
        // A single retained line must fit either byte window, even for custom tiny limits.
        let line_budget = if self.head_done {
            self.lim.tail_bytes
        } else {
            self.lim.head_bytes.max(self.lim.tail_bytes)
        };
        let suffix_bytes = usize::from(newline) + if cut { LINE_CUT_MARK.len() } else { 0 };
        if self.cur_kept_bytes + suffix_bytes > line_budget {
            let room = line_budget.saturating_sub(LINE_CUT_MARK.len() + usize::from(newline));
            let mut remaining = room;
            let mut kept = 0;
            for (_, chunk) in &mut self.cur {
                let n = prefix_bytes(chunk, remaining).len();
                chunk.truncate(n);
                remaining -= n;
                kept += n;
            }
            self.cur.retain(|(_, s)| !s.is_empty());
            self.cur_kept_bytes = kept;
            source_kept = kept + usize::from(newline && line_budget > 0);
        }
        let needs_mark = self.cur_source_bytes > self.cur_kept_bytes;
        let room = line_budget.saturating_sub(self.cur_kept_bytes + usize::from(newline));
        if needs_mark && room >= LINE_CUT_MARK.len() {
            self.append(self.cur_err, LINE_CUT_MARK);
        }
        if newline && line_budget > 0 {
            self.append(self.cur_err, "\n");
        }
        let raw_kept_bytes = self.cur_kept_bytes;
        let mut chunks = std::mem::take(&mut self.cur);
        let bytes = chunks.iter().map(|(_, s)| s.len()).sum::<usize>();
        self.omitted_bytes += source_total.saturating_sub(source_kept);
        self.cur_chars = 0;
        self.cur_source_bytes = 0;
        self.cur_kept_bytes = 0;
        if !self.head_done
            && self.head_lines < self.lim.head_lines
            && self.head_bytes + bytes <= self.lim.head_bytes
        {
            self.head_lines += 1;
            self.head_bytes += bytes;
            for (err, chunk) in chunks {
                sink(err, &chunk);
            }
            if self.head_lines >= self.lim.head_lines || self.head_bytes == self.lim.head_bytes {
                self.head_done = true;
            }
            return;
        }
        self.head_done = true;
        // A line first encountered at the head's byte boundary may be larger than the tail budget.
        if bytes > self.lim.tail_bytes {
            let mut room = self.lim.tail_bytes;
            let mut retained_source = 0;
            for (_, s) in &mut chunks {
                let n = prefix_bytes(s, room).len();
                s.truncate(n);
                room -= n;
                retained_source += n;
            }
            let retained_source = retained_source.min(raw_kept_bytes)
                + usize::from(newline && retained_source == bytes);
            self.omitted_bytes += source_kept - retained_source;
            source_kept = retained_source;
        }
        let bytes = chunks.iter().map(|(_, s)| s.len()).sum::<usize>();
        self.tail_bytes += bytes;
        self.tail.push_back(Line {
            chunks,
            bytes,
            kept_source_bytes: source_kept,
        });
        while self.tail.len() > self.lim.tail_lines || self.tail_bytes > self.lim.tail_bytes {
            if let Some(line) = self.tail.pop_front() {
                self.tail_bytes -= line.bytes;
                self.omitted += 1;
                self.omitted_bytes += line.kept_source_bytes;
            }
        }
    }

    pub fn finish(&mut self) -> Vec<(bool, String)> {
        let mut last_head = Vec::new();
        if self.cur_source_bytes > 0 {
            self.push_line(false, &mut |err, text| {
                last_head.push((err, text.to_string()))
            });
        }
        last_head.extend(self.tail.drain(..).flat_map(|line| line.chunks));
        self.tail_bytes = 0;
        last_head
    }
}

/// Clip a complete stream. The second return value counts wholly omitted lines; byte omissions
/// (including long-line cuts) are included in the notice even when that line count is zero.
#[cfg(test)]
pub fn clip_text(chunks: &[&str], lim: Limits, log: Option<&str>) -> (String, usize) {
    let mut ht = HeadTail::new(lim);
    let mut out = String::new();
    for c in chunks {
        ht.feed(false, c, &mut |_, s| out.push_str(s));
    }
    let tail = ht.finish();
    if ht.truncated() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&omit_note(ht.omitted, ht.omitted_bytes, log));
        out.push('\n');
    }
    for (_, line) in tail {
        out.push_str(&line);
    }
    (out, ht.omitted)
}

pub fn omit_note(omitted: usize, bytes: usize, log: Option<&str>) -> String {
    let suffix = log.map_or_else(
        || "; use --full to retrieve it".to_string(),
        |p| format!(", full output: {p}"),
    );
    format!("[agent-cli-x: {omitted} lines omitted ({bytes} bytes){suffix}]")
}

pub struct ExecOutput {
    json: bool,
    full: bool,
    ht: HeadTail,
    json_out: String,
    json_err: String,
    full_kept: usize,
    full_omitted_bytes: usize,
    full_omitted_lines: usize,
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
            json_out: String::new(),
            json_err: String::new(),
            full_kept: 0,
            full_omitted_bytes: 0,
            full_omitted_lines: 0,
            log,
            log_path,
        }
    }

    pub fn feed(&mut self, is_err: bool, text: &str) {
        if let Some(f) = self.log.as_mut() {
            let _ = f.write_all(text.as_bytes());
        }
        if self.full {
            if !self.json {
                emit(is_err, text);
                return;
            }
            let keep = prefix_bytes(text, JSON_FULL_CAP.saturating_sub(self.full_kept));
            self.full_kept += keep.len();
            self.full_omitted_bytes += text.len() - keep.len();
            self.full_omitted_lines += text[keep.len()..].matches('\n').count();
            if is_err {
                self.json_err.push_str(keep);
            } else {
                self.json_out.push_str(keep);
            }
        } else if self.json {
            let (out, err) = (&mut self.json_out, &mut self.json_err);
            self.ht.feed(is_err, text, &mut |e, s| {
                if e {
                    err.push_str(s)
                } else {
                    out.push_str(s)
                }
            });
        } else {
            self.ht.feed(is_err, text, &mut emit);
        }
    }

    pub fn finish(mut self, exit: &Value) -> i32 {
        drop(self.log.take());
        let code = exit["code"].as_i64().unwrap_or(-1) as i32;
        let timed_out = exit["timed_out"].as_bool().unwrap_or(false);
        let log_str = self.log_path.to_string_lossy().to_string();
        let tail = if self.full {
            Vec::new()
        } else {
            self.ht.finish()
        };
        let omitted = self.ht.omitted + self.full_omitted_lines;
        let omitted_bytes = self.ht.omitted_bytes + self.full_omitted_bytes;
        let truncated = self.ht.truncated() || self.full_omitted_bytes > 0;
        if self.json {
            if truncated {
                if !self.json_out.is_empty() && !self.json_out.ends_with('\n') {
                    self.json_out.push('\n');
                }
                self.json_out
                    .push_str(&omit_note(omitted, omitted_bytes, Some(&log_str)));
                self.json_out.push('\n');
            }
            for (err, text) in tail {
                if err {
                    self.json_err.push_str(&text);
                } else {
                    self.json_out.push_str(&text);
                }
            }
            println!(
                "{}",
                json!({
                    "exit_code": code, "timed_out": timed_out, "ms": exit["ms"],
                    "stdout": self.json_out, "stderr": self.json_err, "truncated": truncated,
                    "omitted_lines": omitted, "omitted_bytes": omitted_bytes,
                    "log": truncated.then_some(log_str),
                })
            );
        } else {
            if truncated {
                eprintln!("\n{}", omit_note(omitted, omitted_bytes, Some(&log_str)));
            }
            for (err, line) in tail {
                emit(err, &line);
            }
            if timed_out {
                eprintln!("[agent-cli-x: command timed out, process tree killed]");
            }
        }
        if !truncated {
            let _ = fs::remove_file(&self.log_path);
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
        head_bytes: 16 * 1024,
        tail_lines: 2,
        tail_bytes: 16 * 1024,
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
        assert!(s.starts_with("1\n2\n3\n[agent-cli-x: 5 lines omitted (10 bytes)"));
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

    #[test]
    fn a_single_cut_line_counts_as_truncated() {
        let text = "x".repeat(5000);
        let mut ht = HeadTail::new(SINGLE);
        let mut out = String::new();
        ht.feed(false, &text, &mut |_, s| out.push_str(s));
        for (_, s) in ht.finish() {
            out.push_str(&s);
        }
        assert!(ht.truncated());
        assert_eq!(ht.omitted, 0);
        assert_eq!(ht.omitted_bytes, 3000);
        assert!(out.ends_with(LINE_CUT_MARK));
        assert_eq!(out.chars().count(), 2000 + LINE_CUT_MARK.chars().count());
    }

    #[test]
    fn byte_windows_are_strict_and_utf8_safe() {
        let limits = Limits {
            head_lines: 100,
            head_bytes: 33,
            tail_lines: 100,
            tail_bytes: 17,
        };
        let mut ht = HeadTail::new(limits);
        let mut head = String::new();
        for _ in 0..100 {
            ht.feed(
                false,
                "汉字汉字汉字汉字汉字汉字汉字\n",
                &mut |_, s| head.push_str(s),
            );
        }
        let tail: String = ht.finish().into_iter().map(|(_, s)| s).collect();
        assert!(head.len() <= limits.head_bytes);
        assert!(tail.len() <= limits.tail_bytes);
        assert!(ht.truncated());
    }

    #[test]
    fn huge_unterminated_input_keeps_a_bounded_buffer() {
        let mut ht = HeadTail::new(PER_PEER);
        let piece = "z".repeat(16384);
        for _ in 0..1000 {
            ht.feed(false, &piece, &mut |_, _| {});
        }
        assert_eq!(ht.cur_kept_bytes, 2000);
        assert!(ht.cur.iter().map(|(_, s)| s.len()).sum::<usize>() <= 2000);
        let tail = ht.finish();
        assert!(ht.truncated());
        assert!(tail.into_iter().map(|(_, s)| s.len()).sum::<usize>() < 8192);
    }

    #[test]
    fn clip_line_respects_both_character_and_byte_limits() {
        let (line, omitted) = clip_line_bytes(&format!("{}\n", "汉".repeat(3000)), 2000, 64);
        assert!(line.len() <= 64);
        assert!(line.ends_with('\n'));
        assert!(omitted > 0);
        assert!(line.contains(LINE_CUT_MARK));
        assert_eq!(clip_line("abc\n", 3), ("abc\n".into(), 0));
        let (line, omitted) = clip_line_bytes("汉字\n", 2, 1);
        assert_eq!(line, "\n");
        assert_eq!(omitted, 2);
        assert_eq!(clip_line_bytes("\n", 1, 0), (String::new(), 1));
    }

    #[test]
    fn byte_omissions_count_source_bytes_without_markers() {
        let text = "汉".repeat(3000) + "\n";
        let limits = Limits {
            head_lines: 1,
            head_bytes: 32,
            tail_lines: 1,
            tail_bytes: 8,
        };
        let mut ht = HeadTail::new(limits);
        let mut head = String::new();
        ht.feed(false, &text, &mut |_, s| head.push_str(s));
        for (_, s) in ht.finish() {
            head.push_str(&s);
        }
        let kept_source = head.replace(LINE_CUT_MARK, "").len();
        assert_eq!(ht.omitted_bytes, text.len() - kept_source);
    }

    #[test]
    fn a_single_cut_line_preserves_the_complete_local_log() {
        let log_path =
            std::env::temp_dir().join(format!("agent-cli-x-output-{}.log", util::random_hex(8)));
        let log = File::create(&log_path).unwrap();
        let mut output = ExecOutput {
            json: true,
            full: false,
            ht: HeadTail::new(SINGLE),
            json_out: String::new(),
            json_err: String::new(),
            full_kept: 0,
            full_omitted_bytes: 0,
            full_omitted_lines: 0,
            log: Some(log),
            log_path: log_path.clone(),
        };
        let text = "x".repeat(5000) + "\n";
        output.feed(false, &text);
        assert_eq!(output.finish(&json!({"code": 0})), 0);
        assert_eq!(fs::read_to_string(&log_path).unwrap(), text);
        fs::remove_file(log_path).unwrap();
    }

    #[test]
    fn mixed_streams_share_one_head_tail_budget() {
        let mut ht = HeadTail::new(PER_PEER);
        let mut head = Vec::new();
        let row = "x".repeat(200) + "\n";
        for n in 0..200 {
            ht.feed(n % 2 == 0, &row, &mut |err, text| {
                head.push((err, text.to_string()))
            });
        }
        let tail = ht.finish();
        assert!(head.iter().map(|(_, s)| s.len()).sum::<usize>() <= PER_PEER.head_bytes);
        assert!(tail.iter().map(|(_, s)| s.len()).sum::<usize>() <= PER_PEER.tail_bytes);
        assert!(head.iter().any(|(err, _)| *err));
        assert!(head.iter().any(|(err, _)| !*err));
        assert!(ht.truncated());
    }
}
