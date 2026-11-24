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

/// 已签发 token 的分类计数（不含默认 token）。
#[derive(Debug, Default, PartialEq)]
pub struct Codes {
    /// 仍有效的授权码
    pub valid: usize,
    /// 已到期的授权码名称
    pub expired: Vec<String>,
    /// 长期 token（非授权码）数
    pub permanent: usize,
}

struct Inner {
    cfg_mtime: Option<SystemTime>,
    tok_mtime: Option<SystemTime>,
    default: String,
    /// (名称, token 的 sha256, 是否禁止借道, 授权码到期时间)
    named: Vec<(String, String, bool, Option<u64>)>,
}

/// 鉴权结果。
pub enum Check {
    Ok(Caller),
    /// 授权码已过期（携带名称，便于给出续期命令）
    Expired(String),
    Invalid,
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

    /// 按当前 tokens.toml（有变化先重新加载）统计授权码与长期 token。
    pub fn codes(&self, now: u64) -> Codes {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.maybe_reload(&mut g);
        let mut c = Codes::default();
        for (name, _, _, expires) in &g.named {
            match expires {
                None => c.permanent += 1,
                Some(e) if now >= *e => c.expired.push(name.clone()),
                Some(_) => c.valid += 1,
            }
        }
        c
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
                    .map(|(n, e)| (n, e.sha256.to_ascii_lowercase(), e.no_relay, e.expires))
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

    /// 校验 token，成功时返回调用方；授权码到期返回 `Expired(名称)`。
    pub fn check(&self, got: &str) -> Check {
        if got.is_empty() {
            return Check::Invalid;
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
        let now = util::unix_now();
        let mut expired = None;
        for (name, sha, no_relay, expires) in &g.named {
            if util::ct_eq(h.as_bytes(), sha.as_bytes()) && who.is_none() && expired.is_none() {
                if expires.is_some_and(|e| now >= e) {
                    expired = Some(name.clone());
                } else {
                    who = Some(Caller {
                        name: name.clone(),
                        no_relay: *no_relay,
                    });
                }
            }
        }
        match (who, expired) {
            (Some(c), _) => Check::Ok(c),
            (None, Some(n)) => Check::Expired(n),
            (None, None) => Check::Invalid,
        }
    }
}
