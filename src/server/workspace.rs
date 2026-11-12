//! 工作区解析与黑名单路径守卫。
//!
//! 外部路径格式为 `工作区:相对路径`。解析时：拒绝绝对路径、`..`、`:`（NTFS 备用数据流）、
//! Windows 保留名；对已存在部分做 canonicalize，阻止符号链接 / junction 越出工作区；
//! 最终按真实路径检查权限：deny_paths 硬性禁止，readonly_paths / [[policy.paths]] 按 CRUD 细分。

use crate::config::Config;
use crate::error::{AppError, Result};
use globset::{GlobBuilder, GlobMatcher};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

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

/// 对路径的一种操作（CRUD）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// ls / read / grep / get，以及作为 exec 工作目录
    Read,
    /// 新建文件或目录（write / put 目标不存在时）
    Create,
    /// 修改已有文件（write 覆盖 / edit / put 覆盖）
    Update,
    /// 删除（rm）
    Delete,
}

impl Access {
    fn bit(self) -> u8 {
        match self {
            Access::Read => 1,
            Access::Create => 2,
            Access::Update => 4,
            Access::Delete => 8,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Create => "create",
            Access::Update => "update",
            Access::Delete => "delete",
        }
    }
}

/// 权限集合，用字母表示：r 读取、c 新建、u 修改、d 删除；空串表示无任何权限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perm(u8);

impl Perm {
    pub const ALL: Perm = Perm(15);
    pub const NONE: Perm = Perm(0);

    pub fn parse(s: &str) -> Option<Perm> {
        let mut bits = 0;
        for ch in s.trim().chars() {
            bits |= match ch.to_ascii_lowercase() {
                'r' => 1,
                'c' => 2,
                'u' => 4,
                'd' => 8,
                '-' => 0,
                _ => return None,
            };
        }
        Some(Perm(bits))
    }

    pub fn allows(self, a: Access) -> bool {
        self.0 & a.bit() != 0
    }
}

impl std::fmt::Display for Perm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == 0 {
            return f.write_str("none");
        }
        for (b, c) in [(1, 'r'), (2, 'c'), (4, 'u'), (8, 'd')] {
            if self.0 & b != 0 {
                write!(f, "{c}")?;
            }
        }
        Ok(())
    }
}

struct PathRule {
    raw: String,
    scope: Option<String>,
    absolute: bool,
    matcher: GlobMatcher,
    allow: Perm,
    /// 不含通配符（* ? [ {）的规则；同一层命中时比通配规则更具体
    literal: bool,
    /// `工作区:` 形式（冒号后为空）：只匹配该工作区根目录本身
    ws_root: bool,
}

/// 某路径的权限判定结果。
pub struct Verdict {
    pub perm: Perm,
    /// 决定该结果的规则；没有规则命中时为 None（完全权限）。
    pub rule: Option<String>,
    /// 是否由 deny_paths / 数据目录这类不可覆盖的硬性禁止决定。
    pub hard: bool,
}

