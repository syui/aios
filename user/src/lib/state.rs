// aios の状態の木 (aios get、doc/aios.md)。/proc /etc /var/lib/aipkg などから集めて 1 つの JSON にする。
// 読むだけなので root でなくてよい (読めないものは入れない)
use serde_json::{Map, Value, json};
use std::fs;

/// 木の一番上の名前 (この順に出す)。health はほかの根から見たて (いちばん上: まずここを見る)
pub const ROOTS: [&str; 12] = ["health", "host", "kernel", "mem", "disk", "proc", "service", "pkg", "net", "user", "boot", "log"];

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// root の名前の部分だけ集める (aios get kernel なら kernel だけ見る)
pub fn collect(root: &str) -> Option<Value> {
    Some(match root {
        "health" => health(),
        "host" => host(),
        "kernel" => kernel(),
        "mem" => mem(),
        "disk" => disk(),
        "proc" => procs(),
        "service" => services(),
        "pkg" => pkg(),
        "net" => net(),
        "user" => users(),
        "boot" => boot(),
        "log" => log(),
        _ => return None,
    })
}

pub fn collect_all() -> Value {
    let mut m = Map::new();
    for r in ROOTS {
        if let Some(v) = collect(r) {
            m.insert(r.into(), v);
        }
    }
    Value::Object(m)
}

/// 点でつないだ PATH の値。配列は番号か、name / mount / pid が同じもの
pub fn select<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for key in path {
        cur = match cur {
            Value::Object(m) => m.get(*key)?,
            Value::Array(a) => match key.parse::<usize>().ok().and_then(|i| a.get(i)) {
                Some(x) if !a.iter().any(|e| has_name(e, key)) => x,
                _ => a.iter().find(|e| has_name(e, key))?,
            },
            _ => return None,
        };
    }
    Some(cur)
}

fn has_name(e: &Value, key: &str) -> bool {
    ["name", "mount", "pid"].iter().any(|k| match &e[*k] {
        Value::String(s) => s == key,
        Value::Number(n) => n.to_string() == key,
        _ => false,
    })
}

/// sysctl のような行 (PATH = 値)。配列の要素は名前があれば名前で
pub fn flatten(prefix: &str, v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                flatten(&join(prefix, k), x, out);
            }
        }
        Value::Array(a) if a.iter().all(|e| !e.is_object() && !e.is_array()) => {
            let items: Vec<String> = a.iter().map(scalar).collect();
            out.push(format!("{} = [{}]", prefix, items.join(", ")));
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                // プロセスは名前が重なるので pid で、ディスクはマウント先で、ほかは名前で
                let key = ["pid", "mount", "name"].iter().find_map(|k| match &x[*k] {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                });
                flatten(&join(prefix, &key.unwrap_or_else(|| i.to_string())), x, out);
            }
        }
        _ => out.push(format!("{} = {}", prefix, scalar(v))),
    }
}

fn join(a: &str, b: &str) -> String {
    if a.is_empty() { b.to_string() } else { format!("{}.{}", a, b) }
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        x => x.to_string(),
    }
}

// ---- host ----

fn uname() -> (String, String, String) {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u) };
    let s = |f: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(f.as_ptr()) }.to_string_lossy().into_owned();
    (s(&u.release), s(&u.nodename), s(&u.machine))
}

/// aipkg が入れたパッケージ (名前 → 版)。/var/lib/aipkg/local/NAME-VER-REL
pub fn installed() -> Map<String, Value> {
    let mut m = Map::new();
    let Ok(dir) = fs::read_dir("/var/lib/aipkg/local") else { return m };
    for e in dir.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        // 名前に - が入ることがあるので、後ろから 2 つ (版と rel) を外す
        let mut parts: Vec<&str> = n.rsplitn(3, '-').collect();
        if parts.len() == 3 {
            parts.reverse();
            m.insert(parts[0].to_string(), json!(format!("{}-{}", parts[1], parts[2])));
        }
    }
    m
}

fn host() -> Value {
    let (_, node, machine) = uname();
    let name = read("/etc/hostname").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or(node);
    let base = installed().get("base").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    json!({ "name": name, "os": os_name(&base), "arch": machine })
}

