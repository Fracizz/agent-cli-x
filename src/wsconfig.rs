//! `agent-cli-x workspace add|remove`：保留注释地修改 config.toml，按 serve 启动时的规则校验后写回，再重启 serve 生效。
//! 本机直接执行；远程时由对端 serve（`/v1/workspace`）调用同一套逻辑。不必手改 TOML（容易把键写进错误的表里）。

use crate::client::admin;
use crate::config;
use crate::error::{AppError, Result};
use crate::server::workspace::{Access, Workspaces};
use crate::{tls, util};
use serde_json::{json, Value};
use std::path::Path;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

/// 解析可编辑文档；失败时说明 serve 可能正以 last-good 运行。
fn parse_doc(text: &str, p: &Path) -> Result<DocumentMut> {
    text.parse::<DocumentMut>().map_err(|e| {
        AppError::config(format!(
            "failed to parse {}: {e}. Fix it by hand first (`agent-cli-x serve --check` validates)",
            p.display()
        ))
    })
}

/// `allow` 只能由 r c u d 组成（各至多一次）。
fn check_allow(allow: &str) -> Result<()> {
    let mut seen = String::new();
    for ch in allow.chars() {
        if !"rcud".contains(ch) || seen.contains(ch) {
            return Err(AppError::bad_request(format!(
                "--allow `{allow}`: only r c u d, each at most once (e.g. rcud, r)"
            )));
        }
        seen.push(ch);
    }
    Ok(())
}

/// 在配置文本中登记（或更新）工作区；`allow` 给出时写入 `[[policy.paths]] path = "<name>:"` 规则。
pub fn add_to(
    text: &str,
    p: &Path,
    name: &str,
    path: &str,
    default: bool,
    allow: Option<&str>,
) -> Result<String> {
    let mut doc = parse_doc(text, p)?;
    let ws = doc
        .entry("workspaces")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| AppError::config("`workspaces` in config.toml is not a table"))?;
    ws.insert(name, value(path));
    if default {
        // 根表的键总是渲染在第一个 [表] 之前，不会落进别的表
        doc["default_workspace"] = value(name);
    }
    if let Some(allow) = allow {
        let policy = doc
            .entry("policy")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| AppError::config("`policy` in config.toml is not a table"))?;
        let rules = policy
            .entry("paths")
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
            .as_array_of_tables_mut()
            .ok_or_else(|| {
                AppError::config(
                    "policy.paths in config.toml is not written as [[policy.paths]]; add the rule by hand",
                )
            })?;
        let scope = format!("{name}:");
        let found = rules
            .iter()
            .position(|t| t.get("path").and_then(|v| v.as_str()) == Some(scope.as_str()));
        match found.and_then(|i| rules.get_mut(i)) {
            Some(t) => t["allow"] = value(allow),
            None => {
                let mut t = Table::new();
                t["path"] = value(scope);
                t["allow"] = value(allow);
                rules.push(t);
            }
        }
    }
    Ok(doc.to_string())
}

/// 从配置文本中删除工作区，连同默认工作区设置与以 `<name>:` 开头的规则；返回新文本和删掉的规则数。
pub fn remove_from(text: &str, p: &Path, name: &str) -> Result<(String, usize)> {
    let mut doc = parse_doc(text, p)?;
    let removed = doc
        .get_mut("workspaces")
        .and_then(|w| w.as_table_like_mut())
        .and_then(|w| w.remove(name));
    if removed.is_none() {
        return Err(AppError::not_found(format!(
            "workspace `{name}` is not in config.toml; see `agent-cli-x whoami`"
        )));
    }
    if doc.get("default_workspace").and_then(|v| v.as_str()) == Some(name) {
        doc.remove("default_workspace");
    }
    let scope = format!("{name}:");
    let mut rules = 0;
    if let Some(policy) = doc.get_mut("policy").and_then(|v| v.as_table_mut()) {
        if let Some(paths) = policy
            .get_mut("paths")
            .and_then(|v| v.as_array_of_tables_mut())
        {
            let before = paths.len();
            paths.retain(|t| {
                !t.get("path")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.starts_with(&scope))
            });
            rules += before - paths.len();
        }
        for key in ["deny_paths", "readonly_paths"] {
            if let Some(list) = policy.get_mut(key).and_then(|v| v.as_array_mut()) {
                let before = list.len();
                list.retain(|v| !v.as_str().is_some_and(|s| s.starts_with(&scope)));
                rules += before - list.len();
            }
        }
    }
    Ok((doc.to_string(), rules))
}

/// 按 serve 启动时的规则校验新配置文本。
fn validate(home: &Path, text: &str) -> Result<Workspaces> {
    let cfg = config::parse_config(text, &config::config_path(home))?;
    Workspaces::new(&cfg, home)
}

/// 写回已校验的配置。
pub fn save(home: &Path, text: &str) -> Result<()> {
    let p = config::config_path(home);
    util::atomic_write(&p, text.as_bytes())?;
    util::restrict_permissions(&p);
    Ok(())
}

fn read_config(home: &Path) -> Result<String> {
    let p = config::config_path(home);
    std::fs::read_to_string(&p).map_err(|e| {
        AppError::config(format!(
            "failed to read {}: {e}. Run `agent-cli-x init` first",
            p.display()
        ))
    })
}

