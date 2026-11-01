// aiosd: aios を操作する常駐の仕組み (doc/aios.md の段階 2)。root で動く (aiosd.service)
//   /run/aiosd.sock (AF_UNIX) で 1 行 1 つの JSON を受けて、1 行の JSON で答える:
//     {"op":"ping"}
//     {"op":"service","action":"start|stop|restart|enable|disable","name":"sshd"}
//     {"op":"pkg","action":"install|remove|upgrade|refresh","names":["git"]}
//     {"op":"pkg","action":"file","paths":["/home/ai/x-1-1-aarch64.pkg.tar.zst"]}   aipkg -U (aios install pkg)
//     {"op":"power","action":"reboot|poweroff"}
//     {"op":"apply"}      /etc/aios.json のとおりにそろえる (記録は /var/lib/aios/history/N.json)
//     {"op":"rollback"}   ひとつ前の apply の設定に戻す (/etc/aios.json も戻し、そのとき入れたパッケージは外す)
//     {"op":"src"}        /usr/src/aios を作る (root:wheel、2775。中身は aios src が wheel の人として git clone)
//     {"op":"kernel","action":"install","path":"/usr/src/aios/target/Image"}
//                         新しいカーネルを /boot/Image.new に置き、試す entry (aios-new+1.conf) を作る
//     {"op":"kernel","action":"revert"}   試すのをやめる (試していなければ /boot/Image と /boot/Image.prev を入れかえる)
// 起動したとき: 新しいカーネルを試していたら (systemd-boot と同じ boot counting。doc/aios.md の「改造」)、
//   それで起動していれば (aios.new=1) /boot/Image にし (systemd-bless-boot の役)、ほかので起動していれば試すのをやめる
//   答え: {"ok":true|false,"status":N,"out":"...","err":"..."}
// だれが頼んだかは SO_PEERCRED で見る。変える操作は root と wheel のグループの人だけ。
// したことは /var/log/aiosd.log に 1 行 1 つの JSON で残す。読むだけのこと (状態) は aios get が自分で集める
#[path = "../lib/config.rs"]
mod config;
#[path = "../lib/image.rs"]
mod image;
#[path = "../lib/netif.rs"]
#[allow(dead_code)]
mod netif;
#[path = "../lib/sysctl.rs"]
mod sysctl;
#[path = "../lib/state.rs"]
#[allow(dead_code)]
mod state;
#[path = "../lib/unit.rs"]
mod unit;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::sync::Mutex;

const SOCK: &str = "/run/aiosd.sock";
const LOG: &str = "/var/log/aiosd.log";
/// 子のプログラム (aipkg の post_install など) の PATH (/etc/sudoers の secure_path と同じ)
const PATH: &str = "/usr/local/bin:/usr/bin:/bin:/opt/c/bin";

/// 変える操作は 1 つずつ (aipkg を 2 つ同時に動かさない)
static BUSY: Mutex<()> = Mutex::new(());

fn main() {
    // 読み手のいなくなった口に書いても終わらない
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    let _ = std::fs::remove_file(SOCK);
    let l = match UnixListener::bind(SOCK) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("aiosd: {}: {}", SOCK, e);
            std::process::exit(1);
        }
    };
    // だれでもつなげる (できることは相手によって決める)
    let _ = std::fs::set_permissions(SOCK, std::os::unix::fs::PermissionsExt::from_mode(0o666));
    println!("aiosd: listening on {}", SOCK);
    boot_check();
    for c in l.incoming().flatten() {
        std::thread::spawn(move || serve(c));
    }
}

/// つないだ相手 (pid, uid, gid)
fn peer(c: &UnixStream) -> Option<(i32, u32, u32)> {
    use std::os::fd::AsRawFd;
    let mut cr: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(c.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cr as *mut _ as *mut libc::c_void, &mut len) };
    (r == 0).then_some((cr.pid, cr.uid, cr.gid))
}

