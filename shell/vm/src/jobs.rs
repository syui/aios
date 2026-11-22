// うしろで動かすもの: 自分を aish-vm KEY で起こし、出力は build/aish-vm/KEY.log、pid は KEY.pid。aish やこのプラグインが
// 入れかわっても、次に呼ばれたときに KEY.pid から引きついで続きを待つ
use aish_plugin::{Value, error, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// うしろで動いているもの (自分を aish-vm start / ab / bisect で起こしたもの)。出力は build/aish-vm/KEY.log
/// うしろで動いているもの (自分を aish-vm KEY で起こしたもの)。出力は build/aish-vm/KEY.log、pid は KEY.pid。
/// aish やこのプラグインが入れかわっても (aish-mcp.sh --build のたびに入れかわる)、次に呼ばれたときに KEY.pid から
/// 引きついで続きを待つ (前は忘れてしまい、もういちど呼ぶと同じ仕事をもう 1 つ起こした。ldiff の 2 つめが VM を
/// 起こしなおし、1 つめのテストの VM を止めた。2026-11-13。aish-pkg の job と同じ)
pub(crate) struct Job {
    /// 自分で起こした子。入れかわる前に起こしたもの (pid だけわかる) は None
    child: Option<std::process::Child>,
    pid: i32,
    log: PathBuf,
    t0: Instant,
    /// 最後に様子を見たとき
    polled: Instant,
}

impl Job {
    /// 終わったら Some(終わりの番号。引きついだものは None (わからない))
    fn done(&mut self) -> Option<Option<std::process::ExitStatus>> {
        match &mut self.child {
            Some(c) => match c.try_wait() {
                Ok(None) => None,
                Ok(st) => Some(st),
                Err(_) => Some(None),
            },
            None => (!alive(self.pid)).then_some(None),
        }
    }
}

/// pid が動いているか (終わって親に拾われていないもの (ゾンビ) は、終わったとみる)
fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{}/stat", pid)).is_ok_and(|s| s.rsplit_once(')').is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z')))
}

/// 前のプラグインが起こした key のもの。動いていれば、aish-vm KEY のときだけ (pid が別のものに使われていないか。
/// 引数の JSON は wait_ms などで変わるので見ない)。終わっていても KEY.pid が残っていれば、答えをまだ渡していないので
/// 引きつぐ (答えは log の終わりの JSON)。ただし終わってから 5 分より前のものは、新しく始める
fn adopt(dir: &Path, key: &str) -> Option<Job> {
    let pidf = dir.join(format!("{}.pid", key));
    let pid: i32 = std::fs::read_to_string(&pidf).ok()?.trim().parse().ok().filter(|p| *p > 1)?;
    let log = dir.join(format!("{}.log", key));
    let age = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
    let stale = if alive(pid) {
        let cmd = std::fs::read(format!("/proc/{}/cmdline", pid)).unwrap_or_default();
        let mut words = cmd.split(|&b| b == 0).map(|w| String::from_utf8_lossy(w).into_owned());
        !(words.next().is_some_and(|exe| exe.ends_with("aish-vm")) && words.next().as_deref() == Some(key))
    } else {
        age(&log).is_none_or(|a| a > Duration::from_secs(300))
    };
    if stale {
        let _ = std::fs::remove_file(&pidf);
        return None;
    }
    let t0 = age(&pidf).and_then(|e| Instant::now().checked_sub(e)).unwrap_or_else(Instant::now);
    Some(Job { child: None, pid, log, t0, polled: Instant::now() })
}

/// 自分のバイナリ (うしろの仕事はこれを KEY 引数で起こす)。ビルドしなおされて古いものが消えていれば (/proc/self/exe が
/// 「… (deleted)」)、同じ場所の新しいもの (aish が入れかえるまでのあいだも動けるように。なければ、わけを言う)
fn self_exe() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this plugin's file: {}", e))?;
    if exe.is_file() {
        return Ok(exe);
    }
    let name = exe.to_string_lossy().into_owned();
    match name.strip_suffix(" (deleted)").map(PathBuf::from) {
        Some(new) if new.is_file() => Ok(new),
        _ => Err(format!("{}: this plugin's file is gone (rebuilt or removed); build it again (bin/aish-mcp.sh --build) and aish restarts it", name)),
    }
}

