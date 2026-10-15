// pkg_rebuild: いくつものパッケージを順に作りなおす (pkg_edit rebuild → pkg_build → pkg_test、push ならさいごに pkg_push)
//   names がなければ pkg_check の rebuild のもの (中に古いライブラリが入ったまま、ちがう Zig で作ったもの)。
//   unrecorded なら、Zig を書きとめる前に作ったものも。c_deps で使うものを先に作る (静的なライブラリを入れるものが、
//   新しいほうを入れるように)
//   pkgrel を上げるのは、いまの PKGBUILD の版がもう ai/repo にあるときだけ (上げないと、作っても pkg_push も ap -Syu も
//   変わったと見ない)。上げたがまだ送っていないもの (しくじって、もういちど動かしたとき) はそのまま作り、もう作りなおした
//   もの (手元のものがいまの版で、pkg_test を通っているか ai/repo に同じものがあり、古いライブラリもちがう Zig もない) は
//   飛ばす (force なら作る)。どちらも、もういちど動かしても pkgrel が 2 つ上がらないように。pkg_test でしくじったものは、
//   次は同じ版のまま作りなおす
//   CI が作るもの (pkg/ci.json) は飛ばす (pkgrel を上げて unix に push すると、pkg.yml が作る。そのあと pkg_fetch)
//   1 つずつ aish-pkg build / test を子で動かす。その出力は build-NAME.log / test-NAME.log (pkg_build と同じところ) に、
//   ここの出力 (rebuild.log) は進みぐあいだけ。しくじったものは飛ばして続け (fail_fast なら止める)、さいごにまとめる
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Default)]
pub struct Opts {
    pub names: Vec<String>,
    pub unrecorded: bool,
    pub force: bool,
    pub fail_fast: bool,
    pub push: bool,
}

/// 作りなおす 1 つ
#[derive(Debug, PartialEq)]
struct Step {
    name: String,
    /// なぜ (pkg_check の rebuild のわけ、Zig を書きとめる前、asked)
    why: String,
    /// pkgrel を上げるか (いまの PKGBUILD の版がもう ai/repo にある)
    bump: bool,
    /// repo/aarch64 の版 (なければ "")
    have: String,
    /// 手元のものがもういまの版 (確かめていないので、同じ版のまま作りなおす)
    again: bool,
}

/// CI (.github/workflows/pkg.yml) が作るもの
fn ci_names(root: &Path) -> Vec<String> {
    let text = fs::read_to_string(root.join("pkg/ci.json")).unwrap_or_default();
    serde_json::from_str::<Value>(&text).ok().and_then(|v| v.as_array().map(|a| a.iter().filter_map(|e| e["name"].as_str().map(String::from)).collect())).unwrap_or_default()
}

/// 版が同じか (repo のファイルの名前には epoch: がつくことがある)
fn same(have: &str, want: &str) -> bool {
    !want.is_empty() && (have == want || have.ends_with(&format!(":{}", want)))
}

/// c_deps で使うもの (deps に名前の向き) を先に。ほかは名前の順。輪になっていれば、残りを名前の順に
fn order(names: &BTreeSet<String>, deps: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut done: Vec<String> = Vec::new();
    while done.len() < names.len() {
        let ready = |n: &&String| !done.contains(*n) && deps.get(*n).is_none_or(|ds| ds.iter().all(|d| !names.contains(d) || d == *n || done.contains(d)));
        let next = names.iter().find(ready).or_else(|| names.iter().find(|n| !done.contains(*n))).cloned();
        match next {
            Some(n) => done.push(n),
            None => break,
        }
    }
    done
}

/// 作るもの (順に) と、飛ばすもの (名前, わけ)
type Plan = (Vec<Step>, Vec<(String, String)>);