/// OS の名前: /etc/os-release の NAME と VARIANT (「aios (aikernel)」) と base の版。
/// VARIANT は Arch Linux から作った aios (main ブランチ) と、カーネルから作った aios を見分ける
pub fn os_name(version: &str) -> String {
    let rel = read("/etc/os-release").unwrap_or_default();
    let field = |k: &str| rel.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=').map(|v| v.trim().trim_matches('"').to_string())).filter(|v| !v.is_empty());
    let name = field("NAME").unwrap_or_else(|| "aios".into());
    let variant = field("VARIANT").map_or(String::new(), |v| format!(" ({})", v));
    format!("{}{} {}", name, variant, version).trim().to_string()
}

// ---- kernel ----

fn kernel() -> Value {
    let (release, _, _) = uname();
    let uptime: f64 = read("/proc/uptime").and_then(|u| u.split_whitespace().next()?.parse().ok()).unwrap_or(0.0);
    let cpus = read("/proc/cpuinfo").map_or(0, |c| c.lines().filter(|l| l.starts_with("processor")).count());
    let modules: Vec<Value> = read("/proc/modules")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().next().map(|n| json!(n)))
        .collect();
    let mut k = json!({
        "release": release,
        "cmdline": read("/proc/cmdline").unwrap_or_default().trim(),
        "uptime_s": (uptime * 10.0).round() / 10.0,
        "cpus": cpus,
        "modules": modules,
    });
    // aios だけにあるもの (Linux の /proc/kmsg は読むと待つので、/proc/bkl があるときだけ)
    if let Some(b) = read("/proc/bkl") {
        k["bkl"] = bkl(&b);
        if let Some(t) = read("/proc/kmsg") {
            let lines: Vec<&str> = t.lines().collect();
            let tail: Vec<Value> = lines[lines.len().saturating_sub(10)..].iter().map(|l| json!(l)).collect();
            k["kmsg_tail"] = Value::Array(tail);
        }
        // /proc/sys (aios のものは少ないので、ぜんぶ。Linux では多すぎるので出さない)
        let mut sc = Map::new();
        for (name, v) in crate::sysctl::all() {
            sc.insert(name, json!(v));
        }
        k["sysctl"] = Value::Object(sc);
    }
    k
}

/// /proc/bkl の「all」の行と、長く持ったものの上から 5 つ
fn bkl(text: &str) -> Value {
    let mut out = json!({});
    if let Some(all) = text.lines().find(|l| l.starts_with("all")) {
        let n: Vec<f64> = all.split_whitespace().filter_map(|w| w.trim_end_matches('%').parse().ok()).collect();
        if n.len() >= 4 {
            out = json!({ "wait_ms": n[0], "wait_pct": n[1], "hold_ms": n[2], "hold_pct": n[3] });
        }
    }
    let top: Vec<Value> = text
        .lines()
        .skip_while(|l| !l.starts_with("hold の長いもの"))
        .skip(1)
        .take(5)
        .filter_map(|l| {
            // 「名前 回数 合計 ms 平均 us」。名前に空白が入ることがある ((page fault)) ので後ろから読む
            let w: Vec<&str> = l.split_whitespace().collect();
            let n = w.len();
            (n >= 6 && w[n - 1] == "us").then(|| {
                json!({ "name": w[..n - 5].join(" "), "count": w[n - 5].parse::<u64>().unwrap_or(0), "total_ms": w[n - 4].parse::<f64>().unwrap_or(0.0) })
            })
        })
        .collect();
    out["top"] = Value::Array(top);
    out
}

// ---- mem / disk ----

fn meminfo() -> Map<String, Value> {
    read("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.to_string(), json!(v.split_whitespace().next()?.parse::<u64>().ok()?)))
        })
        .collect()
}

fn mem() -> Value {
    let m = meminfo();
    let g = |k: &str| m.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let avail = if m.contains_key("MemAvailable") { g("MemAvailable") } else { g("MemFree") };
    json!({
        "total_kib": g("MemTotal"),
        "used_kib": g("MemTotal").saturating_sub(avail),
        "swap_total_kib": g("SwapTotal"),
        "swap_used_kib": g("SwapTotal").saturating_sub(g("SwapFree")),
    })
}

fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let bs = s.f_frsize as u64;
    Some(((s.f_blocks - s.f_bfree) as u64 * bs / 1024, s.f_blocks as u64 * bs / 1024))
}

