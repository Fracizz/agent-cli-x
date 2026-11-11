//! serve 运行日志：同时写 stderr 与 `<home>/serve-logs/serve-YYYY-MM-DD.log`（保留 7 天）。
//! 以开机自启方式在后台运行时看不到终端输出，排查问题只能靠这里的日志文件。

use crate::util;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const RETAIN_DAYS: u64 = 7;

struct Logger {
    dir: PathBuf,
    /// 上次写入的日期：跨天时顺带清理过期日志。
    day: Mutex<String>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

pub fn dir(home: &Path) -> PathBuf {
    home.join("serve-logs")
}

/// 初始化日志目录并安装 panic 钩子（panic 信息也写入日志文件）。
pub fn init(home: &Path) {
    let d = dir(home);
    let _ = fs::create_dir_all(&d);
    util::prune_older_than(&d, RETAIN_DAYS);
    let _ = LOGGER.set(Logger {
        dir: d,
        day: Mutex::new(util::today()),
    });
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        line(&format!("panic: {info}"));
        prev(info);
    }));
}

/// 写一行日志。未初始化时只写 stderr。
pub fn line(msg: &str) {
    let _ = writeln!(std::io::stderr(), "{msg}");
    let Some(lg) = LOGGER.get() else { return };
    let today = util::today();
    let mut day = lg.day.lock().unwrap_or_else(|e| e.into_inner());
    if *day != today {
        util::prune_older_than(&lg.dir, RETAIN_DAYS);
        *day = today.clone();
    }
    let path = lg.dir.join(format!("serve-{today}.log"));
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {msg}", util::now_iso());
    }
}

/// 格式化后写入 serve 日志。
#[macro_export]
macro_rules! slog {
    ($($t:tt)*) => {
        $crate::server::log::line(&format!($($t)*))
    };
}
