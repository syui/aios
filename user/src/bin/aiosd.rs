// aiosd: aios を操作する常駐の仕組み (doc/aios.md の段階 2)。root で動く (aiosd.service)
//   /run/aiosd.sock (AF_UNIX) で 1 行 1 つの JSON を受けて、1 行の JSON で答える:
//     {"op":"ping"}
//     {"op":"service","action":"start|stop|restart|enable|disable","name":"sshd"}
//     {"op":"pkg","action":"install|remove|upgrade|refresh","names":["git"]}
//     {"op":"power","action":"reboot|poweroff"}
//     {"op":"apply"}      /etc/aios.json のとおりにそろえる (記録は /var/lib/aios/history/N.json)
//     {"op":"rollback"}   ひとつ前の apply の設定に戻す (/etc/aios.json も戻し、そのとき入れたパッケージは外す)
//   答え: {"ok":true|false,"status":N,"out":"...","err":"..."}
// だれが頼んだかは SO_PEERCRED で見る。変える操作は root と wheel のグループの人だけ。
// したことは /var/log/aiosd.log に 1 行 1 つの JSON で残す。読むだけのこと (状態) は aios get が自分で集める
#[path = "../lib/config.rs"]
mod config;
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
        _ => return fail(format!("unknown op {:?} (ping service pkg power apply rollback)", op)),
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
