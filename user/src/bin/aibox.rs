// aibox: コマンドを砂場 (landlock) の中で動かす
//   aibox [-w PATH]... [-n PORT]... [--no-net] [-v] [--] CMD [ARG]...
//   読む・動かすのはどこでも。書く (作る・消す・名前を変える) のは、いまのディレクトリ、/tmp、/dev と -w の下だけ。
//   --no-net で TCP はどこへもつなげない。-n PORT でその口だけつなげる (--no-net がなくても、-n があればそれだけ)。
//   砂場は子にも引き継がれ、外せない。sudo (setuid) も効かなくなる
//   CMD がなければ $SHELL (なければ /bin/sh)。-w の ~ はホーム
//   /etc/claude-code/managed-mcp.json は aish --mcp をこれで起こす (Claude が動かすものはみな砂場の中)。
//   root のする操作は aios do (aiosd が wheel の人かを見てする) を通す
#[path = "../lib/landlock.rs"]
mod landlock;

use std::os::unix::process::CommandExt;

fn usage() -> ! {
    eprintln!("usage: aibox [-w PATH]... [-n PORT]... [--no-net] [-v] [--] CMD [ARG]...");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut write = landlock::default_write();
    let mut ports: Option<Vec<u16>> = None;
    let mut verbose = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-w" => {
                i += 1;
                let w = args.get(i).cloned().unwrap_or_else(|| usage());
                // ~ はホーム (JSON の設定から呼ばれても、シェルが広げないので)
                let w = match (w.strip_prefix('~'), std::env::var("HOME")) {
                    (Some(rest), Ok(h)) if rest.is_empty() || rest.starts_with('/') => format!("{}{}", h, rest),
                    _ => w,
                };
                write.push(w);
            }
            "-n" => {
                i += 1;
                let p = args.get(i).and_then(|p| p.parse().ok()).unwrap_or_else(|| usage());
                ports.get_or_insert_with(Vec::new).push(p);
            }
            "--no-net" => {
                ports.get_or_insert_with(Vec::new);
            }
            "-v" => verbose = true,
            "-h" | "--help" => usage(),
            "--" => {
                i += 1;
                break;
            }
            a if a.starts_with('-') => usage(),
            _ => break,
        }
        i += 1;
    }
    let cmd: Vec<String> = if i < args.len() { args[i..].to_vec() } else { vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())] };
    match landlock::restrict(&write, ports.as_deref()) {
        Ok(missing) => {
            for m in &missing {
                eprintln!("aibox: {}: not found (not writable)", m);
            }
            if verbose {
                let net = match &ports {
                    None => "any".to_string(),
                    Some(p) if p.is_empty() => "none".to_string(),
                    Some(p) => p.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","),
                };
                eprintln!("aibox: write {} / tcp {}", write.iter().filter(|w| !missing.contains(w)).cloned().collect::<Vec<_>>().join(" "), net);
            }
        }
        Err(e) => {
            eprintln!("aibox: {}", e);
            std::process::exit(1);
        }
    }
    // 中のプログラムが「どこに書けるか」を知れるように (aish --mcp が Permission denied のときに教える)
    let writable: Vec<String> = write.iter().filter(|w| std::path::Path::new(w).exists()).cloned().collect();
    let e = std::process::Command::new(&cmd[0]).args(&cmd[1..]).env("AIBOX_WRITE", writable.join(":")).exec();
    eprintln!("aibox: {}: {}", cmd[0], e);
    std::process::exit(127);
}