/// 何をどの順に作るか
fn plan(root: &Path, o: &Opts) -> Result<Plan, String> {
    let pkgbuilds: BTreeMap<String, PathBuf> = crate::up::pkgbuilds(&root.join("pkg")).into_iter().collect();
    let stale = crate::embed::stale(root);
    let unrec: BTreeMap<String, String> = crate::embed::unrecorded(root).into_iter().flat_map(|(v, ns)| ns.into_iter().map(move |n| (n, v.clone()))).collect();
    let before = |v: &str| format!("built before the zig record (zig {})", if v.is_empty() { "?" } else { v });
    // えらぶ
    let mut want: BTreeMap<String, String> = BTreeMap::new();
    if o.names.is_empty() {
        for (n, libs) in &stale {
            want.insert(n.clone(), format!("rebuild: {}", crate::embed::describe(libs)));
        }
        if o.unrecorded {
            for (n, v) in &unrec {
                want.entry(n.clone()).or_insert_with(|| before(v));
            }
        }
    } else {
        for n in &o.names {
            if !pkgbuilds.contains_key(n) {
                return Err(format!("{}: no PKGBUILD", n));
            }
            let why = stale.get(n).map(|l| format!("rebuild: {}", crate::embed::describe(l))).or_else(|| unrec.get(n).map(|v| before(v))).unwrap_or_else(|| "asked".into());
            want.insert(n.clone(), why);
        }
    }
    let ci = ci_names(root);
    let published = crate::repo::published(root);
    let (mut steps, mut skip) = (Vec::new(), Vec::new());
    for (n, why) in want {
        if ci.contains(&n) {
            skip.push((n, "built by the pkg workflow on GitHub (pkg_edit rebuild, push unix, then pkg_fetch)".to_string()));
            continue;
        }
        let pkgver = crate::up::version(&pkgbuilds[&n]);
        let rel = crate::up::scalar(&fs::read_to_string(&pkgbuilds[&n]).unwrap_or_default(), "pkgrel").unwrap_or_default();
        // v が PKGBUILD のいまの版か。pkgver() で版を決めるもの (pkgver が "") は pkgrel だけでくらべる
        let current = |v: &str| if pkgver.is_empty() { v.rsplit_once('-').is_some_and(|(_, r)| r == rel) } else { same(v, &pkgver) };
        let file = crate::test::find(root, &n).and_then(|f| f.file_name().map(|f| f.to_string_lossy().into_owned())).unwrap_or_default();
        let have = crate::repo::parse(&file).map(|(_, v)| v).unwrap_or_default();
        let sent = published.get(&n);
        // 写しがなければ、手元にあるものを送ったものとみる
        let bump = match sent {
            Some(v) => current(v),
            None => published.is_empty() && !have.is_empty() && current(&have),
        };
        let again = !have.is_empty() && current(&have);
        let tested = !file.is_empty() && (crate::test::stamp(root, &file).exists() || sent == Some(&have));
        if again && tested && !stale.contains_key(&n) && !unrec.contains_key(&n) && !o.force {
            skip.push((n, format!("already rebuilt ({})", have)));
            continue;
        }
        if !pkgver.is_empty() && !have.is_empty() && !again && crate::up::vercmp(&have, &pkgver).is_gt() {
            skip.push((n, format!("repo/aarch64 has {}, newer than the PKGBUILD ({})", have, pkgver)));
            continue;
        }
        steps.push(Step { name: n, why, bump, have, again: again && !bump });
    }
    let names: BTreeSet<String> = steps.iter().map(|s| s.name.clone()).collect();
    let deps: BTreeMap<String, Vec<String>> = names.iter().map(|n| (n.clone(), crate::embed::deps(&fs::read_to_string(&pkgbuilds[n]).unwrap_or_default()))).collect();
    let seq = order(&names, &deps);
    steps.sort_by_key(|s| seq.iter().position(|n| *n == s.name).unwrap_or(usize::MAX));
    Ok((steps, skip))
}

/// 子の aish-pkg (build / test / push) を動かす。出力は log に、答えは最後の行の JSON
fn child(root: &Path, args: &[&str], log: &Path) -> Value {
    let (Ok(out), Ok(exe)) = (fs::File::create(log), crate::exe()) else { return json!({ "error": format!("{}: cannot start", log.display()) }) };
    let Ok(err) = out.try_clone() else { return json!({ "error": format!("{}: cannot start", log.display()) }) };
    let st = Command::new(exe).args(args).current_dir(root).env("AIOS_SRC", root).stdin(Stdio::null()).stdout(out).stderr(err).status();
    let text = fs::read_to_string(log).unwrap_or_default();
    let mut r = text.lines().rev().find_map(aish_plugin::parse).unwrap_or_else(|| json!({}));
    match st {
        Ok(s) if !s.success() && r.get("error").is_none() => r["error"] = json!(format!("exit {}", s)),
        Err(e) => r["error"] = json!(e.to_string()),
        _ => {}
    }
    r
}

