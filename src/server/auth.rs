//! 鉴权：config.toml 的默认 token + tokens.toml 中按调用方签发的 token。
//! 每次鉴权前检查两个文件的修改时间，变化后立即重新加载：签发/吊销 token 无需重启 serve。

use crate::config::{self, DEFAULT_TOKEN_NAME};
use crate::util;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

pub struct Auth {
    home: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    cfg_mtime: Option<SystemTime>,
    tok_mtime: Option<SystemTime>,
    default: String,
    /// (名称, token 的 sha256, 是否禁止借道)
    named: Vec<(String, String, bool)>,
}

/// 通过鉴权的调用方。
pub struct Caller {
    pub name: String,
    pub no_relay: bool,
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl Auth {
    pub fn new(home: &Path, default_token: &str) -> Self {
        let a = Self {
            home: home.to_path_buf(),
            inner: Mutex::new(Inner {
                cfg_mtime: mtime(&config::config_path(home)),
                tok_mtime: None,
                default: default_token.to_string(),
                named: Vec::new(),
            }),
        };
        {
            let mut g = a.inner.lock().unwrap_or_else(|e| e.into_inner());
            a.reload_tokens(&mut g);
        }
        a
    }

    /// 已签发的 token 数（不含默认 token）。
    pub fn named_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .named
            .len()
    }

    fn reload_tokens(&self, g: &mut Inner) {
        let p = config::tokens_path(&self.home);
        g.tok_mtime = mtime(&p);
        match config::load_tokens(&self.home) {
            Ok(t) => {
                g.named = t
                    .tokens
                    .into_iter()
                    .filter(|(n, _)| n != DEFAULT_TOKEN_NAME)
                    .map(|(n, e)| (n, e.sha256.to_ascii_lowercase(), e.no_relay))
                    .collect();
            }
            // 解析失败时保留旧列表，避免一次误编辑让所有调用方掉线
            Err(e) => crate::slog!("tokens.toml load failed, keeping old token list: {e}"),
        }
    }

    fn maybe_reload(&self, g: &mut Inner) {
        let cfg_m = mtime(&config::config_path(&self.home));
        if cfg_m != g.cfg_mtime {
            g.cfg_mtime = cfg_m;
            match config::read_config_token(&self.home) {
                Some(t) if t != g.default => {
                    g.default = t;
                    crate::slog!("reloaded default token from config.toml");
                }
                Some(_) => {}
                None => crate::slog!("token in config.toml invalid, keeping old default token"),
            }
        }
        if mtime(&config::tokens_path(&self.home)) != g.tok_mtime {
            self.reload_tokens(g);
            crate::slog!("reloaded tokens.toml ({} caller tokens)", g.named.len());
        }
    }

    /// 校验 token，成功时返回调用方。
    pub fn check(&self, got: &str) -> Option<Caller> {
        if got.is_empty() {
            return None;
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.maybe_reload(&mut g);
        let mut who = None;
        if util::ct_eq(got.as_bytes(), g.default.as_bytes()) {
            who = Some(Caller {
                name: DEFAULT_TOKEN_NAME.to_string(),
                no_relay: false,
            });
        }
        let h = util::sha256_hex(got.as_bytes());
        for (name, sha, no_relay) in &g.named {
            if util::ct_eq(h.as_bytes(), sha.as_bytes()) && who.is_none() {
                who = Some(Caller {
                    name: name.clone(),
                    no_relay: *no_relay,
                });
            }
        }
        who
    }
}
