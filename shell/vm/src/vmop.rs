// vm ツール: test/vm.py で VM を動かす (start stop status run job put get shot keys log mon reboot)。中のカーネルが
// panic していれば、時間切れと status の答えに panic (わけと、関数の名前にした backtrace)
use super::{cut, secs, vmpy};
use crate::jobs::{Job, job};
use crate::kernel::booted;
use aish_plugin::{Value, error, json, s};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub(crate) fn vm(jobs: &mut HashMap<String, Job>, root: &Path, a: &Value) -> Value {
    let op = s(a, "op");
    let short = Duration::from_secs(55);
    let res = |(st, out, err, ms, to): (i32, String, String, u64, bool)| {
        let mut r = json!({ "status": st, "out": out, "ms": ms });
        if !err.trim().is_empty() {
            r["err"] = json!(err.trim());
        }
        if to {
            r["timeout"] = json!(true);
        }
        r
    };
    match op {
        "start" => job(jobs, root, "start", a),
        "stop" => res(vmpy(root, &[op], &[], short)),
        "status" => with_panic(root, res(vmpy(root, &[op], &[], short))),
        "run" if a["bg"] == true => {
            // 中でうしろで (vm.py bg)。様子と出力は op job で
            let (st, out, err, ms, to) = vmpy(root, &["bg", s(a, "cmd")], &[], short);
            match aish_plugin::parse(out.trim()) {
                Some(mut r) if st == 0 => {
                    r["hint"] = json!("vm op job with this id shows its output and whether it ended (wait_ms to wait, kill to stop it)");
                    r
                }
                _ => res((st, out, err, ms, to)),
            }
        }
        "job" => {
            // うしろのものの様子と、前に見せたところから先の出力。Claude Code は 60 秒しか待たないので 40 秒まで待つ
            // (中で様子を見るのにも少しかかる)
            let w = a["wait_ms"].as_u64().unwrap_or(0).min(40_000) as f64 / 1000.0;
            let mut args = vec!["job".to_string()];
            if let Some(id) = a["id"].as_u64() {
                args.push(id.to_string());
            }
            args.extend(["-w".to_string(), format!("{}", w)]);
            if a["kill"] == true {
                args.push("--kill".into());
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let (st, out, err, ms, to) = vmpy(root, &argv, &[], Duration::from_secs_f64(w + 15.0));
            match aish_plugin::parse(out.trim()) {
                Some(mut r) if st == 0 => {
                    if let Some(o) = r["out"].as_str().map(String::from) {
                        r["out"] = json!(cut(root, symbolize(root, o)));
                    }
                    r
                }
                _ => with_panic(root, res((st, out, err, ms, to))),
            }
        }
        "run" => {
            let t = secs(a, "timeout", 50.0, 50.0);
            let (st, out, err, ms, to) = vmpy(root, &["run", s(a, "cmd"), "-t", &format!("{}", t)], &[], Duration::from_secs_f64(t + 5.0));
            // vm.py run は時間切れなら 124 (中のコマンドが終わらないか、VM が止まった)
            let r = res((st, cut(root, symbolize(root, out)), err, ms, to));
            if to || st == 124 { with_panic(root, r) } else { r }
        }
        "put" => {
            let local = s(a, "local");
            let name = Path::new(local).file_name().and_then(|n| n.to_str()).unwrap_or("file");
            let remote = a["remote"].as_str().filter(|r| !r.is_empty()).map(String::from).unwrap_or(format!("/tmp/{}", name));
            let mut r = res(vmpy(root, &["put", local, &remote], &[], short));
            r["remote"] = json!(remote);
            r
        }
        "get" => {
            // 中のファイルを外へ (vm.py get: gzip と base64 でシリアルから。sha256 を確かめる)。置き場の既定は build/aish-vm/get/名前
            let remote = s(a, "remote");
            if remote.is_empty() {
                return error("get: give remote (the file inside)");
            }
            let name = Path::new(remote).file_name().and_then(|n| n.to_str()).unwrap_or("file");
            let local = a["local"].as_str().filter(|l| !l.is_empty()).map(PathBuf::from).unwrap_or_else(|| root.join("build/aish-vm/get").join(name));
            let (st, out, err, ms, to) = vmpy(root, &["get", remote, &local.display().to_string()], &[], short);
            match aish_plugin::parse(out.trim()) {
                Some(mut r) if st == 0 => {
                    r["ms"] = json!(ms);
                    r
                }
                _ => res((st, out, err, ms, to)),
            }
        }
        "shot" => {
            let f = a["file"].as_str().filter(|f| !f.is_empty()).map(PathBuf::from).unwrap_or_else(|| root.join("build/aish-vm/shot.png"));
            let _ = std::fs::create_dir_all(f.parent().unwrap_or(Path::new(".")));
            let (st, out, err, ms, _) = vmpy(root, &["shot", &f.display().to_string()], &[], short);
            json!({ "status": st, "file": out.trim(), "err": err.trim(), "ms": ms })
        }
        "keys" => {
            let keys: Vec<String> = a["keys"].as_array().map(|k| k.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let mut args = vec!["keys"];
            args.extend(keys.iter().map(String::as_str));
            res(vmpy(root, &args, &[], short))
        }
        "log" => {
            let n = a["lines"].as_u64().unwrap_or(40);
            let g = s(a, "grep");
            // grep するなら多めに読んでから絞る
            let (st, out, err, ms, to) = vmpy(root, &["log", &format!("{}", if g.is_empty() { n } else { 20000 })], &[], short);
            let text = if g.is_empty() { out } else { out.lines().filter(|l| l.contains(g)).collect::<Vec<_>>().iter().rev().take(n as usize).rev().cloned().collect::<Vec<_>>().join("\n") };
            res((st, symbolize(root, text), err, ms, to))
        }
        "mon" => res(vmpy(root, &["mon", s(a, "cmd")], &[], short)),
        "reboot" => {
            // 中で sudo reboot してプロンプトまで待つ (Claude Code は 60 秒しか待たないので 50 秒まで)。
            // reset_after: その秒で来なければリセット (止まった新しいカーネルから、aiboot が前のもので起こしなおす)
            let t = secs(a, "timeout", 50.0, 50.0);
            let mut args = vec!["reboot".to_string(), "-t".into(), format!("{}", t)];
            if let Some(r) = a["reset_after"].as_f64() {
                args.extend(["--reset-after".into(), format!("{}", r)]);
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            res(vmpy(root, &argv, &[], Duration::from_secs_f64(t + 5.0)))
        }
        _ => error("op: start stop status run (bg) job put get shot keys log mon reboot"),
    }
}

/// カーネルの panic のバックトレース (backtrace: 0x... の行) があれば、addr2line で関数の名前にして足す
pub(crate) fn symbolize(root: &Path, text: String) -> String {
    let Some(line) = text.lines().find_map(|l| l.trim_start().strip_prefix("backtrace: 0x").map(|_| l.trim_start())) else { return text };
    let Ok(k) = std::fs::read_to_string(booted(root)) else { return text };
    let addrs: Vec<&str> = line["backtrace:".len()..].split_whitespace().collect();
    let Ok(o) = Command::new("addr2line").arg("-e").arg(k.trim()).args(["-f", "-C", "-p"]).args(&addrs).output() else { return text };
    let names: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().map(|l| format!("  {}", if l.ends_with(":?") { l.rsplit_once(" at ").map_or(l, |x| x.0) } else { l })).collect();
    if names.is_empty() {
        return text;
    }
    format!("{}\nbacktrace (addr2line):\n{}", text, names.join("\n"))
}

/// シリアルの終わりに、いまのカーネルの panic (行の頭が "panic: ") があれば、その行から 30 行 (わけと backtrace を
/// 関数の名前にしたもの)。そのあとに起動しなおした (aios init:) ものは古いので見ない。VM が答えないとき (run や job の
/// 時間切れ) と status の答えに足す: 止まったわけを log で探しにいかなくてよいように
pub(crate) fn panic_note(root: &Path) -> Option<String> {
    let (_, out, _, _, _) = vmpy(root, &["log", "400"], &[], Duration::from_secs(15));
    find_panic(&out).map(|t| symbolize(root, t))
}

/// シリアルの出力の中の、いちばん終わりの panic (その行から 30 行)。そのあとに起動しなおしていれば None
fn find_panic(log: &str) -> Option<String> {
    let lines: Vec<&str> = log.lines().collect();
    let i = lines.iter().rposition(|l| l.trim_start().starts_with("panic: "))?;
    if lines[i..].iter().any(|l| l.starts_with("aios init:")) {
        return None;
    }
    Some(lines[i..].iter().take(30).copied().collect::<Vec<_>>().join("\n"))
}

/// 答えに panic を足す (あれば)
fn with_panic(root: &Path, mut r: Value) -> Value {
    if let Some(p) = panic_note(root) {
        r["panic"] = json!(p);
        r["hint"] = json!("the kernel in the VM panicked (panic has the message and backtrace): vm op start boots it again");
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_in_log() {
        let log = "ok madvise\npid 3674 (madvise): killed by signal 11\npanic: panicked at kernel/src/extfs.rs:264:31:\nRefCell already borrowed\nbacktrace: 0xffffff80403c1158 0xffffff80403c0f38\n";
        let p = find_panic(log).unwrap();
        assert!(p.starts_with("panic: panicked at kernel/src/extfs.rs") && p.contains("RefCell already borrowed") && p.contains("backtrace: 0x"));
        // 起動しなおしたあとなら古い
        assert!(find_panic(&format!("{}aios init: 12 units, 9 enabled\n", log)).is_none());
        // ユーザーのプログラムの出力の中の panic という字は数えない
        assert!(find_panic("thread 'main' panicked at src/main.rs\nok\n").is_none());
    }
}
