// aios: aios を把握・設定・操作するコマンド (doc/aios.md)
//   aios                       様子をロゴといっしょに出す (neofetch のようなもの)。あとに /etc/motd も。
//                              起動のときは motd.service が動かす
//   aios get [PATH] [--json]   状態の木 (host kernel mem disk proc service pkg net user boot)。
//                              PATH は点でつなぐ (kernel.cpus、service.sshd.active)。ふだんは PATH = 値 の行
//   aios do OP ...  [--json]   aiosd (root で動く) に頼んで変える。root と wheel の人だけ:
//                                service start|stop|restart|enable|disable NAME
//                                pkg install|remove NAME... / pkg upgrade / pkg refresh
//                                reboot / poweroff / ping
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
    use std::io::{BufRead, BufReader, Write};
    let as_json = args.iter().any(|a| a == "--json");
    let w: Vec<&str> = args.iter().filter(|a| !a.starts_with("--")).map(String::as_str).collect();
    let req = match w.as_slice() {
        ["service", action, name] => serde_json::json!({ "op": "service", "action": action, "name": name }),
        ["pkg", action, names @ ..] => serde_json::json!({ "op": "pkg", "action": action, "names": names }),
        [p @ ("reboot" | "poweroff")] => serde_json::json!({ "op": "power", "action": p }),
        ["ping"] => serde_json::json!({ "op": "ping" }),
        _ => usage(2),
    };
    let mut c = match std::os::unix::net::UnixStream::connect("/run/aiosd.sock") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("aios do: /run/aiosd.sock: {} (sudo systemctl enable --now aiosd)", e);
            std::process::exit(1);
        }
    };
    let mut line = String::new();
    if writeln!(c, "{}", req).is_err() || BufReader::new(&c).read_line(&mut line).is_err() || line.is_empty() {
        eprintln!("aios do: aiosd did not answer");
        std::process::exit(1);
    }
    let r: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
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
