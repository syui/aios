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
//                         /boot/Image を入れかえる。前のものは /boot/Image.prev (起動の一覧の「previous kernel」)
//     {"op":"kernel","action":"revert"}   /boot/Image と /boot/Image.prev を入れかえる
// 起動したとき: 新しいカーネルを試していたら (/boot/loader/try) 消して、起動できたことにする。
//   起動しなかったので aiboot が前のカーネルで起動したとき (aios.fallback=1) は、/boot/Image を前のものに戻す
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
    Ok(format!("{} (root:wheel 2775)\n", SRC))
}

/// 新しいカーネルを試す回数 (aiboot が起動のたびに減らし、0 なら前のカーネルで起動する)
const TRY: &str = "/boot/loader/try";

/// 起動したときに: 試していた新しいカーネルで起動できたか、aiboot が前のものに戻したか
fn boot_check() {
    if !std::path::Path::new(TRY).exists() {
        return;
    }
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let (msg, status) = if cmdline.split_whitespace().any(|w| w == "aios.fallback=1") {
        // 前のカーネルで起動している。/boot/Image を前のもの (いま動いているもの) にする
        match kernel("revert", "") {
            Ok(_) => ("the new kernel did not boot; /boot/Image is the previous one again (the new one is /boot/Image.prev)", 1),
            Err(e) => {
                eprintln!("aiosd: {}", e);
                let _ = std::fs::remove_file(TRY);
                ("the new kernel did not boot, and going back failed", 2)
            }
        }
    } else {
        let _ = std::fs::remove_file(TRY);
        ("the new kernel booted", 0)
    };
    println!("aiosd: {}", msg);
    record(0, std::process::id() as i32, &json!({ "op": "boot", "result": msg }), status);
}

/// /boot/Image を入れかえる / 戻す。前のものは /boot/Image.prev と、起動の一覧の entry に残す
fn kernel(action: &str, path: &str) -> Result<String, String> {
    const IMAGE: &str = "/boot/Image";
    const PREV: &str = "/boot/Image.prev";
    match action {
        "install" => {
            let new = std::fs::read(path).map_err(|e| format!("{}: {}", path, e))?;
            if !image::is_image(&new) {
                return Err(format!("{}: not an arm64 Image (aios build kernel makes one)", path));
            }
            if std::path::Path::new(IMAGE).exists() {
                std::fs::copy(IMAGE, PREV).map_err(|e| format!("{} -> {}: {}", IMAGE, PREV, e))?;
            }
            std::fs::write(IMAGE, &new).map_err(|e| format!("{}: {}", IMAGE, e))?;
            prev_entry()?;
            // 1 回だけ試す: 起動して aiosd が動けば消す。動かずに起動しなおすと aiboot が前のものにする
            std::fs::write(TRY, "1").map_err(|e| format!("{}: {}", TRY, e))?;
            Ok(format!(
                "{} <- {} ({} bytes). the old one is {}. reboot to use it; if it does not boot, the next boot goes back to the old one\n",
                IMAGE,
                path,
                new.len(),
                PREV
            ))
        }
        "revert" => {
            let (a, b) = (std::fs::read(IMAGE).map_err(|e| format!("{}: {}", IMAGE, e))?, std::fs::read(PREV).map_err(|e| format!("{}: {} (nothing to revert to)", PREV, e))?);
            std::fs::write(IMAGE, &b).map_err(|e| format!("{}: {}", IMAGE, e))?;
            std::fs::write(PREV, &a).map_err(|e| format!("{}: {}", PREV, e))?;
            let _ = std::fs::remove_file(TRY);
            Ok(format!("{} and {} swapped. reboot to use it\n", IMAGE, PREV))
        }
        a => Err(format!("kernel: unknown action {:?} (install revert)", a)),
    }
}

/// 起動の一覧に前のカーネルを出し (prev-aios.conf。default の aios* には当たらない名前)、
/// 一覧が出るように timeout を 0 から 3 秒にする
fn prev_entry() -> Result<(), String> {
    let dir = "/boot/loader/entries";
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {}", dir, e))?;
    // options (root= など) は今のエントリと同じに
    let options: String = std::fs::read_to_string(format!("{}/aios.conf", dir))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.split_whitespace().next() == Some("options"))
        .map(|l| format!("{}\n", l))
        .collect();
    std::fs::write(format!("{}/prev-aios.conf", dir), format!("title   aios (previous kernel)\nlinux   /Image.prev\n{}", options)).map_err(|e| format!("{}: {}", dir, e))?;
    let conf = "/boot/loader/loader.conf";
    if let Ok(t) = std::fs::read_to_string(conf) {
        let fixed: Vec<String> = t.lines().map(|l| if l.split_whitespace().collect::<Vec<_>>() == ["timeout", "0"] { "timeout 3".to_string() } else { l.to_string() }).collect();
        let _ = std::fs::write(conf, fixed.join("\n") + "\n");
    }
    Ok(())
}
