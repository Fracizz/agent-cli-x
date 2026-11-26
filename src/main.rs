mod cli;
mod client;
mod config;
mod decode;
mod desktop;
mod doctor;
mod error;
mod firewall;
mod frame;
mod http;
mod netif;
mod platform;
mod server;
mod tls;
mod tunnel;
mod util;
mod wsconfig;

/// clap 在调试构建下解析子命令很吃栈，Windows 主线程默认只有 1MB，因此换到大栈线程运行。
const STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() {
    let worker = std::thread::Builder::new()
        .stack_size(STACK_BYTES)
        .spawn(|| match cli::run() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                e.exit_code()
            }
        });
    // 线程内 panic 时沿用 Rust 默认的退出码 101
    let code = match worker {
        Ok(handle) => handle.join().unwrap_or(101),
        Err(e) => {
            eprintln!("error[internal]: cannot start main thread: {e}");
            108
        }
    };
    std::process::exit(code);
}
