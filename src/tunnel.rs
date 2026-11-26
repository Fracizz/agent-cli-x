//! TLS 连接与明文 TCP 之间的双向转发，`proxy` 的两端共用。
//!
//! rustls 的连接状态不能拆成读写两半，因此两个线程共享一把锁，且只在加解密时持有；
//! 套接字读写都在锁外进行。
//!
//! 读取前先带超时地等套接字可读：Windows 上 `shutdown` 唤不醒阻塞中的 `recv`，
//! 超时与中止只能由读线程自己发现。

use rustls::Connection;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

const CHUNK: usize = 32 * 1024;

/// 超时设置：`idle` 为两个方向都没有数据的上限，`linger` 为一个方向结束后另一方向的静默上限。
#[derive(Clone, Copy)]
pub struct Limits {
    pub idle: Duration,
    pub linger: Duration,
}

pub const LIMITS: Limits = Limits {
    idle: Duration::from_secs(300),
    linger: Duration::from_secs(60),
};

/// 已转发的应用数据字节数：`sent` 写入 TLS 一侧，`received` 来自 TLS 一侧。
#[derive(Default)]
pub struct Counters {
    pub sent: AtomicU64,
    pub received: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 等待套接字可读（含对端关闭与出错），超时返回 false。
#[cfg(windows)]
fn readable(s: &TcpStream, wait: Duration) -> io::Result<bool> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{WSAPoll, POLLRDNORM, SOCKET, WSAPOLLFD};
    let mut fd = WSAPOLLFD {
        fd: s.as_raw_socket() as SOCKET,
        events: POLLRDNORM,
        revents: 0,
    };
    // SAFETY: 传入一个有效的 WSAPOLLFD，套接字在调用期间由 `s` 持有。
    match unsafe { WSAPoll(&mut fd, 1, wait.as_millis() as i32) } {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(unix)]
fn readable(s: &TcpStream, wait: Duration) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut fd = libc::pollfd {
        fd: s.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: 传入一个有效的 pollfd，描述符在调用期间由 `s` 持有。
    match unsafe { libc::poll(&mut fd, 1, wait.as_millis() as libc::c_int) } {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        _ => match io::Error::last_os_error() {
            e if e.kind() == ErrorKind::Interrupted => Ok(false),
            e => Err(e),
        },
    }
}

struct Pipe<'a> {
    conn: Mutex<Connection>,
    tls: &'a TcpStream,
    plain: &'a TcpStream,
    /// 写 TLS 套接字的唯一入口：保证记录按序列号顺序发出。
    out: Mutex<&'a TcpStream>,
    limits: Limits,
    active: Mutex<Instant>,
    /// 已有一个方向正常结束。
    half_closed: AtomicBool,
    /// 首个错误；出现后两个方向都停止。
    failure: Mutex<Option<io::Error>>,
    stop: AtomicBool,
}

impl Pipe<'_> {
    fn touch(&self) {
        *lock(&self.active) = Instant::now();
    }

    /// 阻塞到可读；另一方向出错或静默超时则返回错误。
    fn wait(&self, s: &TcpStream) -> io::Result<()> {
        let tick = (self.limits.idle.min(self.limits.linger) / 4).min(Duration::from_millis(500));
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Err(ErrorKind::ConnectionAborted.into());
            }
            if readable(s, tick)? {
                return Ok(());
            }
            let limit = if self.half_closed.load(Ordering::SeqCst) {
                self.limits.linger
            } else {
                self.limits.idle
            };
            if lock(&self.active).elapsed() >= limit {
                return Err(ErrorKind::TimedOut.into());
            }
        }
    }

    fn finish(&self, result: io::Result<()>) {
        match result {
            Ok(()) => {
                self.touch();
                self.half_closed.store(true, Ordering::SeqCst);
            }
            Err(e) => {
                lock(&self.failure).get_or_insert(e);
                self.abort();
            }
        }
    }

    fn abort(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tls.shutdown(Shutdown::Both);
        let _ = self.plain.shutdown(Shutdown::Both);
    }

    /// 加密并发送；`data` 为空时只冲刷连接里排队的记录（如 close_notify）。
    fn send(&self, data: &[u8]) -> io::Result<()> {
        let mut out = lock(&self.out);
        let mut rest = data;
        let mut records = Vec::new();
        loop {
            records.clear();
            {
                let mut conn = lock(&self.conn);
                if !rest.is_empty() {
                    let n = conn.writer().write(rest)?;
                    rest = &rest[n..];
                }
                while conn.wants_write() {
                    conn.write_tls(&mut records)?;
                }
            }
            out.write_all(&records)?;
            self.touch();
            if rest.is_empty() {
                return Ok(());
            }
        }
    }

    /// 接收方向偶尔需要回发记录（密钥更新等）。发送线程占用时直接跳过，由它顺带发出，避免互相等待。
    fn flush_pending(&self) -> io::Result<()> {
        let Ok(mut out) = self.out.try_lock() else {
            return Ok(());
        };
        let mut records = Vec::new();
        {
            let mut conn = lock(&self.conn);
            while conn.wants_write() {
                conn.write_tls(&mut records)?;
            }
        }
        out.write_all(&records)
    }

    fn plain_to_tls(&self, pending: &[u8], count: &AtomicU64) -> io::Result<()> {
        if !pending.is_empty() {
            self.send(pending)?;
            count.fetch_add(pending.len() as u64, Ordering::Relaxed);
        }
        let mut plain = self.plain;
        let mut buf = vec![0u8; CHUNK];
        loop {
            self.wait(plain)?;
            let n = match plain.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            self.send(&buf[..n])?;
            count.fetch_add(n as u64, Ordering::Relaxed);
        }
        lock(&self.conn).send_close_notify();
        self.send(&[])
    }

    fn tls_to_plain(&self, early: &[u8], count: &AtomicU64) -> io::Result<()> {
        let (mut tls, mut plain) = (self.tls, self.plain);
        let mut text = early.to_vec();
        // 握手或响应头阶段可能已经解密出一部分数据
        let mut closed = drain(&mut lock(&self.conn), &mut text)?;
        let mut raw = vec![0u8; CHUNK];
        loop {
            if !text.is_empty() {
                plain.write_all(&text)?;
                count.fetch_add(text.len() as u64, Ordering::Relaxed);
                self.touch();
                text.clear();
            }
            if closed {
                break;
            }
            self.wait(tls)?;
            let n = match tls.read(&mut raw) {
                Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            {
                let mut conn = lock(&self.conn);
                let mut rest = &raw[..n];
                while !rest.is_empty() && !closed {
                    conn.read_tls(&mut rest)?;
                    conn.process_new_packets()
                        .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
                    closed = drain(&mut conn, &mut text)?;
                }
            }
            self.flush_pending()?;
        }
        let _ = plain.shutdown(Shutdown::Write);
        Ok(())
    }
}