fn serve(c: UnixStream) {
    let Some((pid, uid, _)) = peer(&c) else { return };
    let Ok(rd) = c.try_clone() else { return };
    let mut w = c;
    for line in BufReader::new(rd).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let reply = handle(&req, uid, pid);
        if writeln!(w, "{}", reply).and_then(|_| w.flush()).is_err() {
            break;
        }
        // 電源を切るのは、答えを返してから
        if reply["ok"] == true && req["op"] == "power" {
            let _ = Command::new("systemctl").arg(req["action"].as_str().unwrap_or("")).env("PATH", PATH).status();
        }
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// root か wheel のグループの人か
fn allowed(uid: u32) -> bool {
    if uid == 0 {
        return true;
    }
    let name = std::fs::read_to_string("/etc/passwd").ok().and_then(|p| {
        p.lines().map(|l| l.split(':').collect::<Vec<_>>()).find(|f| f.len() > 2 && f[2].parse() == Ok(uid)).map(|f| f[0].to_string())
    });
    let Some(name) = name else { return false };
    std::fs::read_to_string("/etc/group").is_ok_and(|g| {
        g.lines().any(|l| {
            let f: Vec<&str> = l.split(':').collect();
            f.len() >= 4 && f[0] == "wheel" && f[3].split(',').any(|m| m == name)
        })
    })
}

fn fail(msg: impl std::fmt::Display) -> Value {
    json!({ "ok": false, "err": msg.to_string() })
}

fn handle(req: &Value, uid: u32, pid: i32) -> Value {
    let op = s(req, "op");
    if op == "ping" {
        return json!({ "ok": true, "version": env!("CARGO_PKG_VERSION"), "uid": uid, "allowed": allowed(uid) });
    }
    let cmd: Vec<String> = match op {
        "service" => {
            let (action, name) = (s(req, "action"), s(req, "name"));
            if !matches!(action, "start" | "stop" | "restart" | "enable" | "disable") {
                return fail(format!("service: unknown action {:?} (start stop restart enable disable)", action));
            }
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c)) {
                return fail("service: bad name");
            }
            vec!["systemctl".into(), action.into(), name.into()]
        }
        "pkg" if s(req, "action") == "file" => {
            // 作ったパッケージのファイル (aios build pkg) を入れる
            let paths: Vec<String> = req["paths"].as_array().map(|a| a.iter().filter_map(|n| n.as_str().map(String::from)).collect()).unwrap_or_default();
            if paths.is_empty() || paths.iter().any(|p| !p.starts_with('/') || !p.ends_with(".pkg.tar.zst") || !std::path::Path::new(p).is_file()) {
                return fail("pkg file: give the full paths of .pkg.tar.zst files");
            }
            let mut v = vec!["aipkg".to_string(), "-U".to_string()];
            v.extend(paths);
            v
        }
        "pkg" => {
            let names: Vec<String> = req["names"].as_array().map(|a| a.iter().filter_map(|n| n.as_str().map(String::from)).collect()).unwrap_or_default();
            if names.iter().any(|n| n.is_empty() || n.starts_with('-') || !n.chars().all(|c| c.is_ascii_alphanumeric() || "-_.+".contains(c))) {
                return fail("pkg: bad name");
            }
            let flag = match s(req, "action") {
                "install" if !names.is_empty() => "-S",
                "remove" if !names.is_empty() => "-R",
                "upgrade" => "-Syu",
                "refresh" => "-Sy",
                a => return fail(format!("pkg: unknown action {:?} or no names (install remove upgrade refresh)", a)),
            };
            let mut v = vec!["aipkg".to_string(), flag.to_string()];
            v.extend(names);
            v
        }
        "power" => {
            if !matches!(s(req, "action"), "reboot" | "poweroff") {
                return fail("power: reboot or poweroff");
            }
            // 動かすのは答えたあと (serve)
            if !allowed(uid) {
                return deny(uid, pid, req);
            }
            record(uid, pid, req, 0);
            return json!({ "ok": true, "status": 0 });
        }
        "apply" | "rollback" => {
            if !allowed(uid) {
                return deny(uid, pid, req);
            }
            let _busy = BUSY.lock();
            let r = if op == "apply" { apply() } else { rollback() };
            let r = match r {
                Ok((cfg, rollback_of)) => apply_cfg(&cfg, uid, rollback_of),
                Err(e) => fail(e),
            };
            record(uid, pid, req, if r["ok"] == true { 0 } else { 1 });
            return r;
        }
        "src" | "kernel" => {
            if !allowed(uid) {
                return deny(uid, pid, req);
            }
            let _busy = BUSY.lock();
            let r = if op == "src" { src_init() } else { kernel(s(req, "action"), s(req, "path")) };
            let r = match r {
                Ok(msg) => json!({ "ok": true, "status": 0, "out": msg }),
                Err(e) => fail(e),
            };
            record(uid, pid, req, if r["ok"] == true { 0 } else { 1 });
            return r;
        }
        _ => return fail(format!("unknown op {:?} (ping service pkg power apply rollback src kernel)", op)),
    };
    if !allowed(uid) {
        return deny(uid, pid, req);
    }
    let _busy = BUSY.lock();
    match Command::new(&cmd[0]).args(&cmd[1..]).env("PATH", PATH).stdin(std::process::Stdio::null()).output() {
        Ok(o) => {
            let status = o.status.code().unwrap_or(-1);
            record(uid, pid, req, status);
            json!({
                "ok": o.status.success(),
                "status": status,
                "cmd": cmd.join(" "),
                "out": String::from_utf8_lossy(&o.stdout),
                "err": String::from_utf8_lossy(&o.stderr),
            })
        }
        Err(e) => fail(format!("{}: {}", cmd[0], e)),
    }
}