pub struct Workspaces {
    roots: BTreeMap<String, PathBuf>,
    default: Option<String>,
    /// deny_paths：硬性禁止，命中自身或任一父目录即无任何权限，不能被更具体的规则放开。
    deny: Vec<PathRule>,
    /// readonly_paths 与 [[policy.paths]]：离目标最近（最具体）的一层命中规则决定权限。
    perms: Vec<PathRule>,
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
            "absolute path `{raw}` not allowed; use workspace:relative/path"
        )));
    }
    let mut out = Vec::new();
    for part in s.split('/') {
        match part {
            "" | "." => continue,
            ".." => return Err(AppError::denied("`..` is not allowed")),
            p => {
                if p.contains(':') || p.contains('\0') {
                    return Err(AppError::denied(format!(
                        "file name `{p}` has illegal characters"
                    )));
                }
                if cfg!(windows) && is_reserved_windows_name(p) {
                    return Err(AppError::denied(format!(
                        "`{p}` is a reserved Windows name"
                    )));
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
            let c = dunce::canonicalize(p).map_err(|e| {
                AppError::config(format!("workspace {name} path `{p}` invalid: {e}"))
            })?;
            if !c.is_dir() {
                return Err(AppError::config(format!(
                    "workspace {name} path `{p}` is not a directory"
                )));
            }
            roots.insert(name.clone(), c);
        }
        let default = match &cfg.default_workspace {
            Some(d) if !roots.contains_key(d) => {
                return Err(AppError::config(format!(
                    "default_workspace `{d}` is not in [workspaces]"
                )))
            }
            Some(d) => Some(d.clone()),
            None if roots.len() == 1 => roots.keys().next().cloned(),
            None => None,
        };
        let p = &cfg.policy;
        let deny = p
            .deny_paths
            .iter()
            .map(|raw| compile_rule(raw, &roots, "deny_paths", Perm::NONE))
            .collect::<Result<Vec<_>>>()?;
        let mut perms = Vec::new();
        for raw in &p.readonly_paths {
            perms.push(compile_rule(raw, &roots, "readonly_paths", Perm(1))?);
        }
        for rule in &p.paths {
            let allow = Perm::parse(&rule.allow).ok_or_else(|| {
                AppError::config(format!(
                    "[[policy.paths]] `{}` has invalid allow = `{}`: only r c u d allowed (empty string = no access)",
                    rule.path, rule.allow
                ))
            })?;
            perms.push(compile_rule(&rule.path, &roots, "[[policy.paths]]", allow)?);
        }
        let home = dunce::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        Ok(Self {
            roots,
            default,
            deny,
            perms,
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
        self.deny.iter().map(|r| r.raw.clone()).collect()
    }

    /// 权限规则列表：`(规则, 允许的权限)`。
    pub fn perm_rules(&self) -> Vec<(String, String)> {
        self.perms
            .iter()
            .map(|r| (r.raw.clone(), r.allow.to_string()))
            .collect()
    }

    /// 解析路径并要求读取权限。
    pub fn resolve(&self, spec: &str) -> Result<Resolved> {
        let r = self.resolve_unchecked(spec)?;
        self.check(&r, Access::Read)?;
        Ok(r)
    }

    /// 解析将要写入的文件路径：已存在要求修改权限，不存在要求新建权限。
    pub fn resolve_write(&self, spec: &str) -> Result<Resolved> {
        let r = self.resolve_unchecked(spec)?;
        self.check(
            &r,
            if r.exists {
                Access::Update
            } else {
                Access::Create
            },
        )?;
        Ok(r)
    }

    /// 只做路径安全解析（工作区、`..`、符号链接越界等），不检查权限；调用方须自行 [`check`](Self::check)。
    pub fn resolve_unchecked(&self, spec: &str) -> Result<Resolved> {
        let (ws, rel_raw) = self.split_spec(spec)?;
        let root = self.roots[&ws].clone();
        let comps = parse_rel(rel_raw)?;
        let joined = comps.iter().fold(root.clone(), |p, c| p.join(c));
        let (abs, exists) = canonical_within(&root, &joined)?;
        let rel = abs.strip_prefix(&root).map(norm).unwrap_or_default();
        Ok(Resolved {
            ws,
            root,
            abs,
            rel,
            exists,
        })
    }

    /// 同 [`resolve_unchecked`](Self::resolve_unchecked)，但最后一级若是符号链接 / junction，
    /// 返回链接本身而不是其目标（rm 用：删除链接不能误删目标）。
    pub fn resolve_entry(&self, spec: &str) -> Result<Resolved> {
        let (ws, rel_raw) = self.split_spec(spec)?;
        let comps = parse_rel(rel_raw)?;
        let Some((last, dirs)) = comps.split_last() else {
            return self.resolve_unchecked(spec);
        };
        let root = self.roots[&ws].clone();
        let joined = dirs.iter().fold(root.clone(), |p, c| p.join(c));
        let (parent, _) = canonical_within(&root, &joined)?;
        let abs = parent.join(last);
        let exists = std::fs::symlink_metadata(&abs).is_ok();
        let rel = abs.strip_prefix(&root).map(norm).unwrap_or_default();
        Ok(Resolved {
            ws,
            root,
            abs,
            rel,
            exists,
        })
    }

    /// 拆出 `工作区:相对路径`；没写工作区时用默认工作区。
    fn split_spec<'a>(&self, spec: &'a str) -> Result<(String, &'a str)> {
        match spec.split_once(':') {
            Some((w, r)) if self.roots.contains_key(w) => Ok((w.to_string(), r)),
            Some((w, _)) if w.len() > 1 => {
                let names: Vec<_> = self.roots.keys().cloned().collect();
                Err(AppError::not_found(format!(
                    "unknown workspace `{w}`; available: {}",
                    names.join(", ")
                )))
            }
            _ => {
                let d = self.default.clone().ok_or_else(|| {
                    AppError::bad_request(
                        "no workspace given; use `workspace:relative/path` (e.g. proj:src/main.rs)",
                    )
                })?;
                Ok((d, spec))
            }
        }
    }

    /// 检查已解析路径是否允许某种操作。
    pub fn check(&self, r: &Resolved, access: Access) -> Result<()> {
        self.check_abs(&r.ws, &r.root, &r.abs, &r.display(), access)
    }

    pub fn check_abs(
        &self,
        ws: &str,
        root: &Path,
        abs: &Path,
        display: &str,
        access: Access,
    ) -> Result<()> {
        let v = self.verdict(ws, root, abs);
        if v.perm.allows(access) {
            return Ok(());
        }
        let rule = v.rule.unwrap_or_default();
        Err(AppError::denied(if v.hard {
            format!("path `{display}` matches denylist `{rule}`; access denied")
        } else {
            format!(
                "path `{display}` does not allow {} (rule `{rule}` allows: {})",
                access.label(),
                v.perm
            )
        }))
    }

    /// 根据绝对路径反查所属工作区（rollback 用）。
    pub fn owner_of(&self, abs: &Path) -> Option<(String, PathBuf)> {
        self.roots
            .iter()
            .find(|(_, r)| abs.starts_with(r))
            .map(|(n, r)| (n.clone(), r.clone()))
    }

    /// 不可读时返回原因规则（ls / grep / get 遍历时用来跳过）。
    pub fn denied_by(&self, ws: &str, root: &Path, abs: &Path) -> Option<String> {
        let v = self.verdict(ws, root, abs);
        (!v.perm.allows(Access::Read)).then(|| v.rule.unwrap_or_default())
    }

    /// 计算路径的有效权限：
    /// 1. 数据目录、deny_paths 命中自身或任一父目录 → 无任何权限（不可覆盖）；
    /// 2. 否则从路径本身逐级向上，第一层有规则命中的祖先决定权限；
    ///    同一层里不含通配符的规则优先于通配规则，同类多条取交集；
    /// 3. 都没命中 → 完全权限。
    pub fn verdict(&self, ws: &str, root: &Path, abs: &Path) -> Verdict {
        if abs.starts_with(&self.home) {
            return Verdict {
                perm: Perm::NONE,
                rule: Some("<agent-cli-x data dir>".to_string()),
                hard: true,
            };
        }
        for a in abs.ancestors() {
            let hit = self.deny.iter().find(|r| rule_matches(r, ws, root, a));
            if let Some(r) = hit {
                return Verdict {
                    perm: Perm::NONE,
                    rule: Some(r.raw.clone()),
                    hard: true,
                };
            }
        }
        for a in abs.ancestors() {
            let matched: Vec<&PathRule> = self
                .perms
                .iter()
                .filter(|r| rule_matches(r, ws, root, a))
                .collect();
            // 同一层里不含通配符的规则优先（"src" 比 "**" 更具体）
            let has_literal = matched.iter().any(|r| r.literal);
            let mut perm = Perm::ALL;
            let mut hits = Vec::new();
            for r in matched.into_iter().filter(|r| r.literal || !has_literal) {
                perm = Perm(perm.0 & r.allow.0);
                hits.push(r.raw.as_str());
            }
            if !hits.is_empty() {
                return Verdict {
                    perm,
                    rule: Some(hits.join("`, `")),
                    hard: false,
                };
            }
        }
        Verdict {
            perm: Perm::ALL,
            rule: None,
            hard: false,
        }
    }
}

/// 规则是否命中某一层路径 `a`：绝对规则按真实路径匹配；相对规则按相对工作区根的路径匹配
/// （工作区根本身及其外部不参与相对匹配）。
fn rule_matches(r: &PathRule, ws: &str, root: &Path, a: &Path) -> bool {
    if r.scope.as_deref().is_some_and(|s| s != ws) {
        return false;
    }
    if r.ws_root {
        return a == root;
    }
    if r.absolute {
        return r.matcher.is_match(norm(a));
    }
    match a.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => r.matcher.is_match(norm(rel)),
        _ => false,
    }
}

fn compile_rule(
    raw: &str,
    roots: &BTreeMap<String, PathBuf>,
    field: &str,
    allow: Perm,
) -> Result<PathRule> {
    let mut s = raw.trim().replace('\\', "/");
    // 去掉末尾的 `/`，但保留根目录 `/` 与盘符根 `C:/`
    while s.len() > 1 && s.ends_with('/') && !(s.len() == 3 && is_abs_pattern(&s)) {
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
        // `proj:` 表示工作区根目录（作用于整个工作区）
        if let Some(ws) = scope {
            return Ok(PathRule {
                raw: raw.to_string(),
                scope: Some(ws),
                absolute: false,
                literal: true,
                ws_root: true,
                matcher: GlobBuilder::new("")
                    .build()
                    .map_err(|e| AppError::config(e.to_string()))?
                    .compile_matcher(),
                allow,
            });
        }
        return Err(AppError::config(format!("{field} rule `{raw}` is empty")));
    }
    let absolute = scope.is_none() && is_abs_pattern(&s);
    let matcher = GlobBuilder::new(&s)
        .literal_separator(true)
        .case_insensitive(cfg!(windows))
        .backslash_escape(false)
        .build()
        .map_err(|e| AppError::config(format!("{field} rule `{raw}` invalid: {e}")))?
        .compile_matcher();
    Ok(PathRule {
        raw: raw.to_string(),
        scope,
        absolute,
        literal: !s.contains(['*', '?', '[', '{']),
        ws_root: false,
        matcher,
        allow,
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
    let canon = dunce::canonicalize(&existing).map_err(|e| {
        AppError::not_found(format!("cannot resolve path {}: {e}", existing.display()))
    })?;
    if !canon.starts_with(root) {
        return Err(AppError::denied(
            "path escapes the workspace via symlink / junction; access denied",
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
        setup_ro(deny, &[])
    }

    fn setup_ro(deny: &[&str], ro: &[&str]) -> (PathBuf, Workspaces) {
        setup_full(deny, ro, &[])
    }

    fn setup_full(deny: &[&str], ro: &[&str], paths: &[(&str, &str)]) -> (PathBuf, Workspaces) {
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
            node_name: None,
            node_id: None,
            ai_cmd: String::new(),
            default_workspace: None,
            workspaces,
            policy: Policy {
                deny_paths: deny.iter().map(|s| s.to_string()).collect(),
                readonly_paths: ro.iter().map(|s| s.to_string()).collect(),
                paths: paths
                    .iter()
                    .map(|(p, a)| crate::config::PathRule {
                        path: p.to_string(),
                        allow: a.to_string(),
                    })
                    .collect(),
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
                .unwrap_or_else(|| panic!("{bad} should be rejected"));
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
        let rule = compile_rule(&norm(&abs), &roots, "deny_paths", Perm::NONE).unwrap();
        assert!(rule.absolute);
        let ws_abs = Workspaces {
            deny: vec![rule],
            ..ws
        };
        assert!(ws_abs.resolve("proj:src/main.rs").is_err());
        assert!(ws_abs.resolve("proj:secrets").is_ok());
    }

    #[test]
    fn readonly_paths_allow_read_block_write() {
        let (_b, ws) = setup_ro(&[], &["src"]);
        assert!(ws.resolve("proj:src/main.rs").is_ok());
        let e = ws.resolve_write("proj:src/main.rs").err().unwrap();
        assert_eq!(e.code, "policy_denied");
        assert!(ws.resolve_write("proj:src/new/x.txt").is_err());
        assert!(ws.resolve_write("proj:secrets/b.txt").is_ok());
    }

    #[test]
    fn root_rules_keep_trailing_slash() {
        let roots = BTreeMap::new();
        let r = compile_rule("C:/", &roots, "x", Perm::NONE).unwrap();
        assert!(r.absolute);
        assert!(r.matcher.is_match("C:/"));
        assert!(!r.matcher.is_match("C:"));
        let r = compile_rule("/", &roots, "x", Perm::NONE).unwrap();
        assert!(r.matcher.is_match("/"));
        assert_eq!(
            compile_rule("/etc/", &roots, "x", Perm::NONE).unwrap().raw,
            "/etc/"
        );
        assert!(compile_rule("/etc/", &roots, "x", Perm::NONE)
            .unwrap()
            .matcher
            .is_match("/etc"));
    }

    #[test]
    fn readonly_whole_root_ancestor() {
        // 绝对规则命中工作区的某个祖先目录时，整个工作区只读
        let (base, ws) = setup(&[]);
        let anc = norm(&dunce::canonicalize(&base).unwrap());
        let mut roots = BTreeMap::new();
        roots.insert(
            "proj".to_string(),
            dunce::canonicalize(base.join("proj")).unwrap(),
        );
        let ws = Workspaces {
            perms: vec![compile_rule(&anc, &roots, "x", Perm(1)).unwrap()],
            ..ws
        };
        assert!(ws.resolve("proj:src/main.rs").is_ok());
        assert!(ws.resolve_write("proj:src/main.rs").is_err());
    }

    #[test]
    fn perm_parse_and_display() {
        assert_eq!(Perm::parse("rcud"), Some(Perm::ALL));
        assert_eq!(Perm::parse("DR"), Some(Perm(9)));
        assert_eq!(Perm::parse(""), Some(Perm::NONE));
        assert_eq!(Perm::parse("rx"), None);
        assert_eq!(Perm(9).to_string(), "rd");
        assert_eq!(Perm::NONE.to_string(), "none");
    }

    #[test]
    fn crud_rules_most_specific_wins() {
        // 整个工作区只读，src 放开读+新建，src/deep 放开全部；secrets 由 deny 硬性禁止，更具体规则也放不开
        let (base, ws) = setup_full(
            &["secrets"],
            &[],
            &[
                ("proj:", "r"),
                ("src", "rc"),
                ("src/deep", "rcud"),
                ("secrets/inner", "rcud"),
            ],
        );
        let root = dunce::canonicalize(base.join("proj")).unwrap();
        let v = |rel: &str| ws.verdict("proj", &root, &root.join(rel)).perm;
        assert_eq!(v("README.md"), Perm(1));
        assert_eq!(v("src"), Perm(3));
        assert_eq!(v("src/main.rs"), Perm(3));
        assert_eq!(v("src/deep/x.rs"), Perm::ALL);
        assert_eq!(v("secrets/inner/a.txt"), Perm::NONE);
        // src 下：新建可以、覆盖不行
        assert!(ws.resolve_write("proj:src/new.rs").is_ok());
        let e = ws.resolve_write("proj:src/main.rs").err().unwrap();
        assert!(e.message.contains("does not allow update"), "{}", e.message);
        // 通配规则命中文件本身（最深一层），因此比目录规则更具体
        let (_b, ws4) = setup_full(&[], &[], &[("src", "rcud"), ("**/*.rs", "r")]);
        let root4 = ws4.roots["proj"].clone();
        assert_eq!(
            ws4.verdict("proj", &root4, &root4.join("src/a.rs")).perm,
            Perm(1)
        );
        assert_eq!(
            ws4.verdict("proj", &root4, &root4.join("src/a.md")).perm,
            Perm::ALL
        );
        // 同层多条取交集
        let (_b, ws2) = setup_full(&[], &[], &[("src", "rcu"), ("src", "rd")]);
        let root2 = ws2.roots["proj"].clone();
        assert_eq!(
            ws2.verdict("proj", &root2, &root2.join("src/a")).perm,
            Perm(1)
        );
        // 非法 allow 报配置错误
        let mut cfg_err = None;
        let base3 =
            std::env::temp_dir().join(format!("agent-cli-x-ws-{}", crate::util::random_hex(6)));
        fs::create_dir_all(&base3).unwrap();
        let mut workspaces = BTreeMap::new();
        workspaces.insert("proj".to_string(), base3.to_string_lossy().to_string());
        let cfg = Config {
            listen: String::new(),
            token: "x".repeat(20),
            node_name: None,
            node_id: None,
            ai_cmd: String::new(),
            default_workspace: None,
            workspaces,
            policy: Policy {
                paths: vec![crate::config::PathRule {
                    path: "a".into(),
                    allow: "rw".into(),
                }],
                ..Policy::default()
            },
        };
        if let Err(e) = Workspaces::new(&cfg, &base3.join("home")) {
            cfg_err = Some(e.message);
        }
        assert!(cfg_err.unwrap().contains("allow"));
    }

    #[test]
    fn resolve_entry_keeps_symlink() {
        let (base, ws) = setup(&[]);
        let root = dunce::canonicalize(base.join("proj")).unwrap();
        let link = root.join("src/link");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(root.join("secrets"), &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(root.join("secrets"), &link).is_ok();
        if !made {
            return; // Windows 未开启开发者模式时无法创建符号链接，跳过
        }
        let e = ws.resolve_entry("proj:src/link").unwrap();
        assert_eq!(e.rel, "src/link");
        assert!(e.exists);
        let full = ws.resolve_unchecked("proj:src/link").unwrap();
        assert_eq!(full.rel, "secrets");
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