fn disk() -> Value {
    let mut out = Vec::new();
    for l in read("/proc/mounts").unwrap_or_default().lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        if w.len() < 3 || matches!(w[2], "proc" | "sysfs" | "devtmpfs" | "devpts" | "cgroup" | "cgroup2" | "mqueue" | "debugfs" | "securityfs" | "pstore" | "bpf" | "tracefs" | "configfs" | "fusectl") {
            continue;
        }
        let Some((used, total)) = statvfs(w[1]) else { continue };
        out.push(json!({ "mount": w[1], "dev": w[0], "fs": w[2], "used_kib": used, "total_kib": total }));
    }
    Value::Array(out)
}

// ---- proc ----

fn procs() -> Value {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else { return Value::Array(out) };
    for e in dir.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Some(stat) = read(&format!("/proc/{}/stat", pid)) else { continue };
        let (Some(l), Some(r)) = (stat.find('('), stat.rfind(')')) else { continue };
        let f: Vec<&str> = stat[r + 1..].split_whitespace().collect();
        let num = |i: usize| f.get(i - 3).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        let rss = read(&format!("/proc/{}/status", pid))
            .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse::<u64>().ok())
            .unwrap_or(0);
        let cmd = fs::read(format!("/proc/{}/cmdline", pid)).map(|b| String::from_utf8_lossy(&b).replace('\0', " ").trim().to_string()).unwrap_or_default();
        out.push(json!({
            "pid": pid,
            "ppid": num(4),
            "name": &stat[l + 1..r],
            "state": f.first().copied().unwrap_or("?"),
            "threads": num(20),
            "cpu_s": (num(14) + num(15)) as f64 / 100.0,
            "rss_kib": rss,
            "cmd": cmd,
        }));
    }
    out.sort_by_key(|p| p["pid"].as_u64());
    Value::Array(out)
}

// ---- service ----

fn services() -> Value {
    let enabled = crate::unit::enabled();
    let mut active: std::collections::HashMap<String, (String, String)> = Default::default();
    // init に聞く (systemctl list-units。init がいなければ空)
    if let Ok(o) = std::process::Command::new("systemctl").arg("list-units").stderr(std::process::Stdio::null()).output() {
        for l in String::from_utf8_lossy(&o.stdout).lines().skip(1) {
            let w: Vec<&str> = l.split_whitespace().collect();
            if w.len() >= 3 {
                active.insert(w[0].to_string(), (w[1].to_string(), w[2].to_string()));
            }
        }
    }
    let mut out = Vec::new();
    for (name, u) in crate::unit::load_all() {
        let short = name.strip_suffix(".service").unwrap_or(&name).to_string();
        let (a, sub) = active.get(&name).cloned().unwrap_or(("unknown".into(), String::new()));
        out.push(json!({
            "name": short,
            "active": a,
            "sub": sub,
            "enabled": enabled.contains(&name),
            "description": u.description,
        }));
    }
    Value::Array(out)
}

// ---- pkg ----

fn pkg() -> Value {
    let repos: Vec<Value> = read("/etc/aipkg.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let name = l.strip_prefix('[')?.strip_suffix(']')?;
            (name != "options").then(|| json!(name))
        })
        .collect();
    json!({ "installed": Value::Object(installed()), "repos": repos })
}

// ---- net ----

