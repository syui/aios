// sys (まとめ: カーネル、起きてからの時間、CPU、メモリ、スワップ、ディスク、重いプロセス、BKL、カーネルのメッセージ) と、
// M-s でまとめを画面に出すキー
use super::{ai_json, kib, kmsg_text, mib, read, tail};
use crate::procs::{P, list_procs};
use aish_plugin::{Tty, Value, json};

fn meminfo() -> std::collections::HashMap<String, u64> {
    read("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.to_string(), v.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}

fn disk(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let bs = st.f_frsize as u64;
    Some(((st.f_blocks - st.f_bfree) as u64 * bs / 1024, st.f_blocks as u64 * bs / 1024))
}

fn uname_r() -> String {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return "?".into();
    }
    let f = |a: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(a.as_ptr()) }.to_string_lossy().into_owned();
    format!("{} {} ({})", f(&u.sysname), f(&u.release), f(&u.machine))
}

pub(crate) fn summary() -> Value {
    let mut t = String::new();
    t.push_str(&format!("kernel   {}\n", uname_r()));
    if let Some(up) = read("/proc/uptime").and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok()) {
        let s = up as u64;
        t.push_str(&format!("uptime   {}h {}m {}s\n", s / 3600, s / 60 % 60, s % 60));
    }
    let cpus = read("/proc/cpuinfo").map_or(0, |c| c.lines().filter(|l| l.starts_with("processor")).count());
    t.push_str(&format!("cpu      {}\n", cpus));
    let m = meminfo();
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    let avail = if m.contains_key("MemAvailable") { g("MemAvailable") } else { g("MemFree") };
    t.push_str(&format!("memory   {} / {} used\n", kib(g("MemTotal").saturating_sub(avail)), kib(g("MemTotal"))));
    t.push_str(&format!("swap     {} / {} used\n", kib(g("SwapTotal").saturating_sub(g("SwapFree"))), kib(g("SwapTotal"))));
    if let Some((used, total)) = disk("/") {
        t.push_str(&format!("disk /   {} / {} used ({}%)\n", kib(used), kib(total), used * 100 / total.max(1)));
    }
    let ps = list_procs();
    t.push_str(&format!("procs    {}\n", ps.len()));
    let mut top: Vec<&P> = ps.iter().collect();
    top.sort_by_key(|p| std::cmp::Reverse(p.ticks));
    for p in top.iter().take(5) {
        t.push_str(&format!("  {:>6} {:>8.1}s {:>8} {}\n", p.pid, p.ticks as f64 / 100.0, kib(p.rss_kb), p.name));
    }
    if let Some(b) = read("/proc/bkl")
        && let Some(all) = b.lines().find(|l| l.starts_with("all"))
    {
        t.push_str(&format!("bkl      {}\n", all.trim_start_matches("all").trim()));
    }
    // カーネルの中のメモリ (root か wheel なら): ページキャッシュ、ヒープ、パスの覚え (くわしくは mem と fs)
    if let Ok(m) = ai_json("/proc/ai/mem") {
        let pg = m["page"].as_u64().unwrap_or(4096);
        let h = &m["kernel"]["heap"];
        t.push_str(&format!(
            "cache    page cache {} ({} pages), tmpfs {}, shared {}; heap {} / {} (peak {}, fails {})\n",
            mib(m["page_cache"]["pages"].as_u64().unwrap_or(0) * pg),
            m["page_cache"]["pages"].as_u64().unwrap_or(0),
            mib(m["tmpfs"]["pages"].as_u64().unwrap_or(0) * pg),
            mib(m["shared"]["pages"].as_u64().unwrap_or(0) * pg),
            mib(h["used"].as_u64().unwrap_or(0)),
            mib(h["size"].as_u64().unwrap_or(0)),
            mib(h["peak"].as_u64().unwrap_or(0)),
            h["fails"].as_u64().unwrap_or(0)
        ));
    }
    if let Ok(f) = ai_json("/proc/ai/fs") {
        let p = &f["paths"];
        let (hit, miss) = (p["hits"].as_u64().unwrap_or(0), p["misses"].as_u64().unwrap_or(0));
        t.push_str(&format!("paths    {} cached ({} negative), hit {}%, {} open files\n", p["entries"], p["negative"], hit * 100 / (hit + miss).max(1), f["files"]["open"]));
    }
    if let Some(k) = kmsg_text() {
        t.push_str("kmsg (終わりの 5 行)\n");
        for l in tail(&k, "", 5).lines() {
            t.push_str(&format!("  {}\n", l));
        }
    }
    json!({ "text": t })
}

pub(crate) fn key() -> Value {
    let Ok(mut tty) = Tty::open() else { return json!({}) };
    let t = summary()["text"].as_str().unwrap_or("").to_string();
    let lines: Vec<&str> = t.lines().collect();
    for l in &lines {
        tty.write(&format!("\x1b[2m{}\x1b[0m\r\n", aish_plugin::clip(l, tty.cols().saturating_sub(1))));
    }
    tty.write("\x1b[2m(何かキーで消す)\x1b[0m");
    let _ = tty.key();
    // 出したものを消して、始めの場所 (打ちかけの行の下の行の頭) へ戻る
    tty.write(&format!("\r\x1b[{}A\x1b[J", lines.len()));
    json!({})
}