/// 秒か分で
fn took(t: Instant) -> String {
    let s = t.elapsed().as_secs();
    if s < 60 { format!("{}s", s) } else { format!("{}m{:02}s", s / 60, s % 60) }
}

/// 順の一覧 (dry の答えと、動かすときのはじめに)
fn describe_plan(steps: &[Step], skip: &[(String, String)]) -> String {
    let mut t = String::new();
    for (i, s) in steps.iter().enumerate() {
        let how = if s.bump {
            format!("{} → pkgrel+1", if s.have.is_empty() { "?" } else { &s.have })
        } else if s.have.is_empty() {
            "not built yet".to_string()
        } else if s.again {
            format!("{} again: not tested or sent yet", s.have)
        } else {
            format!("{} → the PKGBUILD's version", s.have)
        };
        t.push_str(&format!("  {}. {} ({}): {}\n", i + 1, s.name, how, s.why));
    }
    for (n, why) in skip {
        t.push_str(&format!("  skip {}: {}\n", n, why));
    }
    t
}

/// dry: 作らずに、何をどの順に作るかだけ
pub fn dry(root: &Path, o: &Opts) -> Value {
    match plan(root, o) {
        Ok((steps, skip)) => json!({
            "text": format!("{} to rebuild{}\n{}", steps.len(), if skip.is_empty() { String::new() } else { format!(", {} skipped", skip.len()) }, describe_plan(&steps, &skip)),
            "rebuild": steps.iter().map(|s| json!({ "name": s.name, "why": s.why, "bump": s.bump, "have": s.have, "again": s.again })).collect::<Vec<_>>(),
            "skipped": skip.iter().map(|(n, w)| json!({ "name": n, "why": w })).collect::<Vec<_>>(),
        }),
        Err(e) => aish_plugin::error(e),
    }
}