fn deny(uid: u32, pid: i32, req: &Value) -> Value {
    record(uid, pid, req, -13);
    fail(format!("permission denied: uid {} is not root or in wheel", uid))
}

fn record(uid: u32, pid: i32, req: &Value, status: i32) {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let line = json!({ "t": t, "uid": uid, "pid": pid, "req": req, "status": status });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(LOG) {
        let _ = writeln!(f, "{}", line);
    }
}

// ---- /etc/aios.json ----

/// apply: いまの /etc/aios.json
fn apply() -> Result<(Value, Option<u64>), String> {
    Ok((config::load(config::PATH)?, None))
}

/// rollback: 最後の apply の前の設定。/etc/aios.json をそれに戻し、最後の apply で入れたパッケージのうち
/// 前の設定にないものを外す
fn rollback() -> Result<(Value, Option<u64>), String> {
    let h = config::history();
    let (Some(&last), Some(&prev)) = (h.last(), h.len().checked_sub(2).and_then(|i| h.get(i))) else {
        return Err("rollback: nothing to roll back to (needs two applies in /var/lib/aios/history)".into());
    };
    let (l, p) = (config::read_history(last).ok_or("rollback: cannot read the last record")?, config::read_history(prev).ok_or("rollback: cannot read the record before")?);
    let cfg = p["config"].clone();
    std::fs::write(config::PATH, serde_json::to_string_pretty(&cfg).unwrap_or_default() + "\n").map_err(|e| format!("{}: {}", config::PATH, e))?;
    let keep: Vec<&str> = cfg["pkg"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
    let drop: Vec<String> = l["installed"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).filter(|n| !keep.contains(n)).map(String::from).collect()).unwrap_or_default();
    if !drop.is_empty() {
        let _ = Command::new("aipkg").arg("-R").args(&drop).env("PATH", PATH).stdin(std::process::Stdio::null()).output();
    }
    Ok((cfg, Some(last)))
}

/// cfg にそろえて、記録を残す
fn apply_cfg(cfg: &Value, uid: u32, rollback_of: Option<u64>) -> Value {
    let steps = match config::plan(cfg) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let mut done = Vec::new();
    let mut installed: Vec<String> = Vec::new();
    let mut all_ok = true;
    for st in &steps {
        let (ok, out) = config::exec(st, PATH);
        all_ok &= ok;
        if let (true, config::Act::Run(cmd)) = (ok, &st.act)
            && cmd.first().map(String::as_str) == Some("aipkg")
            && cmd.get(1).map(String::as_str) == Some("-S")
        {
            installed.extend(cmd[2..].iter().cloned());
        }
        let mut j = st.json();
        j["ok"] = json!(ok);
        let tail: String = out.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect();
        j["out"] = json!(tail);
        done.push(j);
    }
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let n = config::write_history(&json!({ "t": t, "uid": uid, "config": cfg, "steps": done, "installed": installed, "rollback_of": rollback_of }));
    json!({ "ok": all_ok, "n": n, "steps": done })
}

// ---- 改造 (段階 4) ----

const SRC: &str = "/usr/src/aios";

