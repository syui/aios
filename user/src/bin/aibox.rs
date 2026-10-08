// aibox: コマンドを砂場 (landlock と seccomp) の中で動かす
//   aibox [-w PATH]... [-n PORT]... [--no-net] [--deny SYSCALL,...] [--kill SYSCALL,...] [-v] [--] CMD [ARG]...
//   読む・動かすのはどこでも。書く (作る・消す・名前を変える) のは、いまのディレクトリ、/tmp、/dev と -w の下だけ。
//   --no-net で TCP はどこへもつなげない。-n PORT でその口だけつなげる (--no-net がなくても、-n があればそれだけ)。
//   システムコール: ptrace、mount、モジュールの読みこみ、reboot など、カーネルの深いところにさわるもの
//   (seccomp.rs の DEFAULT_DENY) は EPERM。--deny で足し (名前か番号)、--kill のものは呼んだら止める
//   砂場は子にも引き継がれ、外せない。sudo (setuid) も効かなくなる
//   CMD がなければ $SHELL (なければ /bin/sh)。-w の ~ はホーム
//   /etc/claude-code/managed-mcp.json は aish --mcp をこれで起こす (Claude が動かすものはみな砂場の中)。
//   root のする操作は aios do (aiosd が wheel の人かを見てする) を通す
#[path = "../lib/landlock.rs"]
mod landlock;
#[path = "../lib/seccomp.rs"]
mod seccomp;

use std::os::unix::process::CommandExt;

fn usage() -> ! {
    eprintln!("usage: aibox [-w PATH]... [-n PORT]... [--no-net] [--deny SYSCALL,...] [--kill SYSCALL,...] [-v] [--] CMD [ARG]...");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut write = landlock::default_write();
    let mut ports: Option<Vec<u16>> = None;
    let mut verbose = false;
    let mut deny: Vec<u32> = seccomp::DEFAULT_DENY.iter().filter_map(|n| seccomp::number(n)).collect();
    let mut kill: Vec<u32> = vec![];
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
            "--deny" | "--kill" => {
                let opt = args[i].clone();
                i += 1;
                for n in args.get(i).unwrap_or_else(|| usage()).split(',').filter(|n| !n.is_empty()) {
                    let Some(v) = seccomp::number(n) else {
                        eprintln!("aibox: {}: unknown system call", n);
                        std::process::exit(2);
                    };
                    if opt == "--deny" { deny.push(v) } else { kill.push(v) }
                }
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
    // システムコールをしぼる (landlock が no_new_privs をつけたあとで)
    if let Err(e) = seccomp::restrict(&deny, &kill) {
        eprintln!("aibox: {}", e);
        std::process::exit(1);
    }
    if verbose {
        eprintln!("aibox: seccomp deny {} kill {}", deny.len(), kill.len());
    }
    // 中のプログラムが「どこに書けるか」を知れるように (aish --mcp が Permission denied のときに教える)
    let writable: Vec<String> = write.iter().filter(|w| std::path::Path::new(w).exists()).cloned().collect();
    let e = std::process::Command::new(&cmd[0]).args(&cmd[1..]).env("AIBOX_WRITE", writable.join(":")).exec();
    eprintln!("aibox: {}: {}", cmd[0], e);
    std::process::exit(127);
}
