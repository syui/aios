// aios: aios を把握・設定・操作するコマンド (doc/aios.md)
//   aios                       様子をロゴといっしょに出す (neofetch のようなもの)。あとに /etc/motd も。
//                              起動のときは motd.service が動かす
//   aios get [PATH] [--json]   状態の木 (host kernel mem disk proc service pkg net user boot)。
//                              PATH は点でつなぐ (kernel.cpus、service.sshd.active)。ふだんは PATH = 値 の行
//   aios do OP ...  [--json]   aiosd (root で動く) に頼んで変える。root と wheel の人だけ:
//                                service start|stop|restart|enable|disable NAME
//                                pkg install|remove NAME... / pkg upgrade / pkg refresh
//                                reboot / poweroff / ping
//   aios diff                  /etc/aios.json (望む状態) といまのちがいと、そろえる手順 (読むだけ)
//   aios apply                 /etc/aios.json のとおりにそろえる (aiosd に頼む)
//   aios rollback              ひとつ前の apply の設定に戻す (aiosd に頼む)
//   aios history [N]           apply の記録
//   aios config                いまの状態を aios.json の形で (はじめて作るとき: aios config | sudo tee /etc/aios.json)
//   aios src                   aios のソースを /usr/src/aios に (なければ git clone、あれば git pull。wheel の人)
//   aios build kernel          /usr/src/aios のカーネルをビルドして target/Image に (起動できる形)
//   aios install kernel [IMAGE] | --revert
//                              /boot/Image を入れかえる (aiosd に頼む)。前のものは起動の一覧の「previous kernel」
//   aios build pkg NAME|DIR [-o DIR]
//                              PKGBUILD からパッケージを作る (NAME は /usr/src/aios/pkg/*/NAME)。できたものは
//                              いまのディレクトリ (-o で変える) に。作業は ~/.cache/aios/build/NAME
//   aios install pkg FILE...   作ったパッケージを入れる (aiosd が aipkg -U で)
#[path = "../lib/config.rs"]
mod config;
#[path = "../lib/image.rs"]
mod image;
#[path = "../lib/mkpkg.rs"]
mod mkpkg;
#[path = "../lib/netif.rs"]
#[allow(dead_code)]
mod netif;
#[path = "../lib/state.rs"]
mod state;
#[path = "../lib/unit.rs"]
mod unit;

use std::fs;

const LOGO: &str = "\
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢠⡄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢠⣿⣿⡄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢠⣿⣿⣿⣿⡄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⣤⣿⣿⣿⣿⣿⣿⣤⣀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⠀⣠⣾⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣷⣄⠀⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⠀⣼⣿⣿⣿⠟⠉⠀⠀⠀⠀⠉⠻⣿⣿⣿⣧⠀⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⢸⣿⣿⣿⠃⠀⠀⠀⠀⠀⠀⠀⠀⠘⣿⣿⣿⡇⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⠀⢸⣿⣿⣿⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⣿⡇⠀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⠀⢀⣾⣿⣿⣿⡄⠀⠀⠀⠀⠀⠀⠀⠀⢠⣿⣿⣿⣷⡀⠀⠀⠀⠀⠀
⠀⠀⠀⠀⣠⣿⣿⣿⣿⣿⣿⣦⣀⠀⠀⠀⠀⣀⣴⣿⣿⣿⣿⣿⣿⣄⠀⠀⠀⠀
⠀⠀⢀⣼⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣧⡀⠀⠀
⠀⠀⠈⠁⠀⠀⠀⠀⠀⠀⠉⠛⠿⠿⠿⠿⠿⠿⠛⠉⠀⠀⠀⠀⠀⠀⠈⠁⠀⠀";