/// /usr/src/aios を wheel の人が書ける場所にする (中身は作らない)
fn src_init() -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(SRC).map_err(|e| format!("{}: {}", SRC, e))?;
    let gid = std::fs::read_to_string("/etc/group")
        .ok()
        .and_then(|g| g.lines().find_map(|l| l.strip_prefix("wheel:")?.split(':').nth(1)?.parse::<u32>().ok()))
        .ok_or("no wheel group")?;
    let c = std::ffi::CString::new(SRC).unwrap_or_default();
    if unsafe { libc::chown(c.as_ptr(), 0, gid) } != 0 {
        return Err(format!("chown {}: {}", SRC, std::io::Error::last_os_error()));
    }
    // setgid: 中に作るものも wheel のグループになる
    std::fs::set_permissions(SRC, std::fs::Permissions::from_mode(0o2775)).map_err(|e| format!("chmod {}: {}", SRC, e))?;
    // 持ち主 (root) と git を動かす人 (wheel の人) がちがうので、git は「dubious ownership」で止まる。
    // git のシステムの設定 (git のパッケージの backup なので、上げても残る) に、ここは安全と書く
    const GITCONFIG: &str = "/etc/gitconfig";
    if let Ok(g) = std::fs::read_to_string(GITCONFIG)
        && !g.lines().any(|l| l.trim() == format!("directory = {}", SRC))
    {
        let add = format!("{}[safe]\n\tdirectory = {}\n", if g.is_empty() || g.ends_with('\n') { "" } else { "\n" }, SRC);
        std::fs::write(GITCONFIG, g + &add).map_err(|e| format!("{}: {}", GITCONFIG, e))?;
    }
    Ok(format!("{} (root:wheel 2775)\n", SRC))
}

// ESP (/boot) の中のカーネルと起動の一覧 (systemd-boot と同じ entry)
const ENTRIES: &str = "/boot/loader/entries";
const IMAGE: &str = "/boot/Image";
const PREV: &str = "/boot/Image.prev";
/// 試している新しいカーネル。entry は aios-new+LEFT-DONE.conf (state::boot_new_entry)
const NEW: &str = "/boot/Image.new";
/// 新しいカーネルを試す回数 (ローダーが起動のたびに名前の LEFT を減らし、0 になった entry はうしろに並ぶ)
const NEW_TRIES: u32 = 1;
/// 前の作りで aiboot が読んでいた、試す回数のファイル (いまは使わない)
const OLD_TRY: &str = "/boot/loader/try";

/// 起動したときに: 試していた新しいカーネルで起動できたか、ローダーがほかのもので起動したか
fn boot_check() {
    let _ = std::fs::remove_file(OLD_TRY);
    let Some((_, tries)) = state::boot_new_entry() else {
        // entry のない Image.new は、途中で止まった install の残り
        let _ = std::fs::remove_file(NEW);
        return;
    };
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let words: Vec<&str> = cmdline.split_whitespace().collect();
    let (msg, why, status) = if words.contains(&"aios.new=1") {
        match promote() {
            Ok(()) => ("the new kernel booted".to_string(), String::new(), 0),
            Err(e) => {
                eprintln!("aiosd: {}", e);
                (format!("the new kernel booted, but making it /boot/Image failed: {}", e), String::new(), 2)
            }
        }
    } else {
        // わけ: aiboot が渡したもの (load) か、数えから (ローダーが試した: unfinished、試していない: chosen)
        let why = words.iter().find_map(|w| w.strip_prefix("aios.fallback_why=")).map(String::from).unwrap_or_else(|| {
            if matches!(tries, Some((0, _))) { "unfinished" } else { "chosen" }.to_string()
        });
        let what = match why.as_str() {
            "unfinished" => "the new kernel did not finish booting (it was tried, but aiosd did not start; reset went back)",
            "load" => "the new kernel could not be loaded (the firmware's LoadImage failed: not a bootable Image)",
            _ => "another entry was chosen in the boot menu (the new kernel was not tried)",
        };
        cancel();
        (format!("{}; /boot/Image is the old one", what), why, 1)
    };
    println!("aiosd: {}", msg);
    let mut req = json!({ "op": "boot", "result": msg });
    if !why.is_empty() {
        req["why"] = json!(why);
    }
    record(0, std::process::id() as i32, &req, status);
}

/// 試していた新しいカーネルで起動できた: /boot/Image にして (前のものは /boot/Image.prev と一覧の
/// 「previous kernel」)、試す entry を消す
fn promote() -> Result<(), String> {
    if std::path::Path::new(IMAGE).exists() {
        std::fs::rename(IMAGE, PREV).map_err(|e| format!("{} -> {}: {}", IMAGE, PREV, e))?;
    }
    std::fs::rename(NEW, IMAGE).map_err(|e| format!("{} -> {}: {}", NEW, IMAGE, e))?;
    prev_entry()?;
    cancel();
    Ok(())
}

