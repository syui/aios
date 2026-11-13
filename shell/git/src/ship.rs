// ship: いまのブランチを送り出す。push の前に git の道具と同じたしかめ (署名、コミットした人、remote が進んでいないか)
// と、変えたままのファイル。push (origin のいまのブランチ、aish.ship.push の refspec)、aish.ship.after のコマンド
// (ミラーなど)、それから publish が要るなら (最後に出したもの (GitHub の release の target_commitish) から
// aish.ship.paths の下が変わっていれば) aish.ship.publish をうしろで動かし、要らなければ CI を待つ。
// 1 回に 50 秒まで。まだなら running: true で、もう一度呼ぶと続きを見る (状態は ~/.cache/aish/ship/ に。aish や
// プラグインを作りなおしても続けられる)。設定はリポジトリの git config:
//   aish.ship.push     origin に送るほかの refspec (いくつでも。例 HEAD:main)
//   aish.ship.after    push のあとに動かすコマンド (いくつでも。例 sh bin/gitea.sh os)
//   aish.ship.publish  出すコマンド (例 sh bin/publish.sh。CI を待つのもこのコマンド)
//   aish.ship.paths    出すものの元のディレクトリ (空白で分ける。例 kernel/ user/ shell/)
//   aish.ship.release  最後に出したものの GitHub の release のタグ (既定 latest)
// aios では、publish は作業中の変更 (kernel/ user/ shell/) があると断り、push のあとで unix の CI が新しい push で
// 取り消されると止まった。ここでは push の前に見て、取り消されたらそう言う (2026-11-13)
use crate::{MAX_MS, ci, gh, git, github_repo, state};
use aish_plugin::{Value, error, json, s};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SHIP: &str = r#"{"type":"object","properties":{
"dir":{"type":"string","description":"リポジトリ (既定: いまのディレクトリ)"},
"check":{"type":"boolean","description":"送らずに、送れるか (署名、変えたままのファイル、publish が要るか) だけ見る"},
"publish":{"type":"boolean","description":"publish するか (既定: aish.ship.paths の下が最後に出したものから変わっていれば)"},
"stop":{"type":"boolean","description":"うしろで動いている publish を止める"},
"wait_ms":{"type":"integer","description":"終わるまで待つ長さ (既定で最大 50000。まだなら running: true なので、もう一度呼ぶ)"}}}"#;

struct Cfg {
    push: Vec<String>,
    after: Vec<String>,
    publish: String,
    paths: Vec<String>,
    release: String,
}

fn cfg(top: &str) -> Cfg {
    let all = |k: &str| git(top, &["config", "--get-all", k]).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()).map(String::from).collect::<Vec<_>>();
    let one = |k: &str| git(top, &["config", "--get", k]).unwrap_or_default();
    let release = one("aish.ship.release");
    Cfg {
        push: all("aish.ship.push"),
        after: all("aish.ship.after"),
        publish: one("aish.ship.publish"),
        paths: one("aish.ship.paths").split_whitespace().map(String::from).collect(),
        release: if release.is_empty() { "latest".into() } else { release },
    }
}

/// うしろの publish の記録 (~/.cache/aish/ship/NAME.json と .log)
fn state_path(top: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let name: String = top.trim_matches('/').replace('/', "_");
    std::path::Path::new(&home).join(".cache/aish/ship").join(format!("{}.json", name))
}

fn under(paths: &[String], file: &str) -> bool {
    paths.iter().any(|p| file.starts_with(p.as_str()))
}

fn alive(pid: u64) -> bool {
    std::path::Path::new(&format!("/proc/{}", pid)).exists()
}

/// 終わりの数行 (色を消して)
fn tail(log: &str, n: usize) -> String {
    let t = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|l| format!("  {}\n", l.chars().take(200).collect::<String>())).collect()
}

/// コマンドを動かして (sh -c)、(うまくいったか, 出力の終わり)
fn sh(top: &str, cmd: &str) -> (bool, String) {
    match Command::new("sh").arg("-c").arg(cmd).current_dir(top).stdin(Stdio::null()).output() {
        Ok(o) => {
            let mut t = String::from_utf8_lossy(&o.stdout).to_string();
            t.push_str(&String::from_utf8_lossy(&o.stderr));
            let last: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).collect();
            (o.status.success(), last[last.len().saturating_sub(2)..].join(" / "))
        }
        Err(e) => (false, e.to_string()),
    }
}

/// git push を、ネットワークのしくじり (503 など) なら 3 回までやりなおす
fn push(top: &str, args: &[&str]) -> (bool, String) {
    let cmd = format!("git push {}", args.join(" "));
    let mut last = String::new();
    for i in 0..4 {
        let (ok, out) = sh(top, &format!("{} 2>&1", cmd));
        if ok {
            return (true, out);
        }
        last = out;
        if !["503", "502", "Could not resolve", "timed out", "unexpected disconnect", "RPC failed"].iter().any(|e| last.contains(e)) {
            break;
        }
        std::thread::sleep(Duration::from_secs(2 << i));
    }
    (false, last)
}

