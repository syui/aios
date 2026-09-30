// journalctl -u UNIT [-n N] [-f]: サービスのログ (/var/log/UNIT.log) を見る
#[path = "../lib/unit.rs"]
mod unit;

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

fn main() {
    // 読み手のいないパイプに書いたら、ほかのコマンドと同じように静かに終わる
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1));
    let follow = args.iter().any(|a| a == "-f");
    let lines: usize = get("-n").and_then(|n| n.parse().ok()).unwrap_or(usize::MAX);
    let Some(u) = get("-u") else {
        eprintln!("usage: journalctl -u UNIT [-n N] [-f]");
        std::process::exit(2);
    };
    let path = format!("/var/log/{}.log", unit::full_name(u));
    let text = fs::read_to_string(&path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    for l in &all[all.len().saturating_sub(lines)..] {
        println!("{}", l);
    }
    if !follow {
        return;
    }
    let mut pos = text.len() as u64;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Ok(mut f) = fs::File::open(&path) else { continue };
        let len = f.metadata().map_or(0, |m| m.len());
        if len < pos {
            pos = 0;
        }
        if len > pos {
            let mut buf = vec![];
            let _ = f.seek(SeekFrom::Start(pos)).and_then(|_| f.read_to_end(&mut buf));
            pos += buf.len() as u64;
            let _ = std::io::stdout().write_all(&buf);
        }
    }
}