fn net() -> Value {
    let mut ifs = Vec::new();
    for name in crate::netif::names().unwrap_or_default() {
        let Ok(i) = crate::netif::get(&name) else { continue };
        ifs.push(json!({
            "name": name,
            "addr": i.addr.map(|a| format!("{}/{}", crate::netif::fmt(a), i.prefix)),
            "dhcp": i.dhcp,
        }));
    }
    let routes: Vec<Value> = crate::netif::routes()
        .into_iter()
        .map(|(dev, dst, gw, prefix)| json!({ "dev": dev, "dst": format!("{}/{}", crate::netif::fmt(dst), prefix), "gw": gw.map(crate::netif::fmt) }))
        .collect();
    let dns: Vec<Value> = read("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("nameserver").map(|s| json!(s.trim())))
        .collect();
    json!({ "interfaces": ifs, "routes": routes, "dns": dns, "firewall": firewall() })
}

/// net.firewall: nftables のいまの様子 (/proc/ai/fw のまとめ: root と wheel の人だけ読める)、aios が入れた決まり
/// (/etc/nftables.conf と、そのあとにだれかが変えたか)、confirm を待っているもの
fn firewall() -> Value {
    let mut f = json!({ "file": crate::config::NFT_CONF, "file_exists": std::path::Path::new(crate::config::NFT_CONF).exists() });
    let rec: Value = read(crate::config::FW_STATE).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    match read("/proc/ai/fw") {
        Some(t) => {
            let lines: Vec<Value> = t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
            if let Some(h) = lines.first() {
                for k in ["active", "conntrack", "tables", "rules", "generation"] {
                    f[k] = h[k].clone();
                }
                f["changed_outside_aios"] = json!(rec["generation"].as_u64().is_some_and(|g| h["generation"].as_u64() != Some(g)));
            }
            let chains: Vec<Value> = lines.iter().filter(|l| l.get("hook").is_some() && l.get("chain").is_some()).map(|c| json!({ "chain": c["chain"], "hook": c["hook"], "priority": c["priority"], "policy": c["policy"], "seen": c["seen"], "to_policy": c["to_policy"] })).collect();
            f["base_chains"] = json!(chains);
            if let Some(d) = lines.iter().find(|l| l.get("drops").is_some()) {
                f["drops"] = d["drops"].clone();
            }
            let recent: Vec<Value> = lines.iter().filter(|l| l.get("ago_ms").is_some() && l.get("hook").is_some()).take(5).cloned().collect();
            f["recent_drops"] = json!(recent);
            if let Some(c) = lines.iter().find_map(|l| l.get("conntrack").filter(|c| c.is_object())) {
                f["conntrack_entries"] = c.clone();
            }
        }
        None => f["error"] = json!("/proc/ai/fw: not readable (root or wheel), or not the aios kernel"),
    }
    if let Some(p) = read(crate::config::FW_PENDING).and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        f["pending_confirm"] = json!({ "record": p["n"], "revert_in_s": p["deadline"].as_u64().unwrap_or(0).saturating_sub(t) });
    }
    f
}

// ---- user ----

fn users() -> Value {
    let groups = read("/etc/group").unwrap_or_default();
    let mut out = Vec::new();
    for l in read("/etc/passwd").unwrap_or_default().lines() {
        let f: Vec<&str> = l.split(':').collect();
        if f.len() < 7 {
            continue;
        }
        let uid: u32 = f[2].parse().unwrap_or(0);
        // ふつうのユーザー (1000 から) と root だけ
        if uid != 0 && uid < 1000 {
            continue;
        }
        let member: Vec<Value> = groups
            .lines()
            .filter_map(|g| {
                let gf: Vec<&str> = g.split(':').collect();
                (gf.len() >= 4 && gf[3].split(',').any(|m| m == f[0])).then(|| json!(gf[0]))
            })
            .collect();
        out.push(json!({ "name": f[0], "uid": uid, "home": f[5], "shell": f[6], "groups": member }));
    }
    Value::Array(out)
}

// ---- boot ----

/// 起動の一覧 (systemd-boot と同じ entry)
pub const BOOT_ENTRIES: &str = "/boot/loader/entries";

/// 試している新しいカーネルの entry (aios install kernel が作る aios-new+LEFT-DONE.conf。systemd-boot と aiboot が
/// 起動のたびに名前の数を進める): ファイルの名前と、残りと済んだ回数 (数えの名前でなければ None)
pub fn boot_new_entry() -> Option<(String, Option<(u64, u64)>)> {
    let mut names: Vec<String> = fs::read_dir(BOOT_ENTRIES).ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names.into_iter().find_map(|n| {
        let lower = n.to_ascii_lowercase();
        let stem = lower.strip_suffix(".conf")?;
        if stem == "aios-new" {
            return Some((n, None));
        }
        let rest = stem.strip_prefix("aios-new+")?;
        let tries = match rest.split_once('-') {
            Some((l, d)) => (l.parse().ok()?, d.parse().ok()?),
            None => (rest.parse().ok()?, 0),
        };
        Some((n, Some(tries)))
    })
}