/// 最後に出したものから、出すものの元が変わったか (Some(ファイル)。わからなければ None)
fn changed_since_release(top: &str, repo: &str, c: &Cfg) -> Option<Vec<String>> {
    let r = gh(&format!("repos/{}/releases/tags/{}", repo, c.release)).ok()?;
    let rc = s(&r, "target_commitish").to_string();
    git(top, &["cat-file", "-e", &format!("{}^{{commit}}", rc)])?;
    let d = git(top, &["diff", "--name-only", &format!("{}..HEAD", rc)]).unwrap_or_default();
    Some(d.lines().filter(|f| under(&c.paths, f)).map(String::from).collect())
}

pub fn ship(v: &Value) -> Value {
    let a = &v["args"];
    let dir = if s(a, "dir").is_empty() { s(v, "pwd").to_string() } else { s(a, "dir").to_string() };
    let Some(top) = git(&dir, &["rev-parse", "--show-toplevel"]) else { return error(format!("{}: not a git repository", dir)) };
    let head = git(&top, &["rev-parse", "HEAD"]).unwrap_or_default();
    let short = head[..head.len().min(7)].to_string();
    let sp = state_path(&top);
    let wait = Duration::from_millis(a["wait_ms"].as_u64().unwrap_or(MAX_MS).min(MAX_MS));

    // うしろで動いているもの (publish か CI 待ち) の続き
    if let Ok(t) = std::fs::read_to_string(&sp) {
        let st: Value = aish_plugin::parse(&t).unwrap_or_default();
        let (sha, kind, log) = (s(&st, "sha").to_string(), s(&st, "kind").to_string(), s(&st, "log").to_string());
        let pid = st["pid"].as_u64().unwrap_or(0);
        if a["stop"].as_bool() == Some(true) {
            if pid != 0 && alive(pid) {
                // プロセスグループごと (publish の中の curl や cargo も)
                let _ = Command::new("kill").arg("--").arg(format!("-{}", pid)).status();
            }
            let _ = std::fs::remove_file(&sp);
            return json!({ "text": format!("stopped {} ({})\n", kind, &sha[..sha.len().min(7)]), "done": true, "ok": false });
        }
        if sha == head && kind == "publish" {
            let t0 = Instant::now();
            loop {
                let txt = std::fs::read_to_string(&log).unwrap_or_default();
                let code = txt.lines().rev().find_map(|l| l.strip_prefix("ship-exit ")).and_then(|c| c.trim().parse::<i32>().ok());
                if let Some(code) = code {
                    let _ = std::fs::remove_file(&sp);
                    let mut text = format!("{}: publish {} ({})\n", short, if code == 0 { "done" } else { "failed" }, log);
                    text.push_str(&tail(&log, if code == 0 { 3 } else { 8 }));
                    // CI が落ちた・取り消された: どれか
                    if txt.contains("CI did not pass") {
                        text.push_str(&s(&ci(&json!({ "args": { "dir": top, "sha": head, "wait_ms": 0 }, "pwd": top })), "text").to_string());
                        if txt.contains("completed cancelled") {
                            text.push_str("  the CI run was cancelled (a newer push?): ship again from the newest commit\n");
                        }
                    }
                    return json!({ "text": text, "done": true, "ok": code == 0 });
                }
                if !alive(pid) {
                    let _ = std::fs::remove_file(&sp);
                    return json!({ "text": format!("{}: publish ended without a result ({})\n{}", short, log, tail(&log, 8)), "done": true, "ok": false });
                }
                if t0.elapsed() + Duration::from_secs(3) > wait {
                    return json!({ "text": format!("{}: publishing ({}s, {})\n{}", short, st["started"].as_u64().map_or(0, |t0| now().saturating_sub(t0)), log, tail(&log, 2)), "done": false, "running": true });
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
        if sha == head && kind == "ci" {
            let r = ci(&json!({ "args": { "dir": top, "sha": head, "wait_ms": wait.as_millis() as u64 }, "pwd": top }));
            if r["done"] == true {
                let _ = std::fs::remove_file(&sp);
            }
            return r;
        }
        // 前の HEAD のもの: publish がまだ動いていれば、新しいものは送らない
        if kind == "publish" && pid != 0 && alive(pid) {
            return error(format!("a publish for {} is still running ({}); wait for it (ship) or stop it (ship stop: true)", &sha[..sha.len().min(7)], log));
        }
        let _ = std::fs::remove_file(&sp);
    }

    // 送る前のたしかめ
    let c = cfg(&top);
    let branch = git(&top, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    if branch == "HEAD" || branch.is_empty() {
        return error("HEAD is detached: check out a branch first");
    }
    let repo = git(&top, &["remote", "get-url", "origin"]).as_deref().and_then(github_repo);
    let pre = state(&json!({ "args": { "dir": top, "remote": true }, "pwd": top }));
    let mut text = s(&pre, "text").to_string();
    let mut problems: Vec<String> = pre["problems"].as_array().map(|p| p.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    // 変えたままのもの: 出すものの元にあれば、publish は断る
    let dirty: Vec<String> = git(&top, &["status", "--porcelain=v1"]).unwrap_or_default().lines().map(|l| l.get(3..).unwrap_or("").to_string()).collect();
    let dirty_src: Vec<&String> = dirty.iter().filter(|f| under(&c.paths, f)).collect();
    let changed = match &repo {
        Some(r) if !c.publish.is_empty() && !c.paths.is_empty() => changed_since_release(&top, r, &c),
        _ => Some(Vec::new()),
    };
    let want = match a["publish"].as_bool() {
        Some(b) => b && !c.publish.is_empty(),
        None => !c.publish.is_empty() && changed.as_ref().is_none_or(|f| !f.is_empty()),
    };
    match &changed {
        Some(f) if !f.is_empty() => text.push_str(&format!("publish: {} file(s) changed under {} since the {} release ({}{})\n", f.len(), c.paths.join(" "), c.release, f.iter().take(3).cloned().collect::<Vec<_>>().join(" "), if f.len() > 3 { " ..." } else { "" })),
        Some(_) => text.push_str(&format!("publish: nothing under {} changed since the {} release\n", c.paths.join(" "), c.release)),
        None if !c.publish.is_empty() => text.push_str(&format!("publish: could not read the {} release; publishing to be safe\n", c.release)),
        None => {}
    }
    if want && !dirty_src.is_empty() {
        problems.push(format!("uncommitted changes under {} ({}): {} would refuse; commit or stash them", c.paths.join(" "), dirty_src.iter().take(5).map(|s| s.as_str()).collect::<Vec<_>>().join(" "), c.publish));
    }
    if a["check"].as_bool() == Some(true) || !problems.is_empty() {
        let ok = problems.is_empty();
        let mut r = json!({ "text": text, "done": true, "ok": ok, "publish": want });
        if !ok {
            r["problems"] = json!(problems);
        }
        return r;
    }

    // 送る
    let mut steps = String::new();
    let (ok, out) = push(&top, &["-u", "origin", &format!("HEAD:{}", branch)]);
    steps.push_str(&format!("push origin {}: {}\n", branch, if ok { "ok" } else { out.as_str() }));
    if !ok {
        return json!({ "text": text + &steps, "done": true, "ok": false });
    }
    for spec in &c.push {
        let (ok, out) = push(&top, &["origin", spec]);
        steps.push_str(&format!("push origin {}: {}\n", spec, if ok { "ok" } else { out.as_str() }));
        if !ok {
            return json!({ "text": text + &steps, "done": true, "ok": false });
        }
    }
    for cmd in &c.after {
        let (ok, out) = sh(&top, cmd);
        steps.push_str(&format!("{}: {}\n", cmd, if ok { "ok".to_string() } else { format!("failed: {}", out) }));
        if !ok {
            return json!({ "text": text + &steps, "done": true, "ok": false });
        }
    }
    text.push_str(&steps);

    // うしろで: publish (CI もそこで待つ) か、CI を待つだけ
    let _ = std::fs::create_dir_all(sp.parent().unwrap());
    if want {
        let log = sp.with_extension("log").display().to_string();
        let child = Command::new("sh")
            .arg("-c")
            .arg(format!("{} > {} 2>&1; echo \"ship-exit $?\" >> {}", c.publish, log, log))
            .current_dir(&top)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
        let pid = match child {
            Ok(ch) => ch.id() as u64,
            Err(e) => return json!({ "text": text + &format!("{}: {}\n", c.publish, e), "done": true, "ok": false }),
        };
        let _ = std::fs::write(&sp, json!({ "sha": head, "kind": "publish", "pid": pid, "log": log, "started": now() }).to_string());
        text.push_str(&format!("{}: started (pid {}, {}): it waits for CI, then publishes. call ship again to follow it\n", c.publish, pid, log));
        return json!({ "text": text, "done": false, "running": true });
    }
    if repo.is_none() {
        return json!({ "text": text, "done": true, "ok": true });
    }
    let _ = std::fs::write(&sp, json!({ "sha": head, "kind": "ci", "started": now() }).to_string());
    let r = ci(&json!({ "args": { "dir": top, "sha": head, "wait_ms": 20_000 }, "pwd": top }));
    text.push_str(s(&r, "text"));
    if r["done"] == true {
        let _ = std::fs::remove_file(&sp);
        return json!({ "text": text, "done": true, "ok": r["ok"] == true });
    }
    json!({ "text": text, "done": false, "running": true })
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

use std::os::unix::process::CommandExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        let p = vec!["kernel/".to_string(), "user/".to_string()];
        assert!(under(&p, "kernel/src/shm.rs"));
        assert!(under(&p, "user/src/bin/aish/main.rs"));
        assert!(!under(&p, "doc/aios.md"));
        assert!(!under(&p, "test/kernel-json.py"));
    }

    #[test]
    fn state_names() {
        let p = state_path("/home/user/aios");
        assert!(p.ends_with(".cache/aish/ship/home_user_aios.json"));
    }
}
