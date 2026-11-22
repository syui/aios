// box: カーネルの砂場。kernel.json の off のあるもの (extra、quirk と core の ns・vdso) を起動の引数 aios.off= で切って
// 起こし、test (既定は CI と同じ vm-check と vm-ctests) を動かして、何も切らないときと FAIL の行をくらべる
use super::{note, secs, sh, strings, sub, vmpy};
use crate::kernel::{boot, kernel_for};
use aish_plugin::{Value, json, s};
use std::path::Path;
use std::time::{Duration, Instant};

/// 切れるもの: kernel/kernel.json の off のある名前 (kernel/src/parts.rs の NAMES と同じ。test/kernel-json.py が確かめる)。
/// 並びは NAMES の順 (/proc/ai/parts と起動のときの「parts: off」と同じ。kernel.json は core が先なので、core の ns と
/// vdso が先に来てしまう)
pub(crate) fn switchable(root: &Path) -> Result<Vec<String>, String> {
    let t = std::fs::read_to_string(root.join("kernel/kernel.json")).map_err(|e| format!("kernel/kernel.json: {}", e))?;
    let v = aish_plugin::parse(&t).ok_or("kernel/kernel.json: not JSON")?;
    let mut names: Vec<String> = v.as_array().map(|a| a.iter().filter(|e| e.get("off").is_some()).filter_map(|e| e["name"].as_str().map(String::from)).collect()).unwrap_or_default();
    let parts = std::fs::read_to_string(root.join("kernel/src/parts.rs")).unwrap_or_default();
    let order: Vec<&str> = parts.split("const NAMES").nth(1).and_then(|s| s.split("];").next()).map(|s| s.split('"').skip(1).step_by(2).collect()).unwrap_or_default();
    names.sort_by_key(|n| order.iter().position(|o| o == n).unwrap_or(usize::MAX));
    Ok(names)
}

/// CI と同じテスト (test/vm-check.sh と test/vm-ctests.sh)。C のテストは中で動かす静的なプログラムに作っておく
const DEFAULT_TEST: &str = "fetch http://10.0.2.2:8000/vm-check.sh > /tmp/vm-check.sh && sh /tmp/vm-check.sh; a=$?; \
fetch http://10.0.2.2:8000/vm-ctests.sh > /tmp/vm-ctests.sh && sh /tmp/vm-ctests.sh run; b=$?; exit $((a + b))";

/// test/bin/NAME を作る (names が空なら vm-ctests.sh の TESTS みな)
pub(crate) fn build_ctests(root: &Path, names: &[String]) -> Result<(), String> {
    let cc = ["aarch64-linux-gnu-gcc", "gcc"].into_iter().find(|c| {
        let (st, out, _, _, _) = sh(root, c, &["-dumpmachine"], &[], Duration::from_secs(10));
        st == 0 && out.starts_with("aarch64")
    });
    let Some(cc) = cc else { return Err("no aarch64 C compiler for the C tests (aarch64-linux-gnu-gcc)".into()) };
    let mut args = vec!["test/vm-ctests.sh", "build", cc];
    args.extend(names.iter().map(String::as_str));
    let (st, _, err, _, _) = sh(root, "sh", &args, &[], Duration::from_secs(600));
    if st != 0 {
        return Err(format!("test/vm-ctests.sh build: {}", err.lines().last().unwrap_or("")));
    }
    Ok(())
}