fn boot() -> Value {
    let list = |dir: &str| -> Vec<Value> {
        let mut v: Vec<String> = fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        v.sort();
        v.into_iter().map(Value::from).collect()
    };
    let kernels: Vec<Value> = list("/boot").into_iter().filter(|n| n.as_str().is_some_and(|s| s.starts_with("Image"))).collect();
    let new = boot_new_entry();
    // boot counting (doc/aios.md): new は試している新しいカーネルの entry、try はその残りの回数
    // (どちらも試していなければ null)。last は aiosd が残した最後の起動の結果
    let last = aiosd_log().into_iter().rev().find(|r| r["req"]["op"] == "boot").map(|r| {
        let mut l = json!({ "t": r["t"], "result": r["req"]["result"], "status": r["status"] });
        // 新しいカーネルで起動しなかったわけ (unfinished / load / chosen)
        if !r["req"]["why"].is_null() {
            l["why"] = r["req"]["why"].clone();
        }
        l
    });
    json!({
        "esp": "/boot",
        "kernels": kernels,
        "entries": list(BOOT_ENTRIES),
        "new": new.as_ref().map(|n| n.0.clone()),
        "try": new.and_then(|n| n.1).map(|t| t.0),
        "prev": fs::metadata("/boot/Image.prev").is_ok(),
        "last": last,
    })
}

// ---- log ----

/// aiosd がしたこと (/var/log/aiosd.log、1 行 1 つの JSON)
const AIOSD_LOG: &str = "/var/log/aiosd.log";
/// log に出す新しいものの数
const LOG_TAIL: usize = 20;

fn aiosd_log() -> Vec<Value> {
    read(AIOSD_LOG).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// aiosd の新しい記録と、apply の記録 (/var/lib/aios/history) の一覧
fn log() -> Value {
    let all = aiosd_log();
    // pid は呼んだプロセス (配列の名前に pid を使わないように caller に)
    let aiosd: Vec<Value> = all[all.len().saturating_sub(LOG_TAIL)..]
        .iter()
        .map(|r| json!({ "t": r["t"], "uid": r["uid"], "caller": r["pid"], "op": r["req"]["op"], "req": r["req"], "status": r["status"] }))
        .collect();
    let mut hist: Vec<(u64, Value)> = fs::read_dir("/var/lib/aios/history")
        .map(|d| {
            d.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().strip_suffix(".json")?.parse::<u64>().ok()?;
                    let h: Value = serde_json::from_str(&read(&e.path().display().to_string())?).ok()?;
                    let ok = h["steps"].as_array().is_none_or(|s| s.iter().all(|x| x["ok"] == true));
                    Some((n, json!({ "n": n, "t": h["t"], "uid": h["uid"], "ok": ok, "steps": h["steps"].as_array().map_or(0, |s| s.len()), "rollback_of": h["rollback_of"] })))
                })
                .collect()
        })
        .unwrap_or_default();
    hist.sort_by_key(|h| h.0);
    let apply: Vec<Value> = hist.into_iter().rev().take(LOG_TAIL).rev().map(|h| h.1).collect();
    json!({ "aiosd": aiosd, "apply": apply })
}

// ---- health ----
//
// いまの見たて (aios get health): ほかの根と /proc/ai から「どこがおかしいか」を集め、1 つずつ status (ok / info / warn /
// crit)、what (何が)、hint (次に使う道具: aios get、aish のツール、コマンド) を出す。いちばん悪いものが全体の status。
// AI がまず見るところ。読めないもの (/proc/ai は root と wheel) は見ずに、そう書く。ネットは見ない (ap -Qu は
// いちばん新しい ap -Sy の同期データベースとくらべる)

/// 1 つの見たて
fn check(name: &str, status: &str, what: String, hint: &str) -> Value {
    let mut c = json!({ "name": name, "status": status, "what": what });
    if !hint.is_empty() && status != "ok" {
        c["hint"] = json!(hint);
    }
    c
}

fn rank(s: &str) -> u8 {
    match s {
        "crit" => 3,
        "warn" => 2,
        "info" => 1,
        _ => 0,
    }
}

/// KiB を読みやすく
fn kib(k: u64) -> String {
    if k >= 1 << 20 { format!("{:.1} GiB", k as f64 / (1u64 << 20) as f64) } else { format!("{} MiB", k >> 10) }
}