const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1;33m";
const RESET: &str = "\x1b[0m";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => info(),
        Some("get") => get(&args[1..]),
        Some("do") => do_(&args[1..]),
        Some("diff") => diff(),
        Some("apply") => send(serde_json::json!({ "op": "apply" }), args.iter().any(|a| a == "--json")),
        Some("rollback") => send(serde_json::json!({ "op": "rollback" }), args.iter().any(|a| a == "--json")),
        Some("history") => history(args.get(1)),
        Some("src") => src(),
        Some("build") if args.get(1).map(String::as_str) == Some("kernel") => build_kernel(),
        Some("install") if args.get(1).map(String::as_str) == Some("kernel") => install_kernel(args.get(2)),
        Some("build") if args.get(1).map(String::as_str) == Some("pkg") => build_pkg(&args[2..]),
        Some("install") if args.get(1).map(String::as_str) == Some("pkg") => install_pkg(&args[2..]),
        Some("config") => println!("{}", serde_json::to_string_pretty(&config::export()).unwrap_or_default()),
        Some("-h" | "--help" | "help") => usage(0),
        Some(c) => {
            eprintln!("aios: unknown command {}", c);
            usage(2)
        }
    }
}

fn usage(code: i32) -> ! {
    eprintln!("usage: aios                       様子 (ロゴつき)");
    eprintln!("       aios get [PATH] [--json]   状態の木 ({})", state::ROOTS.join(" "));
    eprintln!("       aios do service start|stop|restart|enable|disable NAME");
    eprintln!("       aios do pkg install|remove NAME... | pkg upgrade | pkg refresh");
    eprintln!("       aios do reboot | poweroff | ping   (aiosd に頼む。root と wheel の人だけ)");
    eprintln!("       aios diff | apply | rollback | history [N] | config   (/etc/aios.json)");
    eprintln!("       aios src | build kernel | install kernel [IMAGE] | install kernel --revert   (改造)");
    eprintln!("       aios build pkg NAME|DIR [-o DIR] | install pkg FILE...   (パッケージ)");
    std::process::exit(code)
}

/// aios get [PATH] [--json]
fn get(args: &[String]) {
    let as_json = args.iter().any(|a| a == "--json");
    let path = args.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("");
    let keys: Vec<&str> = path.split('.').filter(|k| !k.is_empty()).collect();
    // 一番上だけ集める (proc や service を見ないときは、それを集めない)
    let tree = match keys.first() {
        None => state::collect_all(),
        Some(r) => match state::collect(r) {
            Some(v) => serde_json::json!({ *r: v }),
            None => {
                eprintln!("aios get: {}: not found (one of: {})", r, state::ROOTS.join(" "));
                std::process::exit(1);
            }
        },
    };
    let Some(v) = state::select(&tree, &keys) else {
        eprintln!("aios get: {}: not found", path);
        std::process::exit(1);
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else if v.is_object() || v.is_array() {
        let mut lines = Vec::new();
        state::flatten(path, v, &mut lines);
        for l in lines {
            println!("{}", l);
        }
    } else {
        println!("{}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
    }
}

/// aios do: aiosd に 1 つ頼んで、答えを出す
fn do_(args: &[String]) {
    let as_json = args.iter().any(|a| a == "--json");
    let w: Vec<&str> = args.iter().filter(|a| !a.starts_with("--")).map(String::as_str).collect();
    let req = match w.as_slice() {
        ["service", action, name] => serde_json::json!({ "op": "service", "action": action, "name": name }),
        ["pkg", action, names @ ..] => serde_json::json!({ "op": "pkg", "action": action, "names": names }),
        [p @ ("reboot" | "poweroff")] => serde_json::json!({ "op": "power", "action": p }),
        ["ping"] => serde_json::json!({ "op": "ping" }),
        _ => usage(2),
    };
    let (line, r) = ask(&req);
    if as_json {
        println!("{}", line.trim_end());
    } else {
        if let Some(o) = r["out"].as_str() {
            print!("{}", o);
        }
        if let Some(e) = r["err"].as_str() {
            eprint!("{}{}", e, if e.ends_with('\n') || e.is_empty() { "" } else { "\n" });
        }
        if req["op"] == "ping" {
            println!("aiosd {} (uid {}, {})", r["version"].as_str().unwrap_or("?"), r["uid"], if r["allowed"] == true { "can change" } else { "read only" });
        }
    }
    std::process::exit(if r["ok"] == true { 0 } else { r["status"].as_i64().filter(|s| *s > 0).unwrap_or(1) as i32 });
}

/// aiosd に 1 つ頼む (1 行の JSON を送り、1 行の答え)
fn ask(req: &serde_json::Value) -> (String, serde_json::Value) {
    use std::io::{BufRead, BufReader, Write};
    let mut c = match std::os::unix::net::UnixStream::connect("/run/aiosd.sock") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("aios: /run/aiosd.sock: {} (sudo systemctl enable --now aiosd)", e);
            std::process::exit(1);
        }
    };
    let mut line = String::new();
    if writeln!(c, "{}", req).is_err() || BufReader::new(&c).read_line(&mut line).is_err() || line.is_empty() {
        eprintln!("aios: aiosd did not answer");
        std::process::exit(1);
    }
    let r = serde_json::from_str(&line).unwrap_or_default();
    (line, r)
}

