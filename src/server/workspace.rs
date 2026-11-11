//! 工作区解析与黑名单路径守卫。
//!
//! 外部路径格式为 `工作区:相对路径`。解析时：拒绝绝对路径、`..`、`:`（NTFS 备用数据流）、
//! Windows 保留名；对已存在部分做 canonicalize，阻止符号链接 / junction 越出工作区；
//! 最终按真实路径（含所有父目录）检查黑名单。

use crate::config::Config;
use crate::error::{AppError, Result};
use globset::{GlobBuilder, GlobMatcher};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

pub struct Resolved {
    pub ws: String,
    pub root: PathBuf,
    pub abs: PathBuf,
    /// 相对工作区根目录，`/` 分隔；根目录为空串。
    pub rel: String,
    pub exists: bool,
}

impl Resolved {
    pub fn display(&self) -> String {
        format!("{}:{}", self.ws, self.rel)
    }
}

struct DenyRule {
    raw: String,
    scope: Option<String>,
    absolute: bool,
    matcher: GlobMatcher,
}

pub struct Workspaces {
    roots: BTreeMap<String, PathBuf>,
    default: Option<String>,
    rules: Vec<DenyRule>,
    /// agent-cli-x 数据目录（含 token / 私钥），始终禁止访问。
    home: PathBuf,
}

fn is_abs_pattern(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/') || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic())
}

/// 统一为 `/` 分隔的字符串，供 glob 匹配。
fn norm(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn is_reserved_windows_name(part: &str) -> bool {
    let stem = part
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
}

fn parse_rel(raw: &str) -> Result<Vec<String>> {
    let s = raw.replace('\\', "/");
    if s.starts_with('/') || is_abs_pattern(&s) {
        return Err(AppError::denied(format!(
            "不允许绝对路径 `{raw}`，请使用 工作区:相对路径"
        )));
    }
    let mut out = Vec::new();
    for part in s.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(AppError::denied("不允许使用 `..` 跳出目录")),
            p => {
                if p.contains(':') || p.contains('\0') {
                    return Err(AppError::denied(format!("文件名 `{p}` 含非法字符")));
                }
                if cfg!(windows) && is_reserved_windows_name(p) {
                    return Err(AppError::denied(format!("`{p}` 是 Windows 保留名")));
                }
                out.push(p.to_string());
            }
        }
    }
    Ok(out)
}

impl Workspaces {
    pub fn new(cfg: &Config, home: &Path) -> Result<Self> {
        let mut roots = BTreeMap::new();
        for (name, p) in &cfg.workspaces {
            let c = dunce::canonicalize(p)
                .map_err(|e| AppError::config(format!("工作区 {name} 的路径 `{p}` 无效: {e}")))?;
            if !c.is_dir() {
                return Err(AppError::config(format!(
                    "工作区 {name} 的路径 `{p}` 不是目录"
                )));
            }
            roots.insert(name.clone(), c);
        }
        let default = match &cfg.default_workspace {
            Some(d) if !roots.contains_key(d) => {
                return Err(AppError::config(format!(
                    "default_workspace `{d}` 不在 [workspaces] 中"
                )))
            }
            Some(d) => Some(d.clone()),
            None if roots.len() == 1 => roots.keys().next().cloned(),
            None => None,
        };
        let mut rules = Vec::new();
        for raw in &cfg.policy.deny_paths {
            rules.push(compile_rule(raw, &roots)?);
        }
        let home = dunce::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        Ok(Self {
            roots,
            default,
            rules,
            home,
        })
    }

    pub fn list(&self) -> impl Iterator<Item = (&String, &PathBuf)> {
        self.roots.iter()
    }

