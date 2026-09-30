// fetch URL [-o FILE]: URL の中身を取ってくる (http, https)
#[path = "../lib/http.rs"]
mod http;
#[path = "../lib/tls.rs"]
mod tls;

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(url) = args.first() else {
        eprintln!("usage: fetch URL [-o FILE]");
        std::process::exit(2);
    };
    let body = match http::get(url, Some(tls::connect)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fetch: {}", e);
            std::process::exit(1);
        }
    };
    match args.iter().position(|a| a == "-o").and_then(|i| args.get(i + 1)) {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &body) {
                eprintln!("fetch: {}: {}", path, e);
                std::process::exit(1);
            }
            eprintln!("{} bytes -> {}", body.len(), path);
        }
        None => {
            std::io::stdout().write_all(&body).ok();
        }
    }
}
