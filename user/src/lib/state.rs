// aios の状態の木 (aios get、doc/aios.md)。/proc /etc /var/lib/aipkg などから集めて 1 つの JSON にする。
// 読むだけなので root でなくてよい (読めないものは入れない)
use serde_json::{Map, Value, json};
use std::fs;

/// 木の一番上の名前 (この順に出す)
pub const ROOTS: [&str; 10] = ["host", "kernel", "mem", "disk", "proc", "service", "pkg", "net", "user", "boot"];

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// root の名前の部分だけ集める (aios get kernel なら kernel だけ見る)
pub fn collect(root: &str) -> Option<Value> {
    Some(match root {
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
    json!({ "name": name, "os": format!("aios (unix) {}", base).trim(), "arch": machine })
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
    json!({ "interfaces": ifs, "routes": routes, "dns": dns })
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

fn boot() -> Value {
    let list = |dir: &str| -> Vec<Value> {
        let mut v: Vec<String> = fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        v.sort();
        v.into_iter().map(Value::from).collect()
    };
    let kernels: Vec<Value> = list("/boot").into_iter().filter(|n| n.as_str().is_some_and(|s| s.starts_with("Image"))).collect();
    json!({ "esp": "/boot", "kernels": kernels, "entries": list("/boot/loader/entries") })
}