/// 取出已解密的数据；返回 true 表示对端已正常结束发送（close_notify）。
fn drain(conn: &mut Connection, out: &mut Vec<u8>) -> io::Result<bool> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match conn.reader().read(&mut buf) {
            Ok(0) => return Ok(true),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
            Err(e) => return Err(e),
        }
    }
}

/// 双向转发直到两个方向都结束、出错或超时。
/// `to_tls` / `to_plain` 是进入转发前已经读出、需要先送往对应一侧的数据。
pub fn pump(
    conn: Connection,
    tls: &TcpStream,
    plain: &TcpStream,
    to_tls: &[u8],
    to_plain: &[u8],
    limits: Limits,
    counters: &Counters,
) -> io::Result<()> {
    for s in [tls, plain] {
        s.set_read_timeout(None)?;
        // 对方长时间不收数据时，让阻塞中的写入报错退出
        s.set_write_timeout(Some(limits.idle))?;
    }
    let pipe = Pipe {
        conn: Mutex::new(conn),
        tls,
        plain,
        out: Mutex::new(tls),
        limits,
        active: Mutex::new(Instant::now()),
        half_closed: AtomicBool::new(false),
        failure: Mutex::new(None),
        stop: AtomicBool::new(false),
    };
    thread::scope(|scope| {
        let pipe = &pipe;
        let up = thread::Builder::new().spawn_scoped(scope, move || {
            pipe.finish(pipe.plain_to_tls(to_tls, &counters.sent));
        });
        match up {
            Ok(_) => pipe.finish(pipe.tls_to_plain(to_plain, &counters.received)),
            Err(e) => pipe.finish(Err(e)),
        }
    });
    let failure = lock(&pipe.failure).take();
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls;
    use rustls::{ClientConnection, ServerConnection};
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        (a, l.accept().unwrap().0)
    }

    /// 两端都静默时必须靠超时结束：验证关闭套接字能唤醒阻塞中的读取（Windows 与 Linux）。
    #[test]
    fn silent_tunnel_is_closed_after_idle() {
        let home = std::env::temp_dir().join(format!("acx-tunnel-{}", crate::util::random_hex(6)));
        tls::generate(&home).unwrap();
        let (server_cfg, _) = tls::server_config(&home).unwrap();
        let (client_cfg, _) = tls::client_config(None).unwrap();
        let (mut client_tls, mut server_tls) = pair();
        let mut server = ServerConnection::new(server_cfg).unwrap();
        let accept = thread::spawn(move || {
            while server.is_handshaking() {
                server.complete_io(&mut server_tls).unwrap();
            }
            server_tls
        });
        let name = "agent-cli-x".try_into().unwrap();
        let mut client = ClientConnection::new(client_cfg, name).unwrap();
        while client.is_handshaking() {
            client.complete_io(&mut client_tls).unwrap();
        }
        let _peer = accept.join().unwrap();
        let (plain, _app) = pair();
        let limits = Limits {
            idle: Duration::from_millis(300),
            linger: Duration::from_millis(300),
        };
        let started = Instant::now();
        let e = pump(
            client.into(),
            &client_tls,
            &plain,
            b"",
            b"",
            limits,
            &Counters::default(),
        )
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(home);
    }
}
