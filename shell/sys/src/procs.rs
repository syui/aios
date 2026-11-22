// procs: プロセスの一覧 (CPU の時間かメモリの順)
use super::{kib, read};
use aish_plugin::{Value, json};

pub(crate) struct P {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) state: String,
    pub(crate) threads: u32,
    /// 使った CPU の時間 (1/100 秒)
    pub(crate) ticks: u64,
    pub(crate) rss_kb: u64,
    pub(crate) name: String,
    pub(crate) cmdline: String,
}

pub(crate) fn list_procs() -> Vec<P> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return out };
    for e in dir.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Some(stat) = read(&format!("/proc/{}/stat", pid)) else { continue };
        // pid (comm) state ppid ... utime(14) stime(15) ... threads(20)
        let (Some(l), Some(r)) = (stat.find('('), stat.rfind(')')) else { continue };
        let name = stat[l + 1..r].to_string();
        let f: Vec<&str> = stat[r + 1..].split_whitespace().collect();
        let num = |i: usize| f.get(i - 3).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        let rss_kb = read(&format!("/proc/{}/status", pid))
            .and_then(|st| st.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok())
            .unwrap_or(0);
        let cmdline = std::fs::read(format!("/proc/{}/cmdline", pid))
            .map(|b| String::from_utf8_lossy(&b).replace('\0', " ").trim().to_string())
            .unwrap_or_default();
        out.push(P {
            pid,
            ppid: num(4) as u32,
            state: f.first().unwrap_or(&"?").to_string(),
            threads: num(20) as u32,
            ticks: num(14) + num(15),
            rss_kb,
            name,
            cmdline,
        });
    }
    out
}

pub(crate) fn procs(a: &Value) -> Value {
    let mut ps = list_procs();
    if let Some(n) = a["name"].as_str().filter(|n| !n.is_empty()) {
        ps.retain(|p| p.name.contains(n) || p.cmdline.contains(n));
    }
    match a["sort"].as_str().unwrap_or("cpu") {
        "mem" => ps.sort_by_key(|p| std::cmp::Reverse(p.rss_kb)),
        _ => ps.sort_by_key(|p| std::cmp::Reverse(p.ticks)),
    }
    let limit = a["limit"].as_u64().unwrap_or(20) as usize;
    let mut t = String::from("   pid   ppid s thr    cpu s      rss  cmd\n");
    for p in ps.iter().take(limit) {
        let cmd = if p.cmdline.is_empty() { format!("[{}]", p.name) } else { p.cmdline.chars().take(100).collect() };
        t.push_str(&format!("{:>6} {:>6} {} {:>3} {:>8.1} {:>8}  {}\n", p.pid, p.ppid, p.state, p.threads, p.ticks as f64 / 100.0, kib(p.rss_kb), cmd));
    }
    json!({ "text": t, "count": ps.len() })
}
