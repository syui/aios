// aiwatch: カーネルの中を見張る常駐の仕組み (aiwatch.service、root)。/proc/ai を数秒ごとに読んで、
// カーネルのまちがい (起こしが消えた) や、詰まり (起こしすぎ、大きなロックの混雑) を見つけたら知らせる:
//   lost_wakeup_poll   ppoll / select で長く眠っているのに、待っているもの (読める、書ける、閉じた) がもう来ている
//   lost_wakeup_futex  futex で長く眠っているのに、値がもう待つ値でない (変えた人が起こしそこねた)
//   wake_storm         眠っては起きるのを 1 秒に何百回もくり返している (関係ないのに起こされている)
//   bkl_busy           大きなロックがほとんどいつも使われている (システムコールがみな遅くなる)
// 起こしが消えたものは、続けて 2 回見えたら (たまたまの瞬間をのぞく)。起こしすぎと混雑は、それが 2 回続けて
// 見えたら (起動のときなど、しばらく本当に忙しいだけのものをのぞく)。見つけたら:
//   /var/log/aiwatch.service.log (標準出力) に 1 行 1 つの JSON。直ったら "resolved" の行
//   /run/aiwatch.json に、いま続いているもの (aish-sys の watch ツールと aios get が読む)
//   スレッドのことなら、その /proc/ai/stack/TID を /var/lib/aiwatch/ に残す (プロセスが終わっても、あとで
//     aish-sys の stack でたどれるように)。多くなりすぎないよう、古いものから 64 を残して消す
// --rescue をつけると、起こしが消えた futex のスレッドを起こしなおす (/proc/ai/ctl の kick。知らせたあとで)
// aiwatch [-i 秒] [--rescue] [--once]   (--once は 3 回見て (2 回比べて) 結果を出して終わる。試すとき)
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STATE: &str = "/run/aiwatch.json";
const KEEP_DIR: &str = "/var/lib/aiwatch";
const KEEP_MAX: usize = 64;
/// 長く眠っている、とみなす秒
const LONG_S: f64 = 10.0;
/// 起こしすぎ、とみなす 1 秒あたりの回数
const STORM_PER_S: f64 = 300.0;
/// 大きなロックの混雑、とみなす割合 (1 CPU = 100)
const BKL_BUSY_PCT: f64 = 90.0;

const POLLIN: i64 = 0x1;
const POLLPRI: i64 = 0x2;
const POLLOUT: i64 = 0x4;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn lines(path: &str) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// 1 回分の様子
struct Sample {
    threads: BTreeMap<u64, Value>,
    /// 大きなロックを持っていた合計 (秒) と数えはじめからの秒
    bkl: Option<(f64, f64, Value)>,
}

fn sample() -> Sample {
    let threads = lines("/proc/ai/threads").into_iter().filter_map(|t| Some((t["tid"].as_u64()?, t))).collect();
    let bkl = lines("/proc/ai/bkl").into_iter().next().and_then(|b| {
        let span = b["span_s"].as_f64()?;
        Some((b["hold_pct"].as_f64()? * span / 100.0, span, b["top"].clone()))
    });
    Sample { threads, bkl }
}

/// fd 番号 → その様子 (/proc/ai/fd/PID)
fn fds(pid: u64) -> BTreeMap<u64, Value> {
    lines(&format!("/proc/ai/fd/{}", pid)).into_iter().filter_map(|f| Some((f["fd"].as_u64()?, f))).collect()
}

/// ppoll / select で眠っている t が、もう起きているはずか: 待っているものが来ている fd (と、その様子)
fn ready_fd(t: &Value) -> Option<Value> {
    let w = &t["wait"];
    let (poll, want) = (w["poll"].as_array()?, w["want"].as_array()?);
    let fd = fds(t["pid"].as_u64()?);
    for (n, e) in poll.iter().zip(want) {
        let (n, e) = (n.as_u64()?, e.as_i64()?);
        let Some(f) = fd.get(&n) else { continue };
        let (r, wr, hup) = (f["r"] == true, f["w"] == true, f["hup"] == true);
        if (e & (POLLIN | POLLPRI) != 0 && r) || (e & POLLOUT != 0 && wr) || hup {
            return Some(f.clone());
        }
    }
    None
}

/// 見つけたもの 1 つ。key が同じなら同じもの (続いているあいだは 1 度だけ知らせる)
struct Finding {
    key: String,
    v: Value,
}

