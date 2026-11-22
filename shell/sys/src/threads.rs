// /proc/ai/threads を読むもの: threads (眠っているスレッドが何をどれだけ待っているか)、hang (プロセスがなぜ止まって
// いるか)、stack (呼び出しの並び: unwind.rs)、watch (aiwatch が見つけたもの)
use super::{json_lines, read, read_root, write_root};
use crate::unwind;
use aish_plugin::{Value, error, json, s};
use std::path::Path;
use std::time::Duration;

fn ai_threads() -> Result<Vec<Value>, String> {
    if !Path::new("/proc/ai/threads").exists() {
        return Err("no /proc/ai (not aios, or an older kernel)".into());
    }
    json_lines("/proc/ai/threads")
}

/// プロセス pid の fd (/proc/ai/fd/PID): fd 番号 → その行
fn ai_fds(pid: u64) -> std::collections::BTreeMap<u64, Value> {
    json_lines(&format!("/proc/ai/fd/{}", pid)).unwrap_or_default().into_iter().filter_map(|v| Some((v["fd"].as_u64()?, v))).collect()
}

/// wait の poll の fd 番号を、その fd の様子に置きかえる
fn annotate(t: &mut Value, fds: &std::collections::BTreeMap<u64, Value>) {
    if let Some(list) = t["wait"]["poll"].as_array() {
        let v: Vec<Value> = list.iter().map(|n| n.as_u64().and_then(|n| fds.get(&n).cloned()).unwrap_or(n.clone())).collect();
        t["wait"]["poll"] = json!(v);
    }
}