pub(crate) fn job_box(root: &Path, a: &Value) -> Result<Value, String> {
    let names = switchable(root)?;
    let sets: Vec<Vec<String>> = if a["each"] == true { names.iter().map(|n| vec![n.clone()]).collect() } else { vec![strings(&a["off"])] };
    if sets.iter().all(|s| s.is_empty()) {
        return Err(format!("give off (or each): {}", names.join(" ")));
    }
    if let Some(n) = sets.iter().flatten().find(|n| !names.contains(n)) {
        return Err(format!("{} cannot be turned off (kernel.json: {})", n, names.join(" ")));
    }
    let custom = s(a, "test");
    let test = if custom.is_empty() { DEFAULT_TEST } else { custom };
    let www = match a["www"].as_str().filter(|w| !w.is_empty()) {
        Some(w) => w.to_string(),
        None if custom.is_empty() => "test".to_string(),
        None => String::new(),
    };
    if custom.is_empty() {
        note(json!({ "step": "ctests" }));
        build_ctests(root, &[])?;
    }
    let spec = a["kernel"].as_str().filter(|x| !x.is_empty()).unwrap_or(".");
    note(json!({ "step": "kernel", "spec": spec }));
    let k = kernel_for(root, spec)?;
    let t = Duration::from_secs_f64(secs(a, "timeout", 1200.0, 3600.0));
    // 1 つの組: 起こして (起きなければそれが答え)、切れているかを確かめ、test を動かして FAIL の行を集める
    let run = |off: &[String]| -> Value {
        note(json!({ "step": "boot", "off": off }));
        let t0 = Instant::now();
        if let Err(e) = boot(root, Some(&k), true, &www, off) {
            return json!({ "off": off, "boot": false, "error": e });
        }
        let (_, kmsg, _, _, _) = vmpy(root, &["run", "grep -a 'parts:' /proc/kmsg", "-t", "20"], &[], Duration::from_secs(30));
        let parts = kmsg.lines().find(|l| l.contains("parts: off")).map(|l| l.trim().to_string());
        if !off.is_empty() && parts.is_none() {
            return json!({ "off": off, "boot": true, "error": "the kernel did not say parts: off (aios.off= did not reach it)" });
        }
        let (st, out, _, ms, to) = vmpy(root, &["run", &sub(test), "-t", &format!("{}", t.as_secs())], &[], t + Duration::from_secs(10));
        // FAIL の行 (字下げしたものは、その中の様子なので数えない)
        let fails: Vec<String> = out.lines().filter(|l| l.starts_with("FAIL")).map(|l| l.trim_end().trim_end_matches(':').to_string()).collect();
        let last: Vec<&str> = out.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
        note(json!({ "step": "test", "off": off, "status": st, "fails": fails.len(), "ms": ms }));
        json!({ "off": off, "boot": true, "parts": parts, "status": st, "timeout": to, "fails": fails, "last": last.join(" / "), "ms": t0.elapsed().as_millis() as u64 })
    };
    let base = (a["baseline"] != false).then(|| run(&[]));
    let base_fails: Vec<String> = base.as_ref().map(|b| strings(&b["fails"])).unwrap_or_default();
    let base_status = base.as_ref().and_then(|b| b["status"].as_i64()).unwrap_or(0);
    let mut runs = Vec::new();
    let mut table = Vec::new();
    for set in &sets {
        let mut r = run(set);
        let fails = strings(&r["fails"]);
        let new: Vec<&String> = fails.iter().filter(|f| !base_fails.contains(f)).collect();
        let gone: Vec<&String> = base_fails.iter().filter(|f| !fails.contains(f)).collect();
        let line = if r["boot"] == false || r.get("error").is_some() {
            format!("{}: {}", set.join(","), r["error"].as_str().unwrap_or("did not come up"))
        } else if r["timeout"] == true {
            format!("{}: the test timed out", set.join(","))
        } else if new.is_empty() && r["status"].as_i64().unwrap_or(0) != base_status {
            // FAIL の行はないが、終わりの番号がちがう (テストが始められなかったなど)
            format!("{}: the test exited {} (with nothing off {}) without a new FAIL line: {}", set.join(","), r["status"], base_status, r["last"].as_str().unwrap_or(""))
        } else if new.is_empty() && (base.is_some() || fails.is_empty()) {
            format!("{}: nothing new fails", set.join(","))
        } else {
            format!("{}: {} new: {}", set.join(","), new.len(), new.iter().map(|f| f.as_str()).collect::<Vec<_>>().join("; "))
        };
        r["new_fails"] = json!(new);
        if !gone.is_empty() {
            r["passes_only_off"] = json!(gone);
        }
        table.push(line);
        runs.push(r);
    }
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let mut out = json!({ "kernel": spec, "path": k.display().to_string(), "results": table, "runs": runs, "note": "each ran on a fresh -snapshot VM; new means a FAIL line the run with nothing off does not have" });
    if let Some(b) = base {
        out["baseline"] = b;
    }
    if custom.is_empty() && sets.iter().any(|s| s.iter().any(|n| n == "net")) {
        out["hint"] = json!("the default test fetches its scripts over the network, so with net off it cannot start; give a test that needs no network");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switchable_in_parts_order() {
        // kernel.json は core (ns、vdso) が先だが、切れるものは parts.rs の NAMES の順 (/proc/ai/parts と同じ)
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let names = switchable(&root).unwrap();
        let parts = std::fs::read_to_string(root.join("kernel/src/parts.rs")).unwrap();
        let order: Vec<&str> = parts.split("const NAMES").nth(1).unwrap().split("];").next().unwrap().split('"').skip(1).step_by(2).collect();
        assert_eq!(names, order);
        assert_eq!(names.first().map(String::as_str), Some("aiproc"));
    }
}
