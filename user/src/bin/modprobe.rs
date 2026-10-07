// modprobe NAME... / modprobe -r NAME... / modprobe -a (/etc/modules-load.d のもの) / modprobe -l (使えるもの)
// lsmod, rmmod NAME, insmod FILE も同じもの (呼ばれた名前で)
#[path = "../lib/kmod.rs"]
mod kmod;

use std::process::exit;

fn main() {
    // 読み手のいないパイプに書いたら (| head など)、ほかのコマンドと同じように静かに終わる
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().collect();
    let me = args[0].rsplit('/').next().unwrap_or("modprobe").to_string();
    let rest = &args[1..];
    let mut rc = 0;
    let fail = |name: &str, e: std::io::Error| {
        eprintln!("{}: {}: {}", me, name, e);
    };
    match me.as_str() {
        "lsmod" => {
            println!("Module                  Size  Used by");
            for m in kmod::loaded() {
                println!("{:<24}{:>4}  0", m, 0);
            }
        }
        "rmmod" => {
            for n in rest {
                if let Err(e) = kmod::unload(n) {
                    fail(n, e);
                    rc = 1;
                }
            }
        }
        "insmod" => {
            // insmod FILE: 札のファイルを直に
            for f in rest {
                let name = std::path::Path::new(f).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if let Err(e) = kmod::load(&name) {
                    fail(f, e);
                    rc = 1;
                }
            }
        }
        _ => {
            let (opt, names): (Vec<&String>, Vec<&String>) = rest.iter().partition(|a| a.starts_with('-'));
            let has = |o: &str| opt.iter().any(|a| *a == o);
            if has("-l") || has("--list") {
                for e in std::fs::read_dir(kmod::DIR).into_iter().flatten().flatten() {
                    let p = e.path();
                    if p.extension().is_some_and(|x| x == "ko") {
                        println!("{}", p.file_stem().unwrap().to_string_lossy());
                    }
                }
                exit(0);
            }
            let list: Vec<String> = if has("-a") && names.is_empty() { kmod::boot_list() } else { names.iter().map(|s| s.to_string()).collect() };
            if list.is_empty() {
                eprintln!("usage: modprobe [-r] NAME... | modprobe -a | modprobe -l");
                exit(1);
            }
            for n in &list {
                let r = if has("-r") { kmod::unload(n) } else { kmod::load(n) };
                if let Err(e) = r {
                    fail(n, e);
                    rc = 1;
                }
            }
        }
    }
    exit(rc);
}