pub(crate) fn threads(a: &Value) -> Value {
    let ts = match ai_threads() {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    // プロセスの名前 (代表スレッドの名前) も名前で絞るときに見る
    let leaders: std::collections::BTreeMap<u64, String> = ts.iter().filter(|t| t["tid"] == t["pid"]).map(|t| (t["pid"].as_u64().unwrap_or(0), s(t, "name").to_string())).collect();
    let name = s(a, "name");
    let state = s(a, "state");
    let min = a["min_slept_s"].as_f64().unwrap_or(0.0);
    let limit = a["limit"].as_u64().unwrap_or(50) as usize;
    let mut sel: Vec<Value> = ts
        .into_iter()
        .filter(|t| a["pid"].as_u64().is_none_or(|p| t["pid"].as_u64() == Some(p)))
        .filter(|t| name.is_empty() || s(t, "name").contains(name) || leaders.get(&t["pid"].as_u64().unwrap_or(0)).is_some_and(|l| l.contains(name)))
        .filter(|t| state.is_empty() || s(t, "state") == state)
        .filter(|t| min <= 0.0 || t["slept_s"].as_f64().is_some_and(|x| x >= min))
        .collect();
    let total = sel.len();
    sel.truncate(limit);
    let mut cache = std::collections::BTreeMap::new();
    for t in sel.iter_mut() {
        if t["wait"]["poll"].is_array() {
            let pid = t["pid"].as_u64().unwrap_or(0);
            let fds = cache.entry(pid).or_insert_with(|| ai_fds(pid));
            annotate(t, fds);
        }
    }
    json!({ "count": total, "threads": sel })
}

pub(crate) fn hang(a: &Value) -> Value {
    let ts = match ai_threads() {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    let pids: Vec<u64> = match (a["pid"].as_u64(), s(a, "name")) {
        (Some(p), _) => vec![p],
        (None, n) if !n.is_empty() => ts.iter().filter(|t| t["tid"] == t["pid"] && s(t, "name").contains(n)).filter_map(|t| t["pid"].as_u64()).collect(),
        // どれとも言われなければ、みなを起こさずに見て、あやしいもの (hints のあるもの) だけを短く
        _ => {
            let me = std::process::id() as u64;
            let all: Vec<u64> = ts.iter().filter(|t| t["tid"] == t["pid"]).filter_map(|t| t["pid"].as_u64()).filter(|&p| p != me).collect();
            let found: Vec<Value> = all
                .iter()
                .map(|&p| hang_one(p, &ts, &[], false, false))
                .filter(|r| r["hints"].as_array().is_some_and(|h| !h.is_empty()))
                .map(|r| json!({ "pid": r["pid"], "name": r["name"], "hints": r["hints"] }))
                .collect();
            return json!({ "checked": all.len(), "suspects": found, "hint": "pid か name をつけると、そのプロセスのスレッドと待っているもの、kick (起こしてみる) と stack まで見る" });
        }
    };
    if pids.is_empty() {
        return error("no such process");
    }
    // 起こしてみる (futex で眠っているもの)。少し待って読みなおし、同じ futex で眠りなおしたかを見る
    let kick = a["kick"].as_bool().unwrap_or(true);
    let mut after: Vec<Value> = Vec::new();
    if kick {
        for p in &pids {
            if ts.iter().any(|t| t["pid"].as_u64() == Some(*p) && t["wait"]["futex"].is_u64()) {
                let cmd = format!("kick {}", p);
                if std::fs::write("/proc/ai/ctl", &cmd).is_err() {
                    let _ = write_root("/proc/ai/ctl", &cmd);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        after = ai_threads().unwrap_or_default();
    }
    let procs: Vec<Value> = pids.iter().map(|&pid| hang_one(pid, &ts, &after, kick, a["stack"].as_bool().unwrap_or(false))).collect();
    json!({ "procs": procs })
}

fn hang_one(pid: u64, ts: &[Value], after: &[Value], kick: bool, stack: bool) -> Value {
    let fds = ai_fds(pid);
    let mut mine: Vec<Value> = ts.iter().filter(|t| t["pid"].as_u64() == Some(pid)).cloned().collect();
    let name = mine.iter().find(|t| t["tid"].as_u64() == Some(pid)).map_or(String::new(), |t| s(t, "name").to_string());
    let mut hints: Vec<String> = Vec::new();
    // 読まれずに残っているもの (眠っている人がそれを見張っていないなら、起こしが消えたかもしれない)
    let pending: Vec<Value> = fds.values().filter(|f| f["pending"].as_u64().is_some_and(|n| n > 0)).cloned().collect();
    let watched: std::collections::BTreeSet<u64> = mine.iter().filter_map(|t| t["wait"]["poll"].as_array()).flatten().filter_map(|n| n.as_u64()).collect();
    for f in &pending {
        let n = f["fd"].as_u64().unwrap_or(0);
        if watched.contains(&n) {
            hints.push(format!("fd {} ({}) に読まれていないデータがあるのに、それを見張って眠っているスレッドがいる: 起こしが消えたかも", n, s(f, "what")));
        }
    }
    // 長く眠っているものから
    mine.sort_by(|x, y| y["slept_s"].as_f64().unwrap_or(-1.0).partial_cmp(&x["slept_s"].as_f64().unwrap_or(-1.0)).unwrap_or(std::cmp::Ordering::Equal));
    for t in mine.iter_mut() {
        annotate(t, &fds);
        let tid = t["tid"].as_u64().unwrap_or(0);
        if let Some(addr) = t["wait"]["futex"].as_u64() {
            let unchanged = t["wait"]["val"] == t["wait"]["now"];
            if kick {
                let again = after.iter().find(|x| x["tid"].as_u64() == Some(tid));
                let same = again.is_some_and(|x| x["wait"]["futex"].as_u64() == Some(addr) && x["state"] == "sleep");
                t["kick"] = json!(if same { "眠りなおした (同じ futex)" } else { "起きて進んだ" });
                if !same && t["slept_s"].as_f64().is_some_and(|x| x > 1.0) {
                    hints.push(format!("tid {} ({}) は起こすと進んだ: futex の起こしが消えていたかも", tid, s(t, "name")));
                }
            }
            if unchanged && t["slept_s"].as_f64().is_some_and(|x| x > 5.0) && tid == pid {
                hints.push(format!("代表スレッド {} が futex {:#x} で {:.0} 秒眠っていて、値は変わっていない: だれも知らせていない (ほかのスレッドかプロセスの返事を待っている。stack: true でどこで待っているか)", tid, addr, t["slept_s"].as_f64().unwrap_or(0.0)));
            }
        }
    }
    let busy: Vec<String> = mine.iter().filter(|t| t["state"] == "run" || t["state"] == "ready").map(|t| format!("{} {}", t["tid"], s(t, "name"))).collect();
    if !busy.is_empty() && mine.iter().any(|t| t["slept_s"].as_f64().is_some_and(|x| x > 5.0)) {
        hints.push(format!("走っているもの: {}。CPU が足りないか、大きなロックが混んでいるかもしれない (bkl で)", busy.join(", ")));
    }
    let mut r = json!({ "pid": pid, "name": name, "threads": mine, "pending_fds": pending, "hints": hints });
    if stack {
        // 長く眠っているものから 4 つ (mine はもう長い順)
        let tids: Vec<u64> = mine.iter().filter(|t| t["slept_s"].as_f64().is_some_and(|x| x > 1.0)).filter_map(|t| t["tid"].as_u64()).take(4).collect();
        r["stacks"] = json!(tids.iter().map(|&t| json!({ "tid": t, "frames": thread_frames(t, 30) })).collect::<Vec<_>>());
    }
    r
}

/// tid の呼び出しの並び (/proc/ai/stack/TID から)
fn thread_frames(tid: u64, max: usize) -> Value {
    match read_root(&format!("/proc/ai/stack/{}", tid)).map(|t| serde_json::from_str::<Value>(t.trim())) {
        Ok(Ok(st)) => json!(unwind::frames(&st, max)),
        Ok(Err(e)) => json!(format!("/proc/ai/stack/{}: {}", tid, e)),
        Err(e) => json!(e),
    }
}

pub(crate) fn stack(a: &Value) -> Value {
    let max = a["max"].as_u64().unwrap_or(40) as usize;
    if let Some(f) = a["file"].as_str().filter(|f| !f.is_empty()) {
        // 残しておいたもの: 写していたファイルが同じなら、プロセスが終わっていてもたどれる
        return match std::fs::read_to_string(f).map(|t| serde_json::from_str::<Value>(t.trim())) {
            Ok(Ok(st)) => json!({ "tid": st["tid"], "name": st["name"], "frames": unwind::frames(&st, max) }),
            Ok(Err(e)) => error(format!("{}: {}", f, e)),
            Err(e) => error(format!("{}: {}", f, e)),
        };
    }
    if let Some(t) = a["tid"].as_u64() {
        return json!({ "tid": t, "frames": thread_frames(t, max) });
    }
    let Some(pid) = a["pid"].as_u64() else { return error("tid か pid を") };
    let ts = match ai_threads() {
        Ok(t) => t,
        Err(e) => return error(e),
    };
    let mut mine: Vec<&Value> = ts.iter().filter(|t| t["pid"].as_u64() == Some(pid)).collect();
    mine.sort_by(|x, y| y["slept_s"].as_f64().unwrap_or(-1.0).partial_cmp(&x["slept_s"].as_f64().unwrap_or(-1.0)).unwrap_or(std::cmp::Ordering::Equal));
    let limit = a["limit"].as_u64().unwrap_or(8) as usize;
    let out: Vec<Value> = mine.iter().take(limit).map(|t| json!({ "tid": t["tid"], "name": t["name"], "state": t["state"], "wait": t["wait"], "frames": thread_frames(t["tid"].as_u64().unwrap_or(0), max) })).collect();
    json!({ "pid": pid, "threads": out })
}

pub(crate) fn watch(a: &Value) -> Value {
    let n = a["lines"].as_u64().unwrap_or(20) as usize;
    let state: Value = read("/run/aiwatch.json").and_then(|t| serde_json::from_str(t.trim()).ok()).unwrap_or(Value::Null);
    if state.is_null() {
        return error("no /run/aiwatch.json (aiwatch is not running: systemctl start aiwatch)");
    }
    let log = read("/var/log/aiwatch.service.log").unwrap_or_default();
    let all: Vec<Value> = log.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let recent: Vec<Value> = all.iter().skip(all.len().saturating_sub(n)).cloned().collect();
    json!({ "active": state["active"], "checked_at": state["t"], "interval_s": state["interval_s"], "log": recent })
}