/// 作りなおす。進みぐあいを 1 行ずつ出し、さいごの行に答えの JSON
pub fn rebuild(root: &Path, o: &Opts) -> Value {
    let (steps, skip) = match plan(root, o) {
        Ok(p) => p,
        Err(e) => return aish_plugin::error(e),
    };
    let n = steps.len();
    println!("{} to rebuild{}\n{}", n, if skip.is_empty() { String::new() } else { format!(", {} skipped", skip.len()) }, describe_plan(&steps, &skip).trim_end());
    let logs = root.join("build/aish-pkg");
    let _ = fs::create_dir_all(&logs);
    let (mut rebuilt, mut failed, mut pkgbuilds): (Vec<Value>, Vec<Value>, Vec<String>) = (Vec::new(), Vec::new(), Vec::new());
    let t0 = Instant::now();
    for (i, s) in steps.iter().enumerate() {
        let t = Instant::now();
        let at = format!("[{}/{}] {}", i + 1, n, s.name);
        let mut fail = |step: &str, error: String, log: Option<PathBuf>| {
            println!("{}: {} failed: {}{}", at, step, error, log.as_ref().map_or(String::new(), |l| format!(" ({})", l.strip_prefix(root).unwrap_or(l).display())));
            failed.push(json!({ "name": s.name, "step": step, "error": error, "log": log.map(|l| l.strip_prefix(root).unwrap_or(&l).display().to_string()) }));
        };
        if s.bump {
            match crate::up::bump_rel(root, &s.name) {
                Ok(v) => pkgbuilds.extend(v["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from))),
                Err(e) => {
                    fail("pkgrel", e, None);
                    if o.fail_fast {
                        break;
                    }
                    continue;
                }
            }
        }
        let blog = logs.join(format!("build-{}.log", s.name));
        let b = child(root, &["build", &s.name], &blog);
        if let Some(e) = b["error"].as_str() {
            fail("build", e.to_string(), Some(blog));
            if o.fail_fast {
                break;
            }
            continue;
        }
        let tlog = logs.join(format!("test-{}.log", s.name));
        let r = child(root, &["test", &s.name], &tlog);
        if r["ok"] != true {
            // 通らなかった確かめ (FAIL の行)
            let why = r["error"].as_str().map(String::from).unwrap_or_else(|| r["text"].as_str().unwrap_or("").lines().filter(|l| l.starts_with("FAIL")).collect::<Vec<_>>().join("; "));
            fail("test", why, Some(tlog));
            if o.fail_fast {
                break;
            }
            continue;
        }
        let to = b["file"].as_str().and_then(crate::repo::parse).map(|(_, v)| v).unwrap_or_default();
        println!("{}: {} → {} built and tested ({})", at, if s.have.is_empty() { "-" } else { &s.have }, to, took(t));
        rebuilt.push(json!({ "name": s.name, "from": s.have, "to": to, "ms": t.elapsed().as_millis() as u64 }));
    }
    // 送る: みな通ったときだけ (pkg_push も、確かめていないものがあれば止まる)
    let mut pushed = Value::Null;
    if o.push && failed.is_empty() && !rebuilt.is_empty() {
        let plog = logs.join("push.log");
        let p = child(root, &["push"], &plog);
        match p["error"].as_str() {
            Some(e) => println!("push failed: {} ({})", e, plog.strip_prefix(root).unwrap_or(&plog).display()),
            None => println!("{}", p["text"].as_str().unwrap_or("").trim_end()),
        }
        pushed = p;
    }
    let names = |v: &[Value], f: &dyn Fn(&Value) -> String| v.iter().map(f).collect::<Vec<_>>().join(", ");
    let mut text = String::new();
    if !rebuilt.is_empty() {
        text.push_str(&format!("rebuilt {}: {}\n", rebuilt.len(), names(&rebuilt, &|r| format!("{} {}", r["name"].as_str().unwrap_or(""), r["to"].as_str().unwrap_or("")))));
    }
    if !failed.is_empty() {
        text.push_str(&format!("failed {}: {}\n", failed.len(), names(&failed, &|f| format!("{} ({}: {})", f["name"].as_str().unwrap_or(""), f["step"].as_str().unwrap_or(""), f["error"].as_str().unwrap_or("")))));
    }
    if !skip.is_empty() {
        text.push_str(&format!("skipped {}: {}\n", skip.len(), skip.iter().map(|(n, w)| format!("{} ({})", n, w)).collect::<Vec<_>>().join(", ")));
    }
    let next = match (&pushed, rebuilt.is_empty()) {
        (_, true) => String::new(),
        (Value::Null, _) => "next: pkg_push, then commit the PKGBUILDs and push unix\n".to_string(),
        (p, _) if p.get("error").is_some() => "next: see why pkg_push stopped, then commit the PKGBUILDs and push unix\n".to_string(),
        _ => "next: commit the PKGBUILDs and push unix\n".to_string(),
    };
    text.push_str(&next);
    if n == 0 {
        text.push_str("nothing to rebuild\n");
    }
    json!({ "text": text, "rebuilt": rebuilt, "failed": failed, "skipped": skip.iter().map(|(n, w)| json!({ "name": n, "why": w })).collect::<Vec<_>>(), "pkgbuilds": pkgbuilds, "pushed": pushed, "ms": t0.elapsed().as_millis() as u64 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libraries_first() {
        let names: BTreeSet<String> = ["less", "ncurses", "tmux", "libevent", "zlib"].iter().map(|s| s.to_string()).collect();
        let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        deps.insert("less".into(), vec!["ncurses".into()]);
        deps.insert("tmux".into(), vec!["ncurses".into(), "libevent".into()]);
        // 一覧にないもの (musl) と、自分を指すものは数えない
        deps.insert("ncurses".into(), vec!["musl".into(), "ncurses".into()]);
        assert_eq!(order(&names, &deps), ["libevent", "ncurses", "less", "tmux", "zlib"]);
        // 輪になっていても、みな 1 回ずつ
        let names: BTreeSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
        deps.insert("a".into(), vec!["b".into()]);
        deps.insert("b".into(), vec!["a".into()]);
        assert_eq!(order(&names, &deps), ["a", "b"]);
    }

    #[test]
    fn same_version() {
        assert!(same("6.6-2", "6.6-2"));
        assert!(same("1:2.3-1", "2.3-1"));
        assert!(!same("6.6-2", "6.6-3"));
        // pkgver() で決めるもの (PKGBUILD からはわからない) は同じと見ない
        assert!(!same("0.0.1.r486-1", ""));
    }
}