/// 手順を 1 行ずつ
fn show_steps(steps: &[serde_json::Value]) {
    for s in steps {
        let mark = match s["ok"].as_bool() {
            Some(true) => "ok   ",
            Some(false) => "FAIL ",
            None => "",
        };
        println!("{}{}: {} -> {}   ({})", mark, s["what"].as_str().unwrap_or(""), s["from"].as_str().unwrap_or(""), s["to"].as_str().unwrap_or(""), s["do"].as_str().unwrap_or(""));
        if s["ok"] == false
            && let Some(o) = s["out"].as_str()
        {
            for l in o.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev() {
                println!("       {}", l);
            }
        }
    }
}

/// apply / rollback を aiosd に頼む
fn send(req: serde_json::Value, as_json: bool) {
    let (line, r) = ask(&req);
    if as_json {
        println!("{}", line.trim_end());
    } else if let Some(e) = r["err"].as_str() {
        eprintln!("aios: {}", e);
    } else {
        let steps = r["steps"].as_array().cloned().unwrap_or_default();
        if steps.is_empty() {
            println!("aios: nothing to do (already as /etc/aios.json says)");
        }
        show_steps(&steps);
        println!("(record {}: aios history {})", r["n"], r["n"]);
    }
    std::process::exit(if r["ok"] == true { 0 } else { 1 });
}

/// aios diff: そろえる手順 (動かさない)
fn diff() {
    let cfg = match config::load(config::PATH) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("aios diff: {} (aios config | sudo tee /etc/aios.json で今の状態から作れる)", e);
            std::process::exit(1);
        }
    };
    match config::plan(&cfg) {
        Ok(steps) if steps.is_empty() => println!("aios: no difference"),
        Ok(steps) => show_steps(&steps.iter().map(|s| s.json()).collect::<Vec<_>>()),
        Err(e) => {
            eprintln!("aios diff: {}", e);
            std::process::exit(1);
        }
    }
}

/// aios history [N]
fn history(n: Option<&String>) {
    if let Some(n) = n.and_then(|n| n.parse().ok()) {
        match config::read_history(n) {
            Some(r) => println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default()),
            None => {
                eprintln!("aios history: no record {}", n);
                std::process::exit(1);
            }
        }
        return;
    }
    for n in config::history() {
        let Some(r) = config::read_history(n) else { continue };
        let steps = r["steps"].as_array().map_or(0, |a| a.len());
        let bad = r["steps"].as_array().map_or(0, |a| a.iter().filter(|s| s["ok"] == false).count());
        let what = match r["rollback_of"].as_u64() {
            Some(l) => format!("rollback of {}", l),
            None => "apply".into(),
        };
        println!("{:>4}  t={}  uid={}  {}  {} steps{}", n, r["t"], r["uid"], what, steps, if bad > 0 { format!(", {} failed", bad) } else { String::new() });
    }
}

