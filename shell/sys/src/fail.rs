// fail: わざとしくじらせる (/proc/ai/fail、ctl の fail)。cmd のプロセスのシステムコールを -ERRNO に、カーネルのヒープや
// ページを取れないことにして、どう耐えるかを見る
use super::{ai_json, run, tail, timeout_of, write_root};
use aish_plugin::{Value, error, json, s};
use std::path::Path;

/// errno の名前 → 番号 (Linux の asm-generic)
pub(crate) fn errno_of(s: &str) -> Option<i64> {
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    let t = [
        ("EPERM", 1), ("ENOENT", 2), ("EIO", 5), ("ENXIO", 6), ("EBADF", 9), ("EAGAIN", 11), ("ENOMEM", 12), ("EACCES", 13), ("EFAULT", 14),
        ("EBUSY", 16), ("EEXIST", 17), ("ENODEV", 19), ("ENOTDIR", 20), ("EISDIR", 21), ("EINVAL", 22), ("ENFILE", 23), ("EMFILE", 24),
        ("EFBIG", 27), ("ENOSPC", 28), ("ESPIPE", 29), ("EROFS", 30), ("EPIPE", 32), ("ENOSYS", 38), ("ELOOP", 40), ("EOVERFLOW", 75),
        ("EOPNOTSUPP", 95), ("EADDRINUSE", 98), ("ENETUNREACH", 101), ("ECONNRESET", 104), ("ETIMEDOUT", 110), ("ECONNREFUSED", 111),
        ("EHOSTUNREACH", 113), ("EINTR", 4),
    ];
    t.iter().find(|(n, _)| n.eq_ignore_ascii_case(s)).map(|(_, v)| *v)
}

pub(crate) fn fail(a: &Value, pwd: &str) -> Value {
    if !Path::new("/proc/ai/fail").exists() {
        return error("no /proc/ai/fail (not aios, or an older kernel)");
    }
    if a["off"].as_bool().unwrap_or(false) {
        return match write_root("/proc/ai/ctl", "fail off") {
            Ok(()) => json!({ "off": true }),
            Err(e) => error(e),
        };
    }
    let cmd = s(a, "cmd");
    let target = match (a["pid"].as_u64(), s(a, "name")) {
        (Some(p), _) => format!("pid {}", p),
        (None, n) if !n.is_empty() => format!("name {}", n),
        (None, _) if !cmd.is_empty() => format!("name {}", Path::new(cmd.split_whitespace().next().unwrap_or("")).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())),
        _ => return error("name, pid or cmd is needed"),
    };
    let what = if s(a, "what").is_empty() { "sys" } else { s(a, "what") };
    let times = a["times"].as_u64().unwrap_or(1);
    let rule = match what {
        "sys" => {
            let sc = if s(a, "syscall").is_empty() { "all" } else { s(a, "syscall") };
            let Some(e) = errno_of(if s(a, "errno").is_empty() { "ENOMEM" } else { s(a, "errno") }) else { return error("errno: a name (ENOMEM, EIO ...) or a number") };
            format!("fail {} sys {} {} {}", target, sc, e, times)
        }
        "heap" | "oom" | "page" => format!("fail {} {} {}", target, what, times),
        w => return error(format!("what: sys / heap / oom / page, not {}", w)),
    };
    if let Err(e) = write_root("/proc/ai/ctl", &rule) {
        return error(format!("{} ({})", e, rule));
    }
    if cmd.is_empty() {
        return json!({ "rule": rule, "state": ai_json("/proc/ai/fail").unwrap_or(Value::Null) });
    }
    let r = run(cmd, pwd, timeout_of(a));
    let state = ai_json("/proc/ai/fail").unwrap_or(Value::Null);
    let _ = write_root("/proc/ai/ctl", "fail off");
    match r {
        Ok((st, out, err, ms, timeout)) => json!({ "rule": rule, "hits": state["rule"]["hits"], "status": st, "out": tail(&out, "", 40), "err": tail(&err, "", 40), "ms": ms, "timeout": timeout }),
        Err(e) => error(e),
    }
}
