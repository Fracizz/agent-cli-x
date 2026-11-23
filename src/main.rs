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
mod util;
mod wsconfig;

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
