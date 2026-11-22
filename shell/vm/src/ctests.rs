// ctests: カーネルの C のテスト (test/vm-ctests.sh。CI の VM test と同じもの) を、名前を選んで VM で動かす。
// 外で test/bin/NAME を作り (古いものだけ)、test/ を http で出している VM (なければ、kernel か作業中のソースで
// 起こす) の中で vm-ctests.sh run NAME... を動かし、ok の数と、FAIL したもの (終わりの番号と、そのテストの出力の
// 終わり) だけを返す。時間切れで VM が答えないなら、中のカーネルの panic も
// 前は vm-ctests.sh の TESTS を sed で書きかえて、外で gcc を呼んでいた (2026-11-22)
use super::{boot, build_ctests, kernel_for, note, panic_note, secs, vmpy};
use aish_plugin::{Value, json, s};
use std::path::Path;
use std::time::Duration;

/// 動かすテストの名前 (tests か name。なければ空: vm-ctests.sh の TESTS みな)
fn names(a: &Value) -> Vec<String> {
    let mut v: Vec<String> = a["tests"].as_array().map(|t| t.iter().filter_map(|x| x.as_str()).flat_map(|x| x.split_whitespace().map(String::from).collect::<Vec<_>>()).collect()).unwrap_or_default();
    if !s(a, "name").is_empty() {
        v.insert(0, s(a, "name").to_string());
    }
    v
}

/// 1 つのテストの答え
#[derive(Debug, PartialEq)]
struct Fail {
    name: String,
    status: String,
    tail: String,
}

/// vm-ctests.sh run の出力から: ok の名前と、FAIL したもの (FAIL NAME (status N): のあとの字下げした行がその出力)
fn parse(out: &str) -> (Vec<String>, Vec<Fail>, Option<String>) {
    let (mut ok, mut fails, mut total) = (Vec::new(), Vec::<Fail>::new(), None);
    for l in out.lines() {
        if let Some(n) = l.strip_prefix("ok ") {
            ok.push(n.trim().to_string());
        } else if let Some(rest) = l.strip_prefix("FAIL ") {
            let rest = rest.trim_end().trim_end_matches(':');
            let (name, status) = match rest.split_once(" (status ") {
                Some((n, st)) => (n.to_string(), st.trim_end_matches(')').to_string()),
                None => match rest.split_once(": ") {
                    Some((n, why)) => (n.to_string(), why.to_string()),
                    None => (rest.to_string(), String::new()),
                },
            };
            fails.push(Fail { name, status, tail: String::new() });
        } else if let Some(t) = l.strip_prefix("  ") {
            if let Some(f) = fails.last_mut() {
                if !f.tail.is_empty() {
                    f.tail.push('\n');
                }
                f.tail.push_str(t);
            }
        } else if l.starts_with("ctests: ") {
            total = Some(l.trim().to_string());
        }
    }
    (ok, fails, total)
}

/// VM が test/ を http で出しているか (vm.py status の www)
fn serving(root: &Path) -> bool {
    let (_, out, _, _, _) = vmpy(root, &["status"], &[], Duration::from_secs(15));
    let test = root.join("test");
    aish_plugin::parse(out.trim()).is_some_and(|v| v["ready"] == true && v["www"].as_str().is_some_and(|w| Path::new(w) == test || w.ends_with("/test")))
}

pub fn job(root: &Path, a: &Value) -> Result<Value, String> {
    let list = names(a);
    if let Some(n) = list.iter().find(|n| !root.join("test").join(format!("{}.c", n)).is_file()) {
        return Err(format!("no test/{}.c", n));
    }
    note(json!({ "step": "build", "tests": list.len() }));
    build_ctests(root, &list)?;
    let spec = s(a, "kernel");
    if !spec.is_empty() || !serving(root) {
        // カーネルを選んだか、test/ を出している VM がない: そのカーネル (なければ作業中のソース) で起こしなおす
        let spec = if spec.is_empty() { "." } else { spec };
        note(json!({ "step": "kernel", "spec": spec }));
        let k = kernel_for(root, spec)?;
        note(json!({ "step": "boot" }));
        boot(root, Some(&k), true, "test", &[])?;
    }
    let t = secs(a, "timeout", 1200.0, 3600.0);
    let cmd = format!("fetch http://10.0.2.2:8000/vm-ctests.sh > /tmp/vm-ctests.sh && sh /tmp/vm-ctests.sh run {}", list.join(" "));
    note(json!({ "step": "run" }));
    let (st, out, _, ms, to) = vmpy(root, &["run", &cmd, "-t", &format!("{}", t)], &[], Duration::from_secs_f64(t + 15.0));
    let (ok, fails, total) = parse(&out);
    let failed: Vec<Value> = fails.iter().map(|f| json!({ "name": f.name, "status": f.status, "tail": f.tail })).collect();
    let mut r = json!({ "ok": ok.len(), "failed": failed, "ms": ms });
    if let Some(t) = total {
        r["summary"] = json!(t);
    }
    if to || st == 124 {
        r["timeout"] = json!(true);
        // どこまで進んだか (最後の ok のあと)
        r["last_ok"] = json!(ok.last());
        if let Some(p) = panic_note(root) {
            r["panic"] = json!(p);
        }
    }
    r["pass"] = json!(fails.is_empty() && !(to || st == 124) && st == 0);
    Ok(r)
}

#[cfg(test)]
mod t {
    use super::*;

    #[test]
    fn lists() {
        assert_eq!(names(&json!({ "tests": ["atime chattr", "ptrace"], "name": "openfast" })), vec!["openfast", "atime", "chattr", "ptrace"]);
        assert!(names(&json!({})).is_empty());
    }

    #[test]
    fn output() {
        let out = "ok atime\nFAIL chattr (status 1):\n  FAIL +i write\n  chattr: 1 failed\nok ptrace\nFAIL x: cannot fetch\nctests: 2 failed\n";
        let (ok, fails, total) = parse(out);
        assert_eq!(ok, vec!["atime", "ptrace"]);
        assert_eq!(fails[0], Fail { name: "chattr".into(), status: "1".into(), tail: "FAIL +i write\nchattr: 1 failed".into() });
        assert_eq!(fails[1], Fail { name: "x".into(), status: "cannot fetch".into(), tail: String::new() });
        assert_eq!(total.as_deref(), Some("ctests: 2 failed"));
    }
}