// ---- 改造 ----

const SRC: &str = "/usr/src/aios";
const REPO: &str = "https://git.syui.ai/ai/os";

fn src_dir() -> String {
    std::env::var("AIOS_SRC").unwrap_or_else(|_| SRC.to_string())
}

fn sh(cmd: &mut std::process::Command) -> bool {
    cmd.status().is_ok_and(|s| s.success())
}

/// aios src: なければ git clone (場所は aiosd に作ってもらう)、あれば git pull
fn src() {
    let dir = src_dir();
    if std::path::Path::new(&dir).join(".git").exists() {
        if !sh(std::process::Command::new("git").args(["-C", &dir, "pull", "--ff-only"])) {
            std::process::exit(1);
        }
    } else {
        if dir == SRC && !std::path::Path::new(SRC).exists() {
            let (_, r) = ask(&serde_json::json!({ "op": "src" }));
            if r["ok"] != true {
                eprintln!("aios src: {}", r["err"].as_str().unwrap_or("aiosd failed"));
                std::process::exit(1);
            }
        }
        if !sh(std::process::Command::new("git").args(["clone", "-b", "unix", REPO, &dir])) {
            std::process::exit(1);
        }
    }
    let _ = sh(std::process::Command::new("git").args(["-C", &dir, "log", "--oneline", "-1"]));
}

/// aios build kernel: AIOS_INITRD=none でビルドして、ELF から Image を作る
fn build_kernel() {
    let dir = src_dir();
    let rev = std::process::Command::new("git").args(["-C", &dir, "rev-parse", "--short", "HEAD"]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let ok = sh(std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "aios"])
        .current_dir(format!("{}/kernel", dir))
        .env("AIOS_INITRD", "none")
        .env("AIOS_RELEASE", format!("src-{}", if rev.is_empty() { "dirty" } else { &rev })));
    if !ok {
        eprintln!("aios build kernel: cargo build failed");
        std::process::exit(1);
    }
    let elf = format!("{}/target/aarch64-unknown-none-softfloat/release/aios", dir);
    let out = format!("{}/target/Image", dir);
    let img = std::fs::read(&elf).map_err(|e| format!("{}: {}", elf, e)).and_then(|e| image::elf_to_image(&e));
    match img.and_then(|i| std::fs::write(&out, &i).map(|_| i.len()).map_err(|e| format!("{}: {}", out, e))) {
        Ok(n) => println!("{} ({} bytes, src-{}). next: aios install kernel", out, n, rev),
        Err(e) => {
            eprintln!("aios build kernel: {}", e);
            std::process::exit(1);
        }
    }
}

/// aios install kernel [IMAGE] | --revert
fn install_kernel(arg: Option<&String>) {
    let req = match arg.map(String::as_str) {
        Some("--revert") => serde_json::json!({ "op": "kernel", "action": "revert" }),
        p => {
            let path = p.map(String::from).unwrap_or_else(|| format!("{}/target/Image", src_dir()));
            let abs = std::fs::canonicalize(&path).map(|p| p.to_string_lossy().into_owned()).unwrap_or(path);
            serde_json::json!({ "op": "kernel", "action": "install", "path": abs })
        }
    };
    let (_, r) = ask(&req);
    match r["err"].as_str() {
        Some(e) => {
            eprintln!("aios install kernel: {}", e);
            std::process::exit(1);
        }
        None => print!("{}", r["out"].as_str().unwrap_or("")),
    }
}

