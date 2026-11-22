// trace: カーネルの中の出来事 (/proc/ai/trace)。cmd を動かしているあいだのシステムコールの入りと出、切りかえ、眠りと
// 起こし、割り込み、ディスク、フォルトを記録して、スレッドごと・システムコールごと・遅かったものにまとめる
use super::{mib, read_root, run, tail, timeout_of, write_root};
use aish_plugin::{Value, error, json, s};
use std::path::Path;

/// trace: cmd があれば、記録を空にして (trace clear)、name (既定は cmd の最初の語。all なら絞らない) で記録を始め
/// (trace on)、cmd を動かし、止めて (trace off) 読む。なければ、いま記録にあるものを読むだけ
pub(crate) fn trace(a: &Value, pwd: &str) -> Value {
    if !Path::new("/proc/ai/trace").exists() {
        return error("no /proc/ai/trace (not aios, or an older kernel)");
    }
    let cmd = s(a, "cmd");
    let mut ran = Value::Null;
    if !cmd.is_empty() {
        let name = if a["all"].as_bool().unwrap_or(false) {
            String::new()
        } else if !s(a, "name").is_empty() {
            s(a, "name").to_string()
        } else {
            Path::new(cmd.split_whitespace().next().unwrap_or("")).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())
        };
        for c in ["trace clear", &format!("trace on {}", name)] {
            if let Err(e) = write_root("/proc/ai/ctl", c) {
                return error(e);
            }
        }
        let r = run(cmd, pwd, timeout_of(a));
        let _ = write_root("/proc/ai/ctl", "trace off");
        match r {
            Ok((st, out, err, ms, timeout)) => ran = json!({ "status": st, "out": tail(&out, "", 20), "err": tail(&err, "", 20), "ms": ms, "timeout": timeout }),
            Err(e) => return error(e),
        }
    }
    let text = match read_root("/proc/ai/trace") {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    let limit = a["limit"].as_u64().unwrap_or(200) as usize;
    let (summary, head, events) = trace_summary(&text, limit);
    json!({ "text": summary, "ran": ran, "trace": head, "events": events })
}