fn health() -> Value {
    let mut cs: Vec<Value> = Vec::new();

    // サービス: しくじったもの
    let svc = services();
    let failed: Vec<String> = svc.as_array().into_iter().flatten().filter(|s| s["active"] == "failed").filter_map(|s| s["name"].as_str().map(String::from)).collect();
    cs.push(if failed.is_empty() {
        check("service", "ok", format!("{} units, none failed", svc.as_array().map_or(0, |a| a.len())), "")
    } else {
        check("service", "crit", format!("failed: {}", failed.join(" ")), "systemctl status NAME; journalctl -u NAME (aish: log)")
    });

    // ディスク: 9 割で warn、98% で crit
    let mut full: Vec<(String, u64, u64)> = Vec::new();
    for d in disk().as_array().into_iter().flatten() {
        let (used, total) = (d["used_kib"].as_u64().unwrap_or(0), d["total_kib"].as_u64().unwrap_or(0));
        if total > 0 && used * 100 >= total * 90 {
            full.push((d["mount"].as_str().unwrap_or("?").to_string(), used * 100 / total, total - used));
        }
    }
    let worst = full.iter().map(|f| f.1).max().unwrap_or(0);
    cs.push(if full.is_empty() {
        check("disk", "ok", "every mount under 90%".into(), "")
    } else {
        let what = full.iter().map(|(m, p, left)| format!("{} {}% ({} left)", m, p, kib(*left))).collect::<Vec<_>>().join(", ");
        check("disk", if worst >= 98 { "crit" } else { "warn" }, what, "aios get disk; du -sh /var/cache/* ~/.cache/*; ap -Sc")
    });

    // メモリ: 使えるものが 1 割を切ったら warn、3% で crit。スワップが半分をこえたら warn
    let m = meminfo();
    let g = |k: &str| m.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let (total, avail) = (g("MemTotal"), if m.contains_key("MemAvailable") { g("MemAvailable") } else { g("MemFree") });
    let (st, sf) = (g("SwapTotal"), g("SwapFree"));
    let pct = (avail * 100).checked_div(total).unwrap_or(100);
    let mut ms = if pct < 3 { "crit" } else if pct < 10 { "warn" } else { "ok" };
    let mut what = format!("{} of {} available ({}%)", kib(avail), kib(total), pct);
    if st > 0 && (st - sf) * 2 > st {
        ms = if rank(ms) < 2 { "warn" } else { ms };
        what.push_str(&format!(", swap {} of {} used", kib(st - sf), kib(st)));
    }
    cs.push(check("mem", ms, what, "aios get proc (rss_kib); aish: mem"));

    // カーネル (aios のものだけ): BKL の取りわすれ、ヒープ、oom
    if let Some(b) = read("/proc/bkl") {
        let unlocked: Vec<&str> = b.lines().filter(|l| l.starts_with("unlocked ")).collect();
        cs.push(if unlocked.is_empty() {
            check("bkl", "ok", "nothing touched without the big lock".into(), "")
        } else {
            check("bkl", "crit", format!("touched without the big lock: {}", unlocked.iter().map(|l| l.trim_start_matches("unlocked ")).collect::<Vec<_>>().join(", ")), "a kernel bug: the FILE:LINE in kernel/src; cat /proc/bkl; aish: bkl")
        });
        match read("/proc/ai/heap").and_then(|t| serde_json::from_str::<Value>(t.lines().next()?).ok()) {
            Some(h) => {
                let n = |k: &str| h[k].as_u64().unwrap_or(0);
                let (fails, kills, emerg) = (n("fails"), n("oom_kills"), n("emergencies"));
                let st = if kills > 0 || emerg > 0 { "warn" } else if fails > 0 { "info" } else { "ok" };
                cs.push(check("heap", st, format!("kernel heap {} used of {} (peak {}), {} failed allocations, {} oom kills, {} emergencies", kib(n("used") / 1024), kib(n("size") / 1024), kib(n("peak") / 1024), fails, kills, emerg), "cat /proc/ai/heap; dmesg | grep oom; aish: mem"));
            }
            None => cs.push(check("heap", "info", "/proc/ai/heap: not readable (root or wheel)".into(), "sudo aios get health")),
        }
        // oom で殺したもの (kmsg の終わり)
        let kills: Vec<String> = read("/proc/kmsg").unwrap_or_default().lines().filter(|l| l.contains("oom: killed")).map(|l| l.trim().to_string()).collect();
        if let Some(last) = kills.last() {
            cs.push(check("oom", "warn", format!("{} oom kill(s) in the kernel log, last: {}", kills.len(), last), "dmesg | grep oom; aios get proc"));
        }
        // 長く眠ったままのスレッド: ディスクを待つもの (ps の D) が 60 秒より長い (Linux の hung task は 120 秒)
        let d: Vec<u64> = procs().as_array().into_iter().flatten().filter(|p| p["state"] == "D").filter_map(|p| p["pid"].as_u64()).collect();
        if !d.is_empty() {
            let stuck: Vec<String> = read("/proc/ai/threads")
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|t| t["pid"].as_u64().is_some_and(|p| d.contains(&p)) && t["slept_s"].as_f64().is_some_and(|s| s > 60.0))
                .map(|t| format!("{} ({}) {}s in {}", t["tid"], t["name"].as_str().unwrap_or("?"), t["slept_s"].as_f64().unwrap_or(0.0) as u64, t["sys"].as_str().unwrap_or("?")))
                .collect();
            cs.push(if stuck.is_empty() {
                check("hung", "ok", format!("{} in disk wait, none longer than 60 s", d.len()), "")
            } else {
                check("hung", "warn", format!("waiting on the disk for long: {}", stuck.join(", ")), "aish: hang, stack TID, threads")
            });
        } else {
            cs.push(check("hung", "ok", "nothing waiting on the disk".into(), ""));
        }
    }

    // 起動: 試している新しいカーネル
    match boot_new_entry() {
        Some((e, tries)) => cs.push(check("boot", "info", format!("trying a new kernel ({}{}); aiosd keeps it once it boots", e, tries.map_or(String::new(), |t| format!(", {} tries left", t.0))), "aios get boot")),
        None => cs.push(check("boot", "ok", "no kernel on trial".into(), "")),
    }

    // ファイアウォール: 確かめ待ち (aios confirm しなければ戻る)、aios の外で変えたもの
    let fw = firewall();
    if let Some(p) = fw.get("pending_confirm") {
        cs.push(check("firewall", "warn", format!("apply {} waits for aios confirm; reverts in {} s", p["record"], p["revert_in_s"]), "aios confirm (or let it revert); aios get net.firewall"));
    } else if fw["changed_outside_aios"] == true {
        cs.push(check("firewall", "info", "the ruleset was changed outside aios apply".into(), "aios diff; aios config (to keep it) or aios apply (to restore)"));
    } else {
        cs.push(check("firewall", "ok", format!("{} rules", fw["rules"].as_u64().unwrap_or(0)), ""));
    }

    // apply: いちばん新しい記録がしくじった
    let lg = log();
    if let Some(last) = lg["apply"].as_array().and_then(|a| a.last()) {
        cs.push(if last["ok"] == true {
            check("apply", "ok", format!("last apply {} ok", last["n"]), "")
        } else {
            check("apply", "warn", format!("last apply {} failed", last["n"]), "aios get log.apply; aios history; aios rollback")
        });
    }

    // パッケージ: 同期データベースより古いもの (ap -Qu。ネットは見ない)
    if let Ok(o) = std::process::Command::new("ap").arg("-Qu").stderr(std::process::Stdio::null()).output() {
        let lines: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().map(String::from).collect();
        let synced = fs::read_dir("/var/lib/aipkg/sync")
            .ok()
            .and_then(|d| d.flatten().filter_map(|e| e.metadata().ok()?.modified().ok()).max())
            .and_then(|t| t.elapsed().ok());
        let age = synced.map_or(String::from("never synced"), |a| format!("synced {} h ago", a.as_secs() / 3600));
        cs.push(if synced.is_none() {
            // 同期データベースがなければ、新しい版があるかはわからない
            check("pkg", "info", "the repository was never synced: cannot tell what is new".into(), "sudo ap -Sy; then aios health")
        } else if lines.is_empty() {
            check("pkg", "ok", format!("up to date ({})", age), "")
        } else {
            check("pkg", "info", format!("{} can be upgraded ({}): {}", lines.len(), age, lines.iter().take(8).cloned().collect::<Vec<_>>().join(", ")), "sudo ap -Syu")
        });
    }

    let worst = cs.iter().map(|c| c["status"].as_str().unwrap_or("ok")).max_by_key(|s| rank(s)).unwrap_or("ok").to_string();
    let bad: Vec<String> = cs.iter().filter(|c| rank(c["status"].as_str().unwrap_or("ok")) >= 2).map(|c| format!("{}: {}", c["name"].as_str().unwrap_or(""), c["what"].as_str().unwrap_or(""))).collect();
    json!({
        "status": worst,
        "summary": if bad.is_empty() { "nothing needs attention".to_string() } else { bad.join("; ") },
        "checks": cs,
    })
}