/// aios build pkg NAME|DIR [-o DIR]: PKGBUILD からパッケージを作る
fn build_pkg(args: &[String]) {
    let mut dest = std::path::PathBuf::from(".");
    let mut what = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => dest = it.next().map(Into::into).unwrap_or_else(|| usage(2)),
            _ => what = Some(a.clone()),
        }
    }
    let Some(what) = what else { usage(2) };
    // DIR (PKGBUILD がある) か、/usr/src/aios/pkg/*/NAME
    let dir = if std::path::Path::new(&what).join("PKGBUILD").exists() {
        std::path::PathBuf::from(&what)
    } else {
        let found = std::fs::read_dir(format!("{}/pkg", src_dir()))
            .into_iter()
            .flatten()
            .flatten()
            .map(|k| k.path().join(&what))
            .find(|d| d.join("PKGBUILD").exists());
        match found {
            Some(d) => d,
            None => {
                eprintln!("aios build pkg: {}: no PKGBUILD there or in {}/pkg/*/ (aios src)", what, src_dir());
                std::process::exit(1);
            }
        }
    };
    match mkpkg::build(&dir, &dest) {
        Ok(f) => {
            let f = std::fs::canonicalize(&f).unwrap_or(f);
            println!("{}. next: aios install pkg {}", f.display(), f.display());
        }
        Err(e) => {
            eprintln!("aios build pkg: {}", e);
            std::process::exit(1);
        }
    }
}

/// aios install pkg FILE...: aiosd が aipkg -U で入れる
fn install_pkg(files: &[String]) {
    if files.is_empty() {
        usage(2);
    }
    let paths: Vec<String> = files.iter().map(|f| std::fs::canonicalize(f).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| f.clone())).collect();
    let (_, r) = ask(&serde_json::json!({ "op": "pkg", "action": "file", "paths": paths }));
    print!("{}", r["out"].as_str().unwrap_or(""));
    if let Some(e) = r["err"].as_str().filter(|e| !e.is_empty()) {
        eprint!("{}", e);
    }
    if r["ok"] != true {
        std::process::exit(1);
    }
}

fn info() {
    let host = fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| uname().1);
    let uid = unsafe { libc::getuid() };
    let user = passwd_field(uid, 0).unwrap_or_default();
    let title = if uid == 0 || user.is_empty() { host.clone() } else { format!("{}@{}", user, host) };
    let mut info: Vec<(String, String)> = Vec::new();
    let mut add = |k: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            info.push((k.to_string(), v));
        }
    };
    add("OS", Some(format!("aios (unix) {}", pkg_version("base").unwrap_or_default()).trim().to_string()));
    add("Kernel", Some(uname().0));
    add("Uptime", uptime());
    add("Packages", packages());
    add("Shell", shell(uid));
    add("Init", Some("aios init".into()));
    add("CPU", cpu());
    add("Memory", memory());
    add("Swap", swap());
    add("Disk (/)", disk("/"));
    add("IP", ip());

    let logo: Vec<&str> = LOGO.lines().collect();
    let width = logo.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let mut right = vec![format!("{}{}{}", BOLD, title, RESET), "-".repeat(title.chars().count())];
    for (k, v) in &info {
        right.push(format!("{}{}{}: {}", BOLD, k, RESET, v));
    }
    println!();
    for i in 0..logo.len().max(right.len()) {
        let l = logo.get(i).copied().unwrap_or("");
        let pad = width - l.chars().count();
        println!("{}{}{}{}   {}", YELLOW, l, RESET, " ".repeat(pad), right.get(i).map_or("", |s| s.as_str()));
    }
    println!();
    // ひとこと (/etc/motd)
    if let Ok(m) = fs::read_to_string("/etc/motd") {
        print!("{}", m);
    }
}

/// (release, nodename)
fn uname() -> (String, String) {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u) };
    let s = |f: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }.to_string_lossy().into_owned();
    (s(&u.release), s(&u.nodename))
}

/// /etc/passwd の uid の行の n 番目 (0 は名前、6 はシェル)
fn passwd_field(uid: u32, n: usize) -> Option<String> {
    let p = fs::read_to_string("/etc/passwd").ok()?;
    p.lines().map(|l| l.split(':').collect::<Vec<_>>()).find(|f| f.len() > 6 && f[2].parse() == Ok(uid)).map(|f| f[n].to_string())
}