    pub fn default_name(&self) -> Option<&str> {
        self.default.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn deny_rules(&self) -> Vec<String> {
        self.rules.iter().map(|r| r.raw.clone()).collect()
    }

    pub fn resolve(&self, spec: &str) -> Result<Resolved> {
        let (ws, rel_raw) = match spec.split_once(':') {
            Some((w, r)) if self.roots.contains_key(w) => (w.to_string(), r),
            Some((w, _)) if w.len() > 1 => {
                let names: Vec<_> = self.roots.keys().cloned().collect();
                return Err(AppError::not_found(format!(
                    "未知工作区 `{w}`，可用: {}",
                    names.join(", ")
                )));
            }
            _ => {
                let d = self.default.clone().ok_or_else(|| {
                    AppError::bad_request(
                        "未指定工作区，请使用 `工作区:相对路径` 格式（如 proj:src/main.rs）",
                    )
                })?;
                (d, spec)
            }
        };
        let root = self.roots[&ws].clone();
        let comps = parse_rel(rel_raw)?;
        let joined = comps.iter().fold(root.clone(), |p, c| p.join(c));
        let (abs, exists) = canonical_within(&root, &joined)?;
        if let Some(rule) = self.denied_by(&ws, &root, &abs) {
            return Err(AppError::denied(format!(
                "路径 `{ws}:{}` 命中黑名单 `{rule}`，禁止访问",
                comps.join("/")
            )));
        }
        let rel = abs.strip_prefix(&root).map(norm).unwrap_or_default();
        Ok(Resolved {
            ws,
            root,
            abs,
            rel,
            exists,
        })
    }

    /// 根据绝对路径反查所属工作区（rollback 用）。
    pub fn owner_of(&self, abs: &Path) -> Option<(String, PathBuf)> {
        self.roots
            .iter()
            .find(|(_, r)| abs.starts_with(r))
            .map(|(n, r)| (n.clone(), r.clone()))
    }

    /// 返回命中的黑名单规则；路径本身或任一父目录命中都算。
    pub fn denied_by(&self, ws: &str, root: &Path, abs: &Path) -> Option<String> {
        if abs.starts_with(&self.home) {
            return Some("<agent-cli-x 数据目录>".to_string());
        }
        let mut rel_prefixes = Vec::new();
        if let Ok(rel) = abs.strip_prefix(root) {
            let mut acc = String::new();
            for c in rel.components() {
                if let Component::Normal(s) = c {
                    if !acc.is_empty() {
                        acc.push('/');
                    }
                    acc.push_str(&s.to_string_lossy());
                    rel_prefixes.push(acc.clone());
                }
            }
        }
        for r in &self.rules {
            if r.scope.as_deref().is_some_and(|s| s != ws) {
                continue;
            }
            let hit = if r.absolute {
                abs.ancestors().any(|a| r.matcher.is_match(norm(a)))
            } else {
                rel_prefixes.iter().any(|p| r.matcher.is_match(p))
            };
            if hit {
                return Some(r.raw.clone());
            }
        }
        None
    }
}

fn compile_rule(raw: &str, roots: &BTreeMap<String, PathBuf>) -> Result<DenyRule> {
    let mut s = raw.trim().replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    let mut scope = None;
    if let Some((pre, rest)) = s.split_once(':') {
        if pre.len() > 1 && roots.contains_key(pre) {
            scope = Some(pre.to_string());
            s = rest.trim_start_matches('/').to_string();
        }
    }
    if s.is_empty() {
        return Err(AppError::config(format!("deny_paths 规则 `{raw}` 为空")));
    }
    let absolute = scope.is_none() && is_abs_pattern(&s);
    let matcher = GlobBuilder::new(&s)
        .literal_separator(true)
        .case_insensitive(cfg!(windows))
        .backslash_escape(false)
        .build()
        .map_err(|e| AppError::config(format!("deny_paths 规则 `{raw}` 无效: {e}")))?
        .compile_matcher();
    Ok(DenyRule {
        raw: raw.to_string(),
        scope,
        absolute,
        matcher,
    })
}

/// 对最近的已存在祖先做 canonicalize 并校验仍在 root 内，再拼回不存在的尾部。
fn canonical_within(root: &Path, joined: &Path) -> Result<(PathBuf, bool)> {
    let mut existing = joined.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    while std::fs::symlink_metadata(&existing).is_err() {
        if existing == root {
            break;
        }
        match existing.file_name() {
            Some(n) => tail.push(n.to_os_string()),
            None => break,
        }
        existing.pop();
    }
    let canon = dunce::canonicalize(&existing)
        .map_err(|e| AppError::not_found(format!("无法解析路径 {}: {e}", existing.display())))?;
    if !canon.starts_with(root) {
        return Err(AppError::denied(
            "路径经符号链接 / junction 指向工作区之外，禁止访问",
        ));
    }
    let mut abs = canon;
    for t in tail.iter().rev() {
        abs.push(t);
    }
    Ok((abs, tail.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Policy;
    use std::fs;

    fn setup(deny: &[&str]) -> (PathBuf, Workspaces) {
        let base =
            std::env::temp_dir().join(format!("agent-cli-x-ws-{}", crate::util::random_hex(6)));
        let root = base.join("proj");
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join("secrets/inner")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/.env"), "K=V").unwrap();
        fs::write(root.join("secrets/inner/a.txt"), "x").unwrap();
        let mut workspaces = BTreeMap::new();
        workspaces.insert("proj".to_string(), root.to_string_lossy().to_string());
        let cfg = Config {
            listen: String::new(),
            token: "x".repeat(20),
            ai_cmd: String::new(),
            default_workspace: None,
            workspaces,
            policy: Policy {
                deny_paths: deny.iter().map(|s| s.to_string()).collect(),
                ..Policy::default()
            },
        };
        let home = base.join("home");
        fs::create_dir_all(&home).unwrap();
        let ws = Workspaces::new(&cfg, &home).unwrap();
        (base, ws)
    }

    #[test]
    fn resolves_normal_paths() {
        let (_b, ws) = setup(&[]);
        let r = ws.resolve("proj:src/main.rs").unwrap();
        assert!(r.exists);
        assert_eq!(r.rel, "src/main.rs");
        // 单工作区时可省略前缀
        assert_eq!(ws.resolve("src\\main.rs").unwrap().rel, "src/main.rs");
        let n = ws.resolve("proj:src/new/file.txt").unwrap();
        assert!(!n.exists);
        assert_eq!(n.rel, "src/new/file.txt");
    }

    #[test]
    fn rejects_escapes() {
        let (_b, ws) = setup(&[]);
        for bad in [
            "proj:../x",
            "proj:/etc/passwd",
            "proj:C:/Windows",
            "proj:a/../../x",
            "proj:file.txt:stream",
        ] {
            let e = ws
                .resolve(bad)
                .err()
                .unwrap_or_else(|| panic!("{bad} 应被拒绝"));
            assert_eq!(e.code, "policy_denied", "{bad}");
        }
        assert_eq!(ws.resolve("nope:x").err().unwrap().code, "not_found");
    }

    #[test]
    fn deny_paths_relative_and_ancestors() {
        let (_b, ws) = setup(&["secrets", "**/.env"]);
        assert!(ws.resolve("proj:secrets").is_err());
        assert!(ws.resolve("proj:secrets/inner/a.txt").is_err());
        assert!(ws.resolve("proj:secrets/new.txt").is_err());
        assert!(ws.resolve("proj:src/.env").is_err());
        assert!(ws.resolve("proj:src/main.rs").is_ok());
    }

    #[test]
    fn deny_paths_scoped_and_absolute() {
        let (base, ws) = setup(&["proj:src/deep"]);
        assert!(ws.resolve("proj:src/deep").is_err());
        assert!(ws.resolve("proj:src/main.rs").is_ok());

        // 绝对规则：用真实路径禁止 src 目录
        let abs = dunce::canonicalize(base.join("proj/src")).unwrap();
        let mut roots = BTreeMap::new();
        roots.insert(
            "proj".to_string(),
            dunce::canonicalize(base.join("proj")).unwrap(),
        );
        let rule = compile_rule(&norm(&abs), &roots).unwrap();
        assert!(rule.absolute);
        let ws_abs = Workspaces {
            rules: vec![rule],
            ..ws
        };
        assert!(ws_abs.resolve("proj:src/main.rs").is_err());
        assert!(ws_abs.resolve("proj:secrets").is_ok());
    }

    #[test]
    fn home_always_denied() {
        let (base, ws) = setup(&[]);
        let home = dunce::canonicalize(base.join("home")).unwrap();
        let root = dunce::canonicalize(base.join("proj")).unwrap();
        assert!(ws
            .denied_by("proj", &root, &home.join("config.toml"))
            .is_some());
    }
}
