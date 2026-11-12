//! Runtime regressions: EOF is independent of process lifetime; admission precedes TLS/body reads.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_agent-cli-x");

struct Server {
    base: PathBuf,
    home: PathBuf,
    client: PathBuf,
    addr: String,
    child: Child,
}

impl Server {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "agent-cli-x-runtime-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = base.join("server");
        let client = base.join("client");
        let ws = base.join("workspace");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&client).unwrap();
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string();
        let init = Command::new(BIN)
            .args(["init", "--listen", &addr, "--workspace"])
            .arg(format!("proj={}", ws.display()))
            .env("AGENT_CLI_X_HOME", &home)
            .env_remove("AGENT_CLI_X_NO_RELAY")
            .output()
            .unwrap();
        assert!(init.status.success(), "init failed");
        fs::create_dir_all(ws.join("readonly")).unwrap();
        let config_path = home.join("config.toml");
        let config_text = fs::read_to_string(&config_path).unwrap();
        fs::write(
            &config_path,
            format!(
                "{config_text}\n[[policy.paths]]\npath = \"proj:\"\nallow = \"rcud\"\n\n[[policy.paths]]\npath = \"proj:readonly\"\nallow = \"r\"\n"
            ),
        )
        .unwrap();
        let config: toml::Value =
            toml::from_str(&fs::read_to_string(home.join("config.toml")).unwrap()).unwrap();
        let token = config["token"].as_str().unwrap();
        let cert = fs::read(home.join("cert.der")).unwrap();
        let sha: String = ring::digest::digest(&ring::digest::SHA256, &cert)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        fs::write(
            client.join("peers.toml"),
            format!(
                "[peers.b]\naddr = {addr:?}\ntoken = {token:?}\nfingerprint = \"sha256:{sha}\"\n"
            ),
        )
        .unwrap();
        let child = Command::new(BIN)
            .arg("serve")
            .env("AGENT_CLI_X_HOME", &home)
            .env_remove("AGENT_CLI_X_NO_RELAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut server = Self {
            base,
            home,
            client,
            addr,
            child,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                panic!("serve failed to start: {status}");
            }
            if TcpStream::connect(&server.addr).is_ok() {
                // Allow the readiness probe's handler to release its admission slot.
                std::thread::sleep(Duration::from_millis(100));
                break;
            }
            assert!(Instant::now() < deadline, "serve startup timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
        server
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("AGENT_CLI_X_HOME", &self.client)
            .env_remove("AGENT_CLI_X_NO_RELAY")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    #[cfg(unix)]
    fn json(&self, args: &[&str]) -> serde_json::Value {
        let output = self.cli(args);
        assert!(
            output.status.success(),
            "command {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn tls(&self) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
        let cert =
            rustls::pki_types::CertificateDer::from(fs::read(self.home.join("cert.der")).unwrap());
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let tcp = TcpStream::connect(&self.addr).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let conn = rustls::ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::try_from("agent-cli-x").unwrap(),
        )
        .unwrap();
        rustls::StreamOwned::new(conn, tcp)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Detached runners deliberately survive serve. Clean up only this fixture's process groups.
        #[cfg(unix)]
        for entry in fs::read_dir(self.home.join("jobs"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let meta = fs::read(entry.path().join("meta.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            if let Some(meta) = meta.filter(|m| m["status"] == "running") {
                if let Some(pid) = meta["pid"].as_u64() {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[cfg(unix)]
#[test]
fn closed_output_still_times_out_and_returns_real_exit_status() {
    let server = Server::new();
    let started = Instant::now();
    let output = server.cli(&["exec", "b", "--timeout", "1", "exec 1>&- 2>&-; sleep 5"]);
    assert_eq!(output.status.code(), Some(104));
    assert!(started.elapsed() < Duration::from_secs(4));

    let output = server.cli(&[
        "exec",
        "b",
        "--timeout",
        "3",
        "exec 1>&- 2>&-; sleep 0.3; exit 7",
    ]);
    assert_eq!(output.status.code(), Some(7));
}

#[cfg(unix)]
#[test]
fn closed_output_jobs_honor_timeout_and_kill() {
    let server = Server::new();
    let job = server.json(&[
        "exec",
        "b",
        "--detach",
        "--timeout",
        "1",
        "--json",
        "exec 1>&- 2>&-; sleep 10",
    ]);
    let id = job["job_id"].as_str().unwrap();
    let output = server.cli(&["job", "logs", "b", id, "--follow", "--json"]);
    assert_eq!(output.status.code(), Some(104));

    let job = server.json(&[
        "exec",
        "b",
        "--detach",
        "--timeout",
        "30",
        "--json",
        "exec 1>&- 2>&-; sleep 10",
    ]);
    let id = job["job_id"].as_str().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let killed = server.json(&["job", "kill", "b", id, "--json"]);
    assert_eq!(killed["status"], "killed");
    let output = server.cli(&["job", "logs", "b", id, "--json"]);
    assert_eq!(output.status.code(), Some(137));
}

#[test]
fn invalid_token_is_rejected_before_waiting_for_body() {
    let server = Server::new();
    let mut tls = server.tls();
    write!(tls, "POST /v1/fs/write HTTP/1.1\r\nHost: agent-cli-x\r\nAuthorization: Bearer invalid\r\nContent-Length: 1048576\r\n\r\n").unwrap();
    tls.flush().unwrap();
    // The declared body is never sent: authentication must finish without reading it.
    let mut response = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = tls
            .read(&mut buf)
            .expect("server waited for unauthenticated request body");
        assert_ne!(n, 0, "connection closed without an auth response");
        response.extend_from_slice(&buf[..n]);
        if response.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    assert!(response.starts_with(b"HTTP/1.1 401 "));
}

#[test]
fn large_upload_with_invalid_token_preserves_auth_exit_code() {
    let server = Server::new();
    let peers_path = server.client.join("peers.toml");
    let mut peers: toml::Value = toml::from_str(&fs::read_to_string(&peers_path).unwrap()).unwrap();
    peers["peers"]["b"]["token"] = toml::Value::String("invalid-token".to_string());
    fs::write(peers_path, toml::to_string(&peers).unwrap()).unwrap();
    // Larger than the socket send buffer: rejection can arrive before write_all completes.
    let local = server.base.join("large.txt");
    fs::write(&local, vec![b'x'; 7 * 1024 * 1024]).unwrap();
    let local = local.to_str().unwrap();
    for args in [
        vec!["write", "b", "proj:large.txt", "--from", local],
        vec!["put", "b", local, "proj:large.txt"],
    ] {
        let output = server.cli(&args);
        assert_eq!(
            output.status.code(),
            Some(101),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("error[auth]"));
        assert!(!server.base.join("workspace/large.txt").exists());
    }
    fs::OpenOptions::new()
        .write(true)
        .open(local)
        .unwrap()
        .set_len(64 << 20)
        .unwrap();
    let started = Instant::now();
    let rejected = server.cli(&["put", "b", local, "proj:large.txt"]);
    assert_eq!(rejected.status.code(), Some(101));
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn frame_upload_finishes_promptly_on_success_and_early_rejection() {
    let server = Server::new();
    let ws = server.base.join("workspace");
    fs::write(ws.join("source.txt"), "payload").unwrap();
    let started = Instant::now();
    let copied = server.cli(&["cp", "b", "proj:source.txt", "b", "proj:copy.txt"]);
    assert!(
        copied.status.success(),
        "{}",
        String::from_utf8_lossy(&copied.stderr)
    );
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(fs::read_to_string(ws.join("copy.txt")).unwrap(), "payload");
    // A tiny stream can finish before the peer's rejection arrives. Both sides must still reach EOF.
    let started = Instant::now();
    let rejected = server.cli(&["cp", "b", "proj:source.txt", "b", "proj:readonly"]);
    assert_eq!(
        rejected.status.code(),
        Some(102),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(!ws.join("readonly/source.txt").exists());
    // A large stream should stop forwarding when the destination rejects it midway.
    fs::File::create(ws.join("big.bin"))
        .unwrap()
        .set_len(64 << 20)
        .unwrap();
    let started = Instant::now();
    let rejected = server.cli(&["cp", "b", "proj:big.bin", "b", "proj:readonly"]);
    assert_eq!(
        rejected.status.code(),
        Some(102),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(!ws.join("readonly/big.bin").exists());
}

#[test]
fn unauthenticated_connections_are_limited_before_tls() {
    let server = Server::new();
    let sockets: Vec<_> = (0..64)
        .map(|_| TcpStream::connect(&server.addr).unwrap())
        .collect();
    let mut excess = TcpStream::connect(&server.addr).unwrap();
    excess
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut byte = [0u8; 1];
    match excess.read(&mut byte) {
        Ok(0) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {}
        result => panic!("65th connection was admitted: {result:?}"),
    }
    drop(sockets);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if server.cli(&["info", "b"]).status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "connection permits were not released"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
