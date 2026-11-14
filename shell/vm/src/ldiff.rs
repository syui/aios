// ldiff: 出すだけのテスト (test/NAME.c) を手本の Linux と aios の VM で動かしてくらべる (test/ldiff.sh) を、うしろで
// いくつでも続けて。答えはテストごとに same / differ (diff -u の頭) / error と、まとめだけ (進みぐあいの行は出さない)。
// linux_only なら aios では動かさず、Linux の出力を見る (作るテストの答えを先に。VM は要らない)。kernel は
// はじめのテストの前に 1 度だけ (そのカーネルで VM を起こしなおす。あとのテストはその VM で)
// 前は run で test/ldiff.sh を 1 つずつ動かし、Linux を作る数分や aios の長いテストで 60 秒を過ぎていた (2026-11-13)
use super::{note, sh};
use aish_plugin::{Value, json, s};
use std::path::Path;
use std::time::Duration;

/// diff などの答えに入れる行 (テストごと)
const MAX_LINES: usize = 40;

/// "NAME ARG..." のならび
fn tests(a: &Value) -> Vec<Vec<String>> {
    let mut t: Vec<Vec<String>> = a["tests"].as_array().map(|v| v.iter().filter_map(|x| x.as_str()).map(|x| x.split_whitespace().map(String::from).collect::<Vec<_>>()).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
    if !s(a, "name").is_empty() {
        let mut one = vec![s(a, "name").to_string()];
        one.extend(a["args"].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default());
        t.insert(0, one);
    }
    t
}

/// ldiff.sh の答えを短く: (same / differ / linux / error, 本文)
fn summary(code: i32, out: &str, err: &str, linux_only: bool) -> (&'static str, String) {
    let lines: Vec<&str> = out.lines().collect();
    let clip = |ls: &[&str]| -> String {
        if ls.len() <= MAX_LINES {
            return ls.join("\n");
        }
        format!("{}\n... ({} more lines)", ls[..MAX_LINES].join("\n"), ls.len() - MAX_LINES)
    };
    match code {
        0 if linux_only => ("linux", clip(&lines)),
        0 => ("same", lines.join("\n")),
        1 => ("differ", clip(&lines)),
        _ => {
            let e: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
            ("error", e[e.len().saturating_sub(5)..].join("\n"))
        }
    }
}

pub fn job(root: &Path, a: &Value) -> Result<Value, String> {
    let list = tests(a);
    if list.is_empty() {
        return Err("give tests (\"NAME ARG...\", test/NAME.c) or name".into());
    }
    let linux_only = a["linux_only"] == true;
    let mut flags: Vec<String> = Vec::new();
    for (k, f) in [("root", "--root"), ("net", "--net"), ("fresh", "--fresh"), ("linux_only", "--linux-only")] {
        if a[k] == true {
            flags.push(f.into());
        }
    }
    if !s(a, "linux").is_empty() {
        flags.extend(["--linux".to_string(), s(a, "linux").to_string()]);
    }
    let mut kernel = s(a, "kernel").to_string();
    let (mut same, mut differ, mut errors, mut text) = (Vec::new(), Vec::new(), Vec::new(), String::new());
    let mut seen: Vec<String> = Vec::new();
    for t in &list {
        let mut args: Vec<String> = vec!["test/ldiff.sh".into()];
        args.extend(flags.iter().cloned());
        if !kernel.is_empty() && !linux_only {
            args.extend(["--kernel".to_string(), std::mem::take(&mut kernel)]);
        }
        args.extend(t.iter().cloned());
        let argv: Vec<&str> = args.iter().map(|x| x.as_str()).collect();
        let (code, out, err, ms, to) = sh(root, "sh", &argv, &[], Duration::from_secs(3600));
        // 手本の Linux に足りない機能の知らせは、テストごとに同じなので終わりに 1 度だけ
        let (notes, out): (Vec<&str>, Vec<&str>) = out.lines().partition(|l| l.starts_with("note: "));
        for n in notes {
            if !seen.iter().any(|x| x == n) {
                seen.push(n.to_string());
            }
        }
        let out = out.join("\n");
        let (kind, body) = if to { ("error", "timed out (1 hour)".to_string()) } else { summary(code, &out, &err, linux_only) };
        let name = t.join(" ");
        note(json!({ "test": name, "result": kind, "ms": ms }));
        match kind {
            "same" => {
                same.push(name.clone());
                text.push_str(&format!("{}: {}\n", name, body));
            }
            "differ" => {
                differ.push(name.clone());
                text.push_str(&format!("{}:\n{}\n", name, body));
            }
            "linux" => text.push_str(&format!("{} (Linux):\n{}\n", name, body)),
            _ => {
                errors.push(name.clone());
                text.push_str(&format!("{}: error\n{}\n", name, body));
            }
        }
        // VM がないなど、あとのテストも同じに落ちるもの
        if kind == "error" && body.contains("no aios VM") {
            break;
        }
    }
    for n in &seen {
        text.push_str(n);
        text.push('\n');
    }
    let ok = differ.is_empty() && errors.is_empty();
    Ok(json!({ "text": text, "ok": ok, "same": same.len(), "differ": differ, "errors": errors, "files": "build/ldiff/NAME.linux, NAME.aios" }))
}

#[cfg(test)]
mod t {
    use super::*;

    #[test]
    fn lists() {
        let a = json!({ "tests": ["shm", "syncrange @home", "  "], "name": "ttou" });
        assert_eq!(tests(&a), vec![vec!["ttou"], vec!["shm"], vec!["syncrange", "@home"]]);
        let a = json!({ "name": "chosen", "args": ["@home", "/tmp"] });
        assert_eq!(tests(&a), vec![vec!["chosen", "@home", "/tmp"]]);
    }

    #[test]
    fn summaries() {
        assert_eq!(summary(0, "same: 3 lines (Linux 7.3 exit 0, aios exit 0)\n", "", false).0, "same");
        let (k, b) = summary(1, &"x\n".repeat(100), "", false);
        assert_eq!(k, "differ");
        assert!(b.ends_with("(60 more lines)"));
        let (k, b) = summary(2, "", "ldiff: building\nldiff: no aios VM with test/ served\n", false);
        assert_eq!(k, "error");
        assert!(b.contains("no aios VM"));
        assert_eq!(summary(0, "a\nlinux only: Linux 7.3 exit 0\n", "", true).0, "linux");
    }
}
