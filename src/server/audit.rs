//! 审计日志：`<home>/audit/YYYY-MM-DD.jsonl`，只追加。

use crate::util;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Audit {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl Audit {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        Self {
            dir,
            lock: Mutex::new(()),
        }
    }

    /// `caller` 是通过鉴权的 token 名称（默认 token 为 `default`，鉴权失败为空）。
    pub fn log(&self, ip: &str, caller: &str, op: &str, ok: bool, detail: Value) {
        let line = json!({"ts": util::now_iso(), "ip": ip, "caller": caller, "op": op, "ok": ok, "detail": detail});
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.dir.join(format!("{}.jsonl", util::today()));
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}