/// key のものがなければ起こし、wait_ms まで待つ。終わったら最後の行の JSON (と途中の行)
pub(crate) fn job(jobs: &mut HashMap<String, Job>, root: &Path, key: &str, a: &Value) -> Value {
    let dir = root.join("build/aish-vm");
    let pidf = dir.join(format!("{}.pid", key));
    // running と答えたまま見に来なかったもの (5 分より前) が終わっていたら、続きではなく新しく始める
    // (前の start の結果を、あとの start に返してしまわないように)
    if jobs.get_mut(key).is_some_and(|j| j.polled.elapsed() > Duration::from_secs(300) && j.done().is_some()) {
        jobs.remove(key);
        let _ = std::fs::remove_file(&pidf);
    }
    if !jobs.contains_key(key)
        && let Some(j) = adopt(&dir, key)
    {
        jobs.insert(key.to_string(), j);
    }
    if !jobs.contains_key(key) {
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join(format!("{}.log", key));
        let exe = match self_exe() {
            Ok(e) => e,
            Err(e) => return error(e),
        };
        let Ok(out) = std::fs::File::create(&log) else { return error(format!("{}: cannot start", log.display())) };
        let Ok(err) = out.try_clone() else { return error("cannot start") };
        match Command::new(exe).args([key, &a.to_string()]).current_dir(root).env("AIOS_SRC", root).stdin(Stdio::null()).stdout(out).stderr(err).spawn() {
            Ok(c) => {
                let pid = c.id() as i32;
                let _ = std::fs::write(&pidf, format!("{}\n", pid));
                jobs.insert(key.to_string(), Job { child: Some(c), pid, log, t0: Instant::now(), polled: Instant::now() })
            }
            Err(e) => return error(e),
        };
    }
    let wait = Duration::from_millis(a["wait_ms"].as_u64().unwrap_or(30_000).min(50_000));
    let end = Instant::now() + wait;
    let j = jobs.get_mut(key).unwrap();
    j.polled = Instant::now();
    let status = loop {
        match j.done() {
            Some(st) => break Some(st),
            None if Instant::now() < end => std::thread::sleep(Duration::from_millis(200)),
            None => break None,
        }
    };
    let text = std::fs::read_to_string(&j.log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(12)..].join("\n");
    let ms = j.t0.elapsed().as_millis() as u64;
    let adopted = j.child.is_none();
    let Some(st) = status else {
        let mut r = json!({ "running": true, "ms": ms, "log": tail, "hint": "call again to wait more" });
        if adopted {
            r["adopted"] = json!(format!("started before aish-vm was restarted (pid {}); its arguments may differ from this call", j.pid));
        }
        return r;
    };
    let log = j.log.display().to_string();
    jobs.remove(key);
    let _ = std::fs::remove_file(&pidf);
    let mut r = lines.iter().rev().find_map(|l| aish_plugin::parse(l)).unwrap_or_else(|| json!({}));
    match st {
        Some(st) if !st.success() && r.get("error").is_none() => r["error"] = json!(format!("exit {}", st)),
        // 引きついだもの: 終わりの番号はわからないので、答えの行がなければしくじったとみる
        None if r.as_object().is_none_or(|o| o.is_empty()) => r["error"] = json!("ended without an answer (stopped, or the machine restarted)"),
        _ => {}
    }
    r["ms"] = json!(ms);
    r["log"] = json!(log);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopt_jobs() {
        let dir = std::env::temp_dir().join(format!("aish-vm-adopt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // 動いているが aish-vm KEY ではないもの (このテスト): 引きつがず、KEY.pid を消す
        std::fs::write(dir.join("ldiff.pid"), format!("{}\n", std::process::id())).unwrap();
        assert!(adopt(&dir, "ldiff").is_none());
        assert!(!dir.join("ldiff.pid").exists());
        // 終わったもの (log が新しい): 答えを渡すために引きつぐ
        let mut c = Command::new("true").spawn().unwrap();
        let pid = c.id() as i32;
        c.wait().unwrap();
        std::fs::write(dir.join("start.pid"), format!("{}\n", pid)).unwrap();
        std::fs::write(dir.join("start.log"), "{\"ok\":true}\n").unwrap();
        let mut j = adopt(&dir, "start").unwrap();
        assert!(j.child.is_none() && j.pid == pid && j.done().is_some());
        // pid がないもの
        assert!(adopt(&dir, "box").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