/// 计算登记工作区后的配置（不写盘）：返回新文本与结果（含新工作区缺少的权限）。
/// `path` 是本机路径；远程登记时由对端 serve 调用。
pub fn plan_add(
    home: &Path,
    name: &str,
    path: &str,
    default: bool,
    allow: Option<&str>,
) -> Result<(String, Value)> {
    config::validate_name(name, "workspace")?;
    if let Some(a) = allow {
        check_allow(a)?;
    }
    let canon = dunce::canonicalize(path)
        .map_err(|e| AppError::bad_request(format!("invalid workspace path `{path}`: {e}")))?;
    if !canon.is_dir() {
        return Err(AppError::bad_request(format!(
            "workspace path `{path}` is not a directory"
        )));
    }
    let canon = canon.to_string_lossy().replace('\\', "/");
    let p = config::config_path(home);
    let text = add_to(&read_config(home)?, &p, name, &canon, default, allow)?;
    let ws = validate(home, &text)?;
    // 预设规则（如 Windows 的 C:/ 只读）可能让新工作区缺少写权限，调用方据此给出放开的命令
    let root = ws.list().find(|(n, _)| *n == name).map(|(_, r)| r.clone());
    let missing: String = root
        .map(|r| {
            [
                (Access::Read, 'r'),
                (Access::Create, 'c'),
                (Access::Update, 'u'),
                (Access::Delete, 'd'),
            ]
            .into_iter()
            .filter(|(a, _)| ws.check_abs(name, &r, &r, name, *a).is_err())
            .map(|(_, c)| c)
            .collect()
        })
        .unwrap_or_default();
    let v = json!({
        "workspace": name,
        "path": canon,
        "default": ws.default_name() == Some(name),
        "missing": missing,
    });
    Ok((text, v))
}

/// 计算删除工作区后的配置（不写盘）。
pub fn plan_remove(home: &Path, name: &str) -> Result<(String, Value)> {
    let p = config::config_path(home);
    let (text, rules) = remove_from(&read_config(home)?, &p, name)?;
    validate(home, &text)?;
    Ok((text, json!({"workspace": name, "removed_rules": rules})))
}

/// 本机：写回配置，再重启正在运行的本机 serve 使其生效。
pub fn apply_local(planned: (String, Value), force: bool) -> Result<Value> {
    let home = config::home_dir();
    let (text, mut v) = planned;
    let cfg = config::parse_config(&text, &config::config_path(&home))?;
    let me = admin::local_client(&cfg, tls::local_fingerprint(&home)?);
    save(&home, &text)?;
    v["serve"] = json!(match admin::restart(&me, force) {
        Ok(_) => "restarted",
        Err(e) if e.code == "connect" => "not_running",
        Err(e) => {
            return Err(AppError::new(
                &e.code,
                format!(
                    "config saved, but restarting the local serve failed: {}. Restart it to apply",
                    e.message
                ),
            ));
        }
    });
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "# head\nnode_name = \"m\"\ntoken = \"0123456789abcdef0123\"\n\n# default_workspace = \"proj\"\n\n[workspaces]\n# proj = \"D:/code/proj\"\n\n[policy]\ndeny_paths = [\n  \"**/.env\",\n]\n\n[[policy.paths]]\npath = \"C:/\"\nallow = \"r\"\n";

    fn cfg(text: &str) -> config::Config {
        config::parse_config(text, Path::new("config.toml")).unwrap()
    }

    #[test]
    fn add_keeps_comments_and_puts_keys_in_the_right_table() {
        let p = Path::new("config.toml");
        let t = add_to(BASE, p, "dd", "D:/", true, Some("rcud")).unwrap();
        assert!(t.contains("# head") && t.contains("# proj = "), "{t}");
        let c = cfg(&t);
        assert_eq!(c.workspaces["dd"], "D:/");
        assert_eq!(c.default_workspace.as_deref(), Some("dd"));
        assert_eq!(c.policy.paths.len(), 2);
        assert_eq!(c.policy.paths[1].path, "dd:");
        assert_eq!(c.policy.paths[1].allow, "rcud");
        // 再次 add 更新路径与已有规则，而不是重复追加
        let t = add_to(&t, p, "dd", "E:/", false, Some("r")).unwrap();
        let c = cfg(&t);
        assert_eq!(c.workspaces["dd"], "E:/");
        assert_eq!(c.policy.paths.len(), 2);
        assert_eq!(c.policy.paths[1].allow, "r");
    }

    #[test]
    fn add_creates_missing_workspaces_table() {
        let text = "token = \"0123456789abcdef0123\"\n\n[policy]\nmax_jobs = 4\n";
        let t = add_to(text, Path::new("c"), "proj", "/srv/p", true, None).unwrap();
        let c = cfg(&t);
        assert_eq!(c.workspaces["proj"], "/srv/p");
        assert_eq!(c.default_workspace.as_deref(), Some("proj"));
        assert_eq!(c.policy.max_jobs, 4);
    }

    #[test]
    fn remove_drops_default_and_scoped_rules() {
        let p = Path::new("config.toml");
        let t = add_to(BASE, p, "dd", "D:/", true, Some("rcud")).unwrap();
        let t = t.replace("\"**/.env\",", "\"**/.env\",\n  \"dd:secrets\",");
        let (t, rules) = remove_from(&t, p, "dd").unwrap();
        assert_eq!(rules, 2);
        let c = cfg(&t);
        assert!(c.workspaces.is_empty());
        assert_eq!(c.default_workspace, None);
        assert_eq!(c.policy.paths.len(), 1);
        assert_eq!(c.policy.deny_paths, vec!["**/.env"]);
        assert_eq!(remove_from(&t, p, "dd").unwrap_err().code, "not_found");
    }

    #[test]
    fn allow_letters() {
        assert!(check_allow("rcud").is_ok());
        assert!(check_allow("").is_ok());
        assert!(check_allow("rr").is_err());
        assert!(check_allow("rw").is_err());
    }
}