/// /proc/ai/trace の中身から、まとめ (text) と、はじめの行と、終わりの limit この出来事
fn trace_summary(text: &str, limit: usize) -> (String, Value, Vec<Value>) {
    use std::collections::BTreeMap;
    let mut lines = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok());
    let head = lines.next().unwrap_or(Value::Null);
    let evs: Vec<Value> = lines.collect();
    let u = |v: &Value| v.as_u64().unwrap_or(0);
    let name_of = |tid: u64| head["names"][tid.to_string()].as_str().map_or(format!("?{}", tid), |n| n.to_string());
    // スレッドごと: システムコールの数と時間 (入りから出まで)、眠った数と待った時間、起こされてから走るまで、フォルト
    #[derive(Default)]
    struct T {
        sys: u64,
        sys_ns: u64,
        sleeps: u64,
        waited_ns: u64,
        woken: u64,
        delay_ns: u64,
        faults: u64,
        runs: u64,
    }
    let mut per: BTreeMap<u64, T> = BTreeMap::new();
    let mut pending: BTreeMap<u64, (String, u64)> = BTreeMap::new();
    let mut asleep: BTreeMap<u64, u64> = BTreeMap::new();
    let mut woken_at: BTreeMap<u64, u64> = BTreeMap::new();
    let mut by_sys: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    let mut slow: Vec<(u64, u64, String, u64)> = Vec::new();
    let mut irqs: BTreeMap<u64, u64> = BTreeMap::new();
    let (mut blk_r, mut blk_w, mut blk_sectors, mut blk_ns, mut blk_pending) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut switches, mut t_first, mut t_last) = (0u64, u64::MAX, 0u64);
    for e in &evs {
        let (t, p) = (u(&e["t"]), u(&e["p"]));
        t_first = t_first.min(t);
        t_last = t_last.max(t);
        let k = s(e, "k");
        match k {
            "sys" => {
                pending.insert(p, (s(e, "n").to_string(), t));
            }
            "ret" => {
                let th = per.entry(p).or_default();
                th.sys += 1;
                if let Some((n, t0)) = pending.remove(&p) {
                    let d = t.saturating_sub(t0);
                    th.sys_ns += d;
                    let b = by_sys.entry(n.clone()).or_insert((0, 0, 0));
                    b.0 += 1;
                    b.1 += d;
                    b.2 = b.2.max(d);
                    slow.push((d, p, n, t0));
                }
            }
            "sleep" => {
                per.entry(p).or_default().sleeps += 1;
                asleep.insert(p, t);
            }
            "wake" => {
                if let Some(t0) = asleep.remove(&p) {
                    let th = per.entry(p).or_default();
                    th.woken += 1;
                    th.waited_ns += t.saturating_sub(t0);
                }
                woken_at.insert(p, t);
            }
            "switch" => {
                switches += 1;
                let th = per.entry(p).or_default();
                th.runs += 1;
                if let Some(t0) = woken_at.remove(&p) {
                    th.delay_ns += t.saturating_sub(t0);
                }
            }
            "irq" => *irqs.entry(u(&e["irq"])).or_insert(0) += 1,
            "blk" => {
                if s(e, "op") == "write" { blk_w += 1 } else { blk_r += 1 }
                blk_sectors += u(&e["n"]);
                blk_pending += 1;
            }
            "blkdone" => {
                blk_ns += u(&e["ns"]);
                blk_pending = blk_pending.saturating_sub(1);
            }
            "fault" => per.entry(p).or_default().faults += 1,
            _ => {}
        }
    }
    let ms = |ns: u64| format!("{:.2} ms", ns as f64 / 1e6);
    let mut t = String::new();
    t.push_str(&format!(
        "trace      {} events ({} dropped), {} over {} ms, filter {:?}, {} switches\n",
        evs.len(),
        u(&head["dropped"]),
        if head["on"].as_bool().unwrap_or(false) { "still on" } else { "stopped" },
        if t_last > t_first { (t_last - t_first) / 1_000_000 } else { 0 },
        s(&head, "filter"),
        switches
    ));
    let mut ths: Vec<(&u64, &T)> = per.iter().collect();
    ths.sort_by_key(|(_, th)| std::cmp::Reverse(th.sys_ns + th.waited_ns));
    t.push_str("threads    tid name: syscalls (time), slept (waited, run delay), faults, runs\n");
    for (tid, th) in ths.iter().take(12) {
        t.push_str(&format!(
            "  {:>6} {:<16} {} ({}), {} ({}, {}), {} faults, {} runs\n",
            tid, name_of(**tid), th.sys, ms(th.sys_ns), th.sleeps, ms(th.waited_ns), ms(th.delay_ns), th.faults, th.runs
        ));
    }
    let mut bs: Vec<(&String, &(u64, u64, u64))> = by_sys.iter().collect();
    bs.sort_by_key(|(_, (_, total, _))| std::cmp::Reverse(*total));
    if !bs.is_empty() {
        t.push_str("syscalls   name: count, total, max\n");
        for (n, (c, total, max)) in bs.iter().take(10) {
            t.push_str(&format!("  {:<16} {:>6}  {:>12}  {:>12}\n", n, c, ms(*total), ms(*max)));
        }
    }
    slow.sort_by_key(|x| std::cmp::Reverse(x.0));
    if !slow.is_empty() {
        t.push_str("slowest    time, tid name, syscall, at (ms from the first event)\n");
        for (d, p, n, t0) in slow.iter().take(8) {
            t.push_str(&format!("  {:>12}  {:>6} {:<16} {:<16} {}\n", ms(*d), p, name_of(*p), n, ms(t0.saturating_sub(t_first))));
        }
    }
    if blk_r + blk_w > 0 {
        t.push_str(&format!("disk       {} reads, {} writes, {} ({} in the device, {} not finished)\n", blk_r, blk_w, mib(blk_sectors * 512), ms(blk_ns), blk_pending));
    }
    if !irqs.is_empty() {
        let v: Vec<String> = irqs.iter().map(|(n, c)| format!("{}: {}", n, c)).collect();
        t.push_str(&format!("irqs       {}\n", v.join(", ")));
    }
    let start = evs.len().saturating_sub(limit);
    (t, head, evs[start..].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_text() {
        let t = concat!(
            "{\"on\":false,\"filter\":\"ls\",\"events\":8,\"dropped\":0,\"names\":{\"7\":\"ls\",\"3\":\"init\"}}\n",
            "{\"t\":1000000,\"c\":0,\"p\":7,\"k\":\"switch\",\"from\":3}\n",
            "{\"t\":1100000,\"c\":0,\"p\":7,\"k\":\"sys\",\"n\":\"openat\",\"a\":\"0x1\"}\n",
            "{\"t\":1300000,\"c\":0,\"p\":7,\"k\":\"blk\",\"op\":\"read\",\"sector\":8,\"n\":8}\n",
            "{\"t\":1400000,\"c\":0,\"p\":7,\"k\":\"sleep\",\"chan\":\"0x10\"}\n",
            "{\"t\":2400000,\"c\":1,\"p\":7,\"k\":\"wake\",\"chan\":\"0x10\",\"by\":0}\n",
            "{\"t\":2500000,\"c\":0,\"p\":7,\"k\":\"switch\",\"from\":0}\n",
            "{\"t\":2500000,\"c\":0,\"p\":7,\"k\":\"blkdone\",\"ns\":1200000}\n",
            "{\"t\":2600000,\"c\":0,\"p\":7,\"k\":\"ret\",\"n\":\"openat\",\"r\":3}\n",
        );
        let (text, head, events) = trace_summary(t, 3);
        assert_eq!(head["filter"], "ls");
        assert_eq!(events.len(), 3);
        assert!(text.contains("trace      8 events (0 dropped), stopped over 1 ms, filter \"ls\", 2 switches"), "{}", text);
        assert!(text.contains("       7 ls               1 (1.50 ms), 1 (1.00 ms, 0.10 ms), 0 faults, 2 runs"), "{}", text);
        assert!(text.contains("  openat                1       1.50 ms       1.50 ms"), "{}", text);
        assert!(text.contains("disk       1 reads, 0 writes, 4 KiB (1.20 ms in the device, 0 not finished)"), "{}", text);
    }
}
