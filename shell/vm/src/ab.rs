// ab: 2 つのカーネルで同じコマンドを n 回ずつ動かし、時間 (ms のまんなか、範囲) と大きなロックを持っていた割合をくらべる
use super::{note, secs, sub, vmpy};
use crate::kernel::{boot, kernel_for};
use aish_plugin::{Value, json, s};
use std::path::Path;
use std::time::Duration;

fn median(v: &[u64]) -> u64 {
    let mut s = v.to_vec();
    s.sort();
    s.get(s.len() / 2).copied().unwrap_or(0)
}

/// 大きなロックを持っていた割合 (/proc/ai/bkl、なければ /proc/bkl の all の行)
fn bkl_hold(root: &Path) -> Option<f64> {
    let (_, out, _, _, _) = vmpy(root, &["run", "sudo cat /proc/ai/bkl 2>/dev/null || grep '^all' /proc/bkl", "-t", "20"], &[], Duration::from_secs(30));
    if let Some(v) = out.lines().find_map(aish_plugin::parse) {
        return v["hold_pct"].as_f64();
    }
    // all  待った ms 待った%  持っていた ms 持っていた%
    let l = out.lines().find(|l| l.starts_with("all"))?;
    let pcts: Vec<f64> = l.split_whitespace().filter_map(|w| w.strip_suffix('%')?.parse().ok()).collect();
    pcts.get(1).copied()
}

pub(crate) fn job_ab(root: &Path, a: &Value) -> Result<Value, String> {
    let n = a["n"].as_u64().unwrap_or(5).clamp(1, 100) as usize;
    let cmd = s(a, "cmd");
    let t = Duration::from_secs_f64(secs(a, "timeout", 300.0, 3600.0));
    let specs = [a["a"].as_str().filter(|x| !x.is_empty()).unwrap_or("HEAD"), a["b"].as_str().filter(|x| !x.is_empty()).unwrap_or(".")];
    let mut res = Vec::new();
    for spec in specs {
        note(json!({ "step": "kernel", "spec": spec }));
        let k = kernel_for(root, spec)?;
        note(json!({ "step": "boot", "spec": spec }));
        boot(root, Some(&k), true, a["www"].as_str().unwrap_or(""), &[])?;
        let setup = s(a, "setup");
        if !setup.is_empty() {
            // setup は cd などを残すためのものなので、そのままのシェルで
            vmpy(root, &["run", setup, "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
        }
        vmpy(root, &["run", "sudo sh -c 'echo > /proc/bkl'", "-t", "20"], &[], Duration::from_secs(30));
        let (mut ms, mut fails) = (Vec::new(), 0);
        for i in 0..n {
            let (st, _, _, m, to) = vmpy(root, &["run", &sub(cmd), "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
            if st != 0 || to {
                fails += 1;
            }
            ms.push(m);
            note(json!({ "step": "run", "spec": spec, "i": i, "ms": m, "status": st }));
        }
        let hold = bkl_hold(root);
        res.push(json!({ "kernel": spec, "path": k.display().to_string(), "ms": ms, "median_ms": median(&ms), "min_ms": ms.iter().min(), "max_ms": ms.iter().max(), "failures": fails, "bkl_hold_pct": hold }));
    }
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let (m0, m1) = (res[0]["median_ms"].as_u64().unwrap_or(0) as f64, res[1]["median_ms"].as_u64().unwrap_or(0) as f64);
    let change = if m0 > 0.0 { (m1 - m0) / m0 * 100.0 } else { 0.0 };
    // はっきりした差か: いちばん短い〜長いの範囲が重ならない (QEMU の上の時間は 10% ほどぶれる)
    let range = |r: &Value| (r["min_ms"].as_u64().unwrap_or(0), r["max_ms"].as_u64().unwrap_or(0));
    let ((a0, a1), (b0, b1)) = (range(&res[0]), range(&res[1]));
    let clear = b1 < a0 || a1 < b0;
    let verdict = match (clear, m1 < m0) {
        (false, _) => "no clear difference (the ranges overlap; try a larger n or a longer cmd)",
        (true, true) => "b is faster",
        (true, false) => "b is slower",
    };
    Ok(json!({ "a": res[0], "b": res[1], "b_vs_a_pct": (change * 10.0).round() / 10.0, "clear": clear, "verdict": verdict, "note": "ms includes the serial round trip (about the same for both); the VM ran with -snapshot" }))
}
