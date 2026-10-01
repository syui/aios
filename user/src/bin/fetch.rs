// fetch URL [-o FILE]: URL の中身を取ってくる (http, https)
//   -o FILE なら、メモリにためずに FILE へ書く (途中は FILE.part。切れたら続きから)
#[path = "../lib/http.rs"]
mod http;
#[path = "../lib/meter.rs"]
mod meter;
#[path = "../lib/tls.rs"]
mod tls;

use std::io::Write;

fn main() {
    // 読み手のいないパイプに書いたら、ほかのコマンドと同じように静かに終わる
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(url) = args.first() else {
        eprintln!("usage: fetch URL [-o FILE]");
        std::process::exit(2);
    };
    if let Some(path) = args.iter().position(|a| a == "-o").and_then(|i| args.get(i + 1)) {
        let mut m = meter::Meter::new(path, 0);
        let r = http::download(url, Some(tls::connect), path, &mut |done, total| {
            m.set_total(total);
            m.update(done);
        });
        let n = std::fs::metadata(path).map_or(0, |m| m.len());
        m.finish(n);
        if let Err(e) = r {
            eprintln!("fetch: {}", e);
            std::process::exit(1);
        }
        return;
    }
    match http::get(url, Some(tls::connect)) {
        Ok(b) => {
            std::io::stdout().write_all(&b).ok();
        }
        Err(e) => {
            eprintln!("fetch: {}", e);
            std::process::exit(1);
        }
    }
}