/// 試すのをやめる: 試す entry と /boot/Image.new を消す (/boot/Image はそのまま)
fn cancel() {
    while let Some((name, _)) = state::boot_new_entry() {
        if std::fs::remove_file(format!("{}/{}", ENTRIES, name)).is_err() {
            break;
        }
    }
    let _ = std::fs::remove_file(NEW);
}

/// 新しいカーネルを試す / やめる。試すのは systemd-boot と同じ boot counting (doc/aios.md の「改造」)
fn kernel(action: &str, path: &str) -> Result<String, String> {
    match action {
        "install" => {
            let new = std::fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
            if !image::is_image(&new) {
                return Err(format!("{}: not an arm64 Image (aios build kernel makes one)", path));
            }
            cancel();
            std::fs::write(NEW, &new).map_err(|e| format!("{}: {}", NEW, e))?;
            // sort-key があるので、残りがあるあいだは一覧の先頭 (default の aios* に当たる)。
            // options は今の aios.conf と同じに、aiosd が見分けるための aios.new=1 を足す
            let name = format!("aios-new+{}.conf", NEW_TRIES);
            let text = format!("title   aios (new kernel)\nsort-key aios\nlinux   /Image.new\n{}options aios.new=1\n", options("aios.conf"));
            std::fs::create_dir_all(ENTRIES).map_err(|e| format!("{}: {}", ENTRIES, e))?;
            std::fs::write(format!("{}/{}", ENTRIES, name), text).map_err(|e| format!("{}/{}: {}", ENTRIES, name, e))?;
            show_menu();
            Ok(format!(
                "{} <- {} ({} bytes), {}/{}. reboot to try it: if it boots, it becomes {} (the old one {}); if not, the next boot goes back to the old one\n",
                NEW,
                path,
                new.len(),
                ENTRIES,
                name,
                IMAGE,
                PREV
            ))
        }
        "revert" => {
            if state::boot_new_entry().is_some() {
                cancel();
                return Ok(format!("stopped trying the new kernel ({} removed). {} stays\n", NEW, IMAGE));
            }
            let (a, b) = (std::fs::read(IMAGE).map_err(|e| format!("{}: {}", IMAGE, e))?, std::fs::read(PREV).map_err(|e| format!("{}: {} (nothing to revert to)", PREV, e))?);
            std::fs::write(IMAGE, &b).map_err(|e| format!("{}: {}", IMAGE, e))?;
            std::fs::write(PREV, &a).map_err(|e| format!("{}: {}", PREV, e))?;
            Ok(format!("{} and {} swapped. reboot to use it\n", IMAGE, PREV))
        }
        a => Err(format!("kernel: unknown action {:?} (install revert)", a)),
    }
}

/// entry の options の行 (root= など)
fn options(entry: &str) -> String {
    std::fs::read_to_string(format!("{}/{}", ENTRIES, entry))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.split_whitespace().next() == Some("options"))
        .map(|l| format!("{}\n", l))
        .collect()
}

/// 起動の一覧に前のカーネルを出す (prev-aios.conf。default の aios* には当たらない名前)
fn prev_entry() -> Result<(), String> {
    std::fs::create_dir_all(ENTRIES).map_err(|e| format!("{}: {}", ENTRIES, e))?;
    let text = format!("title   aios (previous kernel)\nlinux   /Image.prev\n{}", options("aios.conf"));
    std::fs::write(format!("{}/prev-aios.conf", ENTRIES), text).map_err(|e| format!("{}: {}", ENTRIES, e))?;
    show_menu();
    Ok(())
}

/// 一覧が出るように、loader.conf の timeout 0 を 3 秒にする
fn show_menu() {
    let conf = "/boot/loader/loader.conf";
    if let Ok(t) = std::fs::read_to_string(conf) {
        let fixed: Vec<String> = t.lines().map(|l| if l.split_whitespace().collect::<Vec<_>>() == ["timeout", "0"] { "timeout 3".to_string() } else { l.to_string() }).collect();
        let _ = std::fs::write(conf, fixed.join("\n") + "\n");
    }
}
