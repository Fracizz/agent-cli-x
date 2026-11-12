mod cli;
mod client;
mod config;
mod decode;
mod doctor;
mod error;
mod firewall;
mod frame;
mod http;
mod platform;
mod server;
mod tls;
mod util;

fn main() {
    let code = match cli::run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            e.exit_code()
        }
    };
    std::process::exit(code);
}