fn thread_info(t: &Value) -> Value {
    json!({ "pid": t["pid"], "tid": t["tid"], "name": t["name"], "sys": t["sys"], "slept_s": t["slept_s"], "wait": t["wait"] })
}

/// 前と今の 2 回から見つける
fn check(prev: &Sample, cur: &Sample, dt: f64) -> Vec<Finding> {
    let mut out = Vec::new();
    for (tid, t) in &cur.threads {
        let Some(p) = prev.threads.get(tid) else { continue };
        let long = |x: &Value| x["state"] == "sleep" && x["slept_s"].as_f64().is_some_and(|s| s >= LONG_S);
        // 前も今も同じ眠りのまま (起きていない)
        let same_sleep = long(t) && long(p) && t["slept_s"].as_f64() > p["slept_s"].as_f64();
        if same_sleep && t["wait"]["want"].is_array() && ready_fd(p).is_some()
            && let Some(f) = ready_fd(t)
        {
            let mut v = thread_info(t);
            v["kind"] = json!("lost_wakeup_poll");
            v["fd"] = f.clone();
            v["msg"] = json!(format!("{} (tid {}) は {} で {:.0} 秒眠っているが、fd {} ({}) はもう待っているものが来ている: 起こしが消えたかも", s(t, "name"), tid, s(t, "sys"), t["slept_s"].as_f64().unwrap_or(0.0), f["fd"], s(&f, "what")));
            out.push(Finding { key: format!("lost_wakeup_poll:{}", tid), v });
        }
        if same_sleep && t["wait"]["futex"].is_u64() {
            let changed = |x: &Value| x["wait"]["now"] != x["wait"]["val"] && x["wait"]["now"].as_i64() != Some(-1);
            if changed(t) && changed(p) && t["wait"]["futex"] == p["wait"]["futex"] {
                let mut v = thread_info(t);
                v["kind"] = json!("lost_wakeup_futex");
                v["msg"] = json!(format!("{} (tid {}) は futex {:#x} で {:.0} 秒眠っているが、値はもう {} (待っていたのは {}): 起こしそこねたかも", s(t, "name"), tid, t["wait"]["futex"].as_u64().unwrap_or(0), t["slept_s"].as_f64().unwrap_or(0.0), t["wait"]["now"], t["wait"]["val"]));
                out.push(Finding { key: format!("lost_wakeup_futex:{}", tid), v });
            }
        }
        // 起こしすぎ: 走った回数がとても増えているのに、見るたびに最後のシステムコールが待つもの (起きては眠る。
        // 見たときにちょうど起こされて走っていることもある。計算だけのスレッドは 1 秒に何百回も切りかわらない)
        let waiting = |x: &Value| matches!(x["sys"].as_str(), Some("futex" | "ppoll" | "pselect6" | "epoll_pwait" | "epoll_pwait2"));
        if waiting(t) && waiting(p) {
            let runs = t["runs"].as_u64().unwrap_or(0).saturating_sub(p["runs"].as_u64().unwrap_or(0)) as f64;
            let rate = runs / dt.max(0.1);
            if rate >= STORM_PER_S {
                let mut v = thread_info(t);
                v["kind"] = json!("wake_storm");
                v["per_s"] = json!(rate.round());
                v["msg"] = json!(format!("{} (tid {}) は {} で眠っては起きるのを 1 秒に {:.0} 回くり返している: 関係ないものに起こされているか、本当に仕事が多いか", s(t, "name"), tid, s(t, "sys"), rate));
                out.push(Finding { key: format!("wake_storm:{}", tid), v });
            }
        }
    }
    // 大きなロック: 2 回のあいだに持っていた割合 (だれかが数えなおしたら、その回は見ない)
    if let (Some((h0, s0, _)), Some((h1, s1, top))) = (&prev.bkl, &cur.bkl)
        && s1 > s0
    {
        let pct = (h1 - h0) / (s1 - s0) * 100.0;
        if pct >= BKL_BUSY_PCT {
            let top: Vec<Value> = top.as_array().map_or(vec![], |a| a.iter().take(5).cloned().collect());
            out.push(Finding {
                key: "bkl_busy".into(),
                v: json!({ "kind": "bkl_busy", "hold_pct": pct.round(), "top": top, "msg": format!("大きなロックが {:.0}% 使われている (1 CPU = 100%): システムコールがみな遅くなる。aish-sys の bkl で", pct) }),
            });
        }
    }
    out
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

/// スレッドの /proc/ai/stack/TID を残す (多くなったら古いものから消す)
fn keep_stack(tid: u64, kind: &str) -> Option<String> {
    let st = std::fs::read_to_string(format!("/proc/ai/stack/{}", tid)).ok()?;
    std::fs::create_dir_all(KEEP_DIR).ok()?;
    let path = format!("{}/{}-{}-{}.json", KEEP_DIR, now(), kind, tid);
    std::fs::write(&path, st).ok()?;
    let mut all: Vec<_> = std::fs::read_dir(KEEP_DIR).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    all.sort();
    for old in all.iter().take(all.len().saturating_sub(KEEP_MAX)) {
        let _ = std::fs::remove_file(old);
    }
    Some(path)
}

fn log(v: &Value) {
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{}", v);
    let _ = o.flush();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut interval = 5u64;
    let (mut rescue, mut once) = (false, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-i" => {
                i += 1;
                interval = args.get(i).and_then(|v| v.parse().ok()).filter(|&v| v > 0).unwrap_or(interval);
            }
            "--rescue" => rescue = true,
            "--once" => once = true,
            _ => {
                eprintln!("usage: aiwatch [-i SECONDS] [--rescue] [--once]");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if !std::path::Path::new("/proc/ai/threads").exists() {
        eprintln!("aiwatch: no /proc/ai (an older kernel?)");
        std::process::exit(1);
    }
    // いま続いているもの: key → (はじめて見えた時刻, 中身)
    let mut active: BTreeMap<String, (u64, Value)> = BTreeMap::new();
    // 起こしすぎと混雑で、前の回に 1 度だけ見えたもの (もう 1 度見えたら知らせる)
    let mut pending: BTreeSet<String> = BTreeSet::new();
    let mut prev = sample();
    let mut last = std::time::Instant::now();
    let mut rounds = 0;
    loop {
        rounds += 1;
        std::thread::sleep(Duration::from_secs(interval));
        let cur = sample();
        let dt = last.elapsed().as_secs_f64();
        last = std::time::Instant::now();
        let found = check(&prev, &cur, dt);
        let keys: BTreeSet<String> = found.iter().map(|f| f.key.clone()).collect();
        let seen_before = std::mem::take(&mut pending);
        for f in found {
            if let Some(a) = active.get_mut(&f.key) {
                a.1 = f.v;
                continue;
            }
            let slow = matches!(f.v["kind"].as_str(), Some("wake_storm" | "bkl_busy"));
            if slow && !seen_before.contains(&f.key) {
                pending.insert(f.key);
                continue;
            }
            let mut v = f.v;
            v["t"] = json!(now());
            if let Some(tid) = v["tid"].as_u64()
                && let Some(p) = keep_stack(tid, v["kind"].as_str().unwrap_or("x"))
            {
                v["stack_file"] = json!(p);
            }
            log(&v);
            if rescue && v["kind"] == "lost_wakeup_futex"
                && let Some(pid) = v["pid"].as_u64()
            {
                let _ = std::fs::write("/proc/ai/ctl", format!("kick {}", pid));
                log(&json!({ "t": now(), "kind": "rescued", "pid": pid, "tid": v["tid"] }));
            }
            active.insert(f.key, (now(), v));
        }
        // 見えなくなったもの: 直った
        let gone: Vec<String> = active.keys().filter(|k| !keys.contains(*k)).cloned().collect();
        for k in gone {
            let (since, v) = active.remove(&k).unwrap();
            log(&json!({ "t": now(), "kind": "resolved", "was": v["kind"], "key": k, "lasted_s": now().saturating_sub(since) }));
        }
        let state = json!({ "t": now(), "interval_s": interval, "active": active.values().map(|(since, v)| { let mut v = v.clone(); v["since"] = json!(since); v }).collect::<Vec<_>>() });
        let tmp = format!("{}.tmp", STATE);
        if std::fs::write(&tmp, format!("{}\n", state)).is_ok() {
            let _ = std::fs::rename(&tmp, STATE);
        }
        if once && rounds >= 2 {
            println!("{}", state);
            return;
        }
        prev = cur;
    }
}
