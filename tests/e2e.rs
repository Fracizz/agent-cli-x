//! 端到端：在临时目录起一个真实 serve，用另一个 AGENT_CLI_X_HOME 作为客户端走完整链路。

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_agent-cli-x");

struct Env {
    server_home: PathBuf,
    client_home: PathBuf,
    ws: PathBuf,
    _server: ServerGuard,
}

/// serve 进程守卫：无论 setup 中途断言失败还是测试结束都会结束进程并清理目录。
struct ServerGuard {
    child: Child,
    base: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("AGENT_CLI_X_HOME", home)
        .stdin(Stdio::null())
        .output()
        .expect("运行 agent-cli-x 失败")
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn setup() -> Env {
    let base = std::env::temp_dir().join(format!(
        "agent-cli-x-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let server_home = base.join("server-home");
    let client_home = base.join("client-home");
    let ws = base.join("proj");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::create_dir_all(ws.join("secrets")).unwrap();
    fs::create_dir_all(&client_home).unwrap();
    fs::write(
        ws.join("src/main.rs"),
        "fn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();
    fs::write(ws.join("secrets/token.txt"), "TOP-SECRET").unwrap();
    fs::write(ws.join(".env"), "K=V").unwrap();

    let port = free_port();
    let listen = format!("127.0.0.1:{port}");
    let ws_arg = format!("proj={}", ws.display());
    let o = run(
        &server_home,
        &["init", "--listen", &listen, "--workspace", &ws_arg],
    );
    assert!(o.status.success(), "init 失败: {}", text(&o));

    // 追加目录黑名单
    let cfg_path = server_home.join("config.toml");
    let cfg = fs::read_to_string(&cfg_path).unwrap();
    let cfg = cfg.replacen("deny_paths = [", "deny_paths = [\n  \"secrets\",", 1);
    fs::write(&cfg_path, cfg).unwrap();

    let child = Command::new(BIN)
        .arg("serve")
        .env("AGENT_CLI_X_HOME", &server_home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = ServerGuard {
        child,
        base: base.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(&listen).is_err() {
        assert!(Instant::now() < deadline, "serve 未在 10 秒内启动");
        std::thread::sleep(Duration::from_millis(100));
    }

    let token = cfg_token(&cfg_path);
    let fp = String::from_utf8(run(&server_home, &["fingerprint"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    // 不带指纹：只显示指纹并以 106 退出，不登记
    let o = run(
        &client_home,
        &["peer", "add", "b", &listen, "--token", &token],
    );
    assert_eq!(o.status.code(), Some(106), "{}", text(&o));
    assert!(text(&o).contains(&fp));
    // 指纹错误：拒绝
    let o = run(
        &client_home,
        &[
            "peer",
            "add",
            "b",
            &listen,
            "--token",
            &token,
            "--fingerprint",
            "sha256:00",
        ],
    );
    assert_eq!(o.status.code(), Some(106));
    // 正确登记
    let o = run(
        &client_home,
        &[
            "peer",
            "add",
            "b",
            &listen,
            "--token",
            &token,
            "--fingerprint",
            &fp,
        ],
    );
    assert!(o.status.success(), "peer add 失败: {}", text(&o));

    Env {
        server_home,
        client_home,
        ws,
        _server: server,
    }
}

fn cfg_token(p: &Path) -> String {
    let cfg = fs::read_to_string(p).unwrap();
    let line = cfg.lines().find(|l| l.starts_with("token")).unwrap();
    line.split('"').nth(1).unwrap().to_string()
}

#[test]
fn full_flow() {
    let env = setup();
    let c = &env.client_home;

    // info
    let o = run(c, &["info", "b", "--json"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("\"workspaces\""));

    // exec：输出与退出码透传
    let o = run(c, &["exec", "b", "echo", "hello-agent"]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(String::from_utf8_lossy(&o.stdout).contains("hello-agent"));
    let o = run(c, &["exec", "b", "exit 7"]);
    assert_eq!(o.status.code(), Some(7), "{}", text(&o));

    // exec 中文输出不乱码
    let o = run(c, &["exec", "b", "echo 中文输出"]);
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("中文输出"),
        "{}",
        text(&o)
    );

    // exec 超时
    let sleep_cmd = if cfg!(windows) {
        "Start-Sleep -Seconds 30"
    } else {
        "sleep 30"
    };
    let started = Instant::now();
    let o = run(c, &["exec", "b", "--timeout", "2", sleep_cmd]);
    assert_eq!(o.status.code(), Some(104), "{}", text(&o));
    assert!(started.elapsed() < Duration::from_secs(20));

    // 黑名单命令
    let o = run(c, &["exec", "b", "git push --force origin main"]);
    assert_eq!(o.status.code(), Some(102), "{}", text(&o));

    // 黑名单目录：read / ls / exec --cwd / write 都被拒绝
    assert_eq!(
        run(c, &["read", "b", "proj:secrets/token.txt"])
            .status
            .code(),
        Some(102)
    );
    assert_eq!(run(c, &["read", "b", "proj:.env"]).status.code(), Some(102));
    assert_eq!(
        run(c, &["exec", "b", "--cwd", "proj:secrets", "echo x"])
            .status
            .code(),
        Some(102)
    );
    assert_eq!(
        run(c, &["write", "b", "proj:secrets/new.txt", "--content", "x"])
            .status
            .code(),
        Some(102)
    );
    let o = run(c, &["ls", "b", "proj:", "--depth", "3"]);
    assert!(o.status.success(), "{}", text(&o));
    let listing = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(listing.contains("src/main.rs"), "{listing}");
    assert!(
        !listing.contains("secrets"),
        "黑名单目录不应出现在 ls 中: {listing}"
    );
    assert!(!listing.contains(".env"), "{listing}");

    // 越界
    assert_eq!(run(c, &["read", "b", "proj:../x"]).status.code(), Some(102));

    // read
    let o = run(c, &["read", "b", "proj:src/main.rs", "--lines", "2:2"]);
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(
        String::from_utf8_lossy(&o.stdout).trim(),
        "println!(\"hi\");"
    );

    // edit + rollback
    let o = run(
        c,
        &[
            "edit",
            "b",
            "proj:src/main.rs",
            "--old",
            "\"hi\"",
            "--new",
            "\"hello\"",
            "--json",
        ],
    );
    assert!(o.status.success(), "{}", text(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let op = v["op_id"].as_str().unwrap().to_string();
    assert!(fs::read_to_string(env.ws.join("src/main.rs"))
        .unwrap()
        .contains("\"hello\""));
    // 不唯一 / 不存在
    assert_eq!(
        run(
            c,
            &[
                "edit",
                "b",
                "proj:src/main.rs",
                "--old",
                "nothing-here",
                "--new",
                "x"
            ]
        )
        .status
        .code(),
        Some(105)
    );
    let o = run(c, &["rollback", "b", &op]);
    assert!(o.status.success(), "{}", text(&o));
    assert!(fs::read_to_string(env.ws.join("src/main.rs"))
        .unwrap()
        .contains("\"hi\""));

    // write 新文件 + expect-sha 冲突 + rollback 删除新文件
    let o = run(
        c,
        &[
            "write",
            "b",
            "proj:out/new.txt",
            "--content",
            "abc",
            "--mkdir",
            "--json",
        ],
    );
    assert!(o.status.success(), "{}", text(&o));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(
        fs::read_to_string(env.ws.join("out/new.txt")).unwrap(),
        "abc"
    );
    let o = run(
        c,
        &[
            "write",
            "b",
            "proj:out/new.txt",
            "--content",
            "zzz",
            "--expect-sha",
            "deadbeef",
        ],
    );
    assert_eq!(o.status.code(), Some(105));
    run(c, &["rollback", "b", v["op_id"].as_str().unwrap()]);
    assert!(!env.ws.join("out/new.txt").exists());

    // 审计日志已写入
    let audit_dir = env.server_home.join("audit");
    let n = fs::read_dir(&audit_dir).unwrap().count();
    assert!(n >= 1);

    // token 错误
    let peers_path = c.join("peers.toml");
    let peers = fs::read_to_string(&peers_path).unwrap();
    let token = cfg_token(&env.server_home.join("config.toml"));
    fs::write(
        &peers_path,
        peers.replace(&token, "wrong-token-wrong-token"),
    )
    .unwrap();
    assert_eq!(run(c, &["info", "b"]).status.code(), Some(101));
}