/// aipkg が入れたパッケージの版 (/var/lib/aipkg/local/NAME-VER)
fn pkg_version(name: &str) -> Option<String> {
    fs::read_dir("/var/lib/aipkg/local").ok()?.flatten().find_map(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        let v = n.strip_prefix(name)?.strip_prefix('-')?;
        v.starts_with(|c: char| c.is_ascii_digit()).then(|| v.to_string())
    })
}

fn packages() -> Option<String> {
    let n = fs::read_dir("/var/lib/aipkg/local").ok()?.flatten().filter(|e| e.path().is_dir()).count();
    Some(format!("{} (aipkg)", n))
}

fn uptime() -> Option<String> {
    let s: f64 = fs::read_to_string("/proc/uptime").ok()?.split_whitespace().next()?.parse().ok()?;
    let m = s as u64 / 60;
    Some(match (m / 1440, m / 60 % 24, m % 60) {
        (0, 0, m) => format!("{} min", m),
        (0, h, m) => format!("{} h {} min", h, m),
        (d, h, m) => format!("{} d {} h {} min", d, h, m),
    })
}

/// ログインシェル (bash が brush へのリンクなら、そう出す)
fn shell(uid: u32) -> Option<String> {
    let sh = std::env::var("SHELL").ok().or_else(|| passwd_field(uid, 6))?;
    let name = sh.rsplit('/').next()?.to_string();
    match fs::read_link(&sh).ok().and_then(|t| t.file_name().map(|f| f.to_string_lossy().into_owned())) {
        Some(t) if t != name => Some(format!("{} ({})", name, t)),
        _ => Some(name),
    }
}

fn cpu() -> Option<String> {
    let c = fs::read_to_string("/proc/cpuinfo").ok()?;
    let n = c.lines().filter(|l| l.starts_with("processor")).count();
    let part = c.lines().find_map(|l| l.strip_prefix("CPU part")?.split(':').nth(1).map(|s| s.trim().to_string()));
    let model = match part.as_deref() {
        Some("0xd03") => "Cortex-A53",
        Some("0xd07") => "Cortex-A57",
        Some("0xd08") => "Cortex-A72",
        Some("0xd0b") => "Cortex-A76",
        Some("0xd0c") => "Neoverse-N1",
        _ => "aarch64",
    };
    Some(format!("{} x {}", n, model))
}

/// /proc/meminfo の kB
fn meminfo(key: &str) -> Option<u64> {
    let m = fs::read_to_string("/proc/meminfo").ok()?;
    m.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')?.split_whitespace().next()?.parse().ok())
}

fn mib(kb: u64) -> String {
    format!("{} MiB", kb / 1024)
}

fn memory() -> Option<String> {
    let (t, a) = (meminfo("MemTotal")?, meminfo("MemAvailable")?);
    Some(format!("{} / {}", mib(t - a), mib(t)))
}

fn swap() -> Option<String> {
    let (t, f) = (meminfo("SwapTotal")?, meminfo("SwapFree")?);
    Some(if t == 0 { "none".into() } else { format!("{} / {}", mib(t - f), mib(t)) })
}

fn disk(path: &str) -> Option<String> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let total = s.f_blocks as u64 * s.f_frsize as u64 / 1024;
    let free = s.f_bfree as u64 * s.f_frsize as u64 / 1024;
    Some(format!("{} / {}", mib(total - free), mib(total)))
}

/// 最初のインターフェースのアドレス (DHCP か手で決めたか)
fn ip() -> Option<String> {
    let name = netif::names().ok()?.into_iter().next()?;
    let i = netif::get(&name).ok()?;
    let a = i.addr?;
    Some(format!("{}/{} ({}, {})", netif::fmt(a), i.prefix, name, if i.dhcp { "dhcp" } else { "static" }))
}
