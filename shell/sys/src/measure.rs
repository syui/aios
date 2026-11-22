// コマンドを 1 つ動かして測る: bkl (そのあいだの大きなロック、/proc/bkl) と strace (システムコールを kmsg から、/proc/strace)
use super::{aios, kmsg_text, read, run, timeout_of, write_root};
use aish_plugin::{Value, error, json, s};
use std::path::Path;

pub(crate) fn bkl(a: &Value, pwd: &str) -> Value {
    if !Path::new("/proc/bkl").exists() {
        return error("no /proc/bkl (not aios, or an older kernel)");
    }
    let cmd = a["cmd"].as_str().unwrap_or("");
    if cmd.is_empty() {
        return json!({ "text": read("/proc/bkl").unwrap_or_default() });
    }
    if let Err(e) = write_root("/proc/bkl", "\n") {
        return error(e);
    }
    match run(cmd, pwd, timeout_of(a)) {
        Ok((status, out, err, ms, to)) => {
            let mut r = json!({ "status": status, "ms": ms, "text": read("/proc/bkl").unwrap_or_default() });
            if to {
                r["timeout"] = json!(true);
            }
            if status != 0 {
                r["out"] = json!(out);
                r["err"] = json!(err);
            }
            r
        }
        Err(e) => error(e),
    }
}

pub(crate) fn strace(a: &Value, pwd: &str) -> Value {
    if !aios() || !Path::new("/proc/strace").exists() {
        return error("no /proc/strace or /proc/kmsg (not aios, or an older kernel)");
    }
    let cmd = s(a, "cmd");
    let name: String = match a["name"].as_str().filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => cmd.split_whitespace().next().unwrap_or("").rsplit('/').next().unwrap_or("").to_string(),
    };
    if name.is_empty() {
        return error("no command");
    }
    // カーネルの comm は 15 文字まで
    let name: String = name.chars().take(15).collect();
    let all = a["all"].as_bool().unwrap_or(true);
    let before = kmsg_text().map_or(0, |k| k.lines().count());
    if let Err(e) = write_root("/proc/strace", &format!("{}{}", if all { "+" } else { "" }, name)) {
        return error(e);
    }
    let res = run(cmd, pwd, timeout_of(a));
    let _ = write_root("/proc/strace", "");
    let k = kmsg_text().unwrap_or_default();
    let limit = a["limit"].as_u64().unwrap_or(300) as usize;
    // 前に読んだ行より後ろの strace の行 (kmsg があふれていれば、あるものぜんぶ)
    let lines: Vec<&str> = k.lines().skip(before.min(k.lines().count())).filter(|l| l.contains("strace [")).collect();
    let n = lines.len();
    let mut text = lines.iter().take(limit).map(|l| l.split_once("strace ").map_or(*l, |(_, r)| r)).collect::<Vec<_>>().join("\n");
    text.push('\n');
    match res {
        Ok((status, out, err, ms, to)) => {
            let mut r = json!({ "status": status, "ms": ms, "calls": n, "text": text });
            if n > limit {
                r["more"] = json!(n - limit);
            }
            if to {
                r["timeout"] = json!(true);
            }
            r["out"] = json!(out);
            r["err"] = json!(err);
            r
        }
        Err(e) => error(e),
    }
}
