// pkg_update: 新しい版の出たものをまとめて上げる (pkg_check の new → pkg_edit → pkg_build → pkg_test、push なら pkg_push)
//   names がなければ pkg_check の new のもの (daily なら、CI が毎日作る報告 (report.rs) の new の名前を、いま見なおして)。
//   タグだけで tarball がまだないもの (not_out) と、見られなかったものは上げない
//   - npm で入れるもの (.aios.json の pkg.linux.npm。PKGBUILD がない): その版を書きかえるだけ
//   - CI が作るもの (pkg/ci.json): PKGBUILD を上げるだけ。main に push すると pkg.yml が作るので、そのあと pkg_fetch
//   - ほか: PKGBUILD を上げ (sha256sums も)、それを中に入れているもの (静的なライブラリ。embed::embedders) といっしょに
//     pkg_rebuild と同じく作って確かめる (c_deps で使うものを先に。入れているものは pkgrel を上げる)
//   しくじったものは飛ばして続け (fail_fast で止める)、さいごにまとめる。コミットと main への push は呼んだ人が
//   (PKGBUILD と .aios.json。files に)
use crate::{batch, up};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Default)]
pub struct Opts {
    pub names: Vec<String>,
    /// CI の毎日の報告の new から
    pub daily: bool,
    pub fail_fast: bool,
    pub push: bool,
    pub vm: bool,
    pub dry: bool,
}

/// 上げるもの: (名前, いま, 新しい版, npm か)
type Want = Vec<(String, String, String, bool)>;

/// 何を上げるか (いまの配布元を見て)
fn want(root: &Path, o: &Opts) -> Result<(Want, Vec<String>), String> {
    let mut names = o.names.clone();
    if o.daily && names.is_empty() {
        let url = format!("{}/updates.json", crate::report::RELEASE);
        let r: Value = serde_json::from_str(&up::get(&url)?).map_err(|e| format!("{}: {}", url, e))?;
        names = r["new"].as_array().into_iter().flatten().filter_map(|x| x["name"].as_str().map(String::from)).collect();
        if names.is_empty() {
            return Ok((vec![], vec![]));
        }
    }
    let rs = up::check(&root.join("pkg"), &names, false)?;
    let (mut w, mut notes) = (Vec::new(), Vec::new());
    for r in rs {
        let n = r["name"].as_str().unwrap_or("").to_string();
        if let Some(e) = r["error"].as_str() {
            notes.push(format!("{}: could not check ({})", n, e));
        } else if let Some(u) = r["not_out"].as_str() {
            notes.push(format!("{}: {} is not out yet ({})", n, r["latest"].as_str().unwrap_or(""), u));
        } else if r["new"] == true {
            w.push((n, r["pkgver"].as_str().unwrap_or("").to_string(), r["latest"].as_str().unwrap_or("").to_string(), r["kind"] == "npm"));
        }
    }
    Ok((w, notes))
}

pub fn update(root: &Path, o: &Opts) -> Value {
    let (w, notes) = match want(root, o) {
        Ok(x) => x,
        Err(e) => return aish_plugin::error(e),
    };
    let ci = batch::ci_names(root);
    if o.dry {
        let mut text = format!("{} to update\n", w.len());
        for (n, now, latest, npm) in &w {
            let how = if *npm {
                "npm: .aios.json only"
            } else if ci.contains(n) {
                "built by the pkg workflow: edit, push main, then pkg_fetch"
            } else {
                "edit, build, test"
            };
            let users = if *npm || ci.contains(n) { vec![] } else { crate::embed::embedders(root, n) };
            let users = if users.is_empty() { String::new() } else { format!(" (then rebuild {})", users.join(" ")) };
            text.push_str(&format!("  {} {} → {}: {}{}\n", n, now, latest, how, users));
        }
        for x in &notes {
            text.push_str(&format!("  skip {}\n", x));
        }
        return json!({ "text": text, "update": w.iter().map(|(n, a, b, npm)| json!({ "name": n, "now": a, "latest": b, "npm": npm, "ci": ci.contains(n) })).collect::<Vec<_>>(), "skipped": notes });
    }
    println!("{} to update", w.len());
    let mut files: BTreeSet<String> = BTreeSet::new();
    let (mut edited, mut failed, mut ci_wait, mut build): (Vec<Value>, Vec<Value>, Vec<String>, Vec<String>) = (vec![], vec![], vec![], vec![]);
    for (n, now, latest, npm) in &w {
        // 調べたときの版にする (そのあとに出たものを取らないように。作ったものと報告が合う)
        let ver = if latest.starts_with("commit ") { None } else { Some(latest.as_str()) };
        match up::edit(root, n, ver) {
            Ok(r) => {
                files.extend(r["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)));
                let to = r["to"].as_str().unwrap_or(latest).to_string();
                println!("{}: {} → {} edited", n, now, to);
                edited.push(json!({ "name": n, "from": now, "to": to }));
                if *npm {
                    continue;
                }
                if ci.contains(n) {
                    ci_wait.push(n.clone());
                } else {
                    build.push(n.clone());
                }
            }
            Err(e) => {
                println!("{}: edit failed: {}", n, e);
                failed.push(json!({ "name": n, "step": "edit", "error": e }));
                if o.fail_fast {
                    break;
                }
            }
        }
    }
    // 上げたライブラリを中に入れているもの (pkgrel を上げて作りなおす)
    let mut names: BTreeSet<String> = build.iter().cloned().collect();
    for n in &build {
        names.extend(crate::embed::embedders(root, n));
    }
    let mut rb = Value::Null;
    if !names.is_empty() && (failed.is_empty() || !o.fail_fast) {
        let bo = batch::Opts { names: names.into_iter().collect(), fail_fast: o.fail_fast, push: o.push && failed.is_empty(), vm: o.vm, ..Default::default() };
        rb = batch::rebuild(root, &bo);
        files.extend(rb["pkgbuilds"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)));
        failed.extend(rb["failed"].as_array().into_iter().flatten().cloned());
    }
    let mut text = String::new();
    if !edited.is_empty() {
        text.push_str(&format!("updated {}: {}\n", edited.len(), edited.iter().map(|e| format!("{} {}", e["name"].as_str().unwrap_or(""), e["to"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ")));
    }
    if let Some(t) = rb["text"].as_str() {
        // pkg_rebuild のまとめ (next の行は、ここでまとめなおす)
        text.extend(t.lines().filter(|l| !l.starts_with("next:")).map(|l| format!("{}\n", l)));
    }
    if failed.iter().any(|f| f["step"] == "edit") {
        text.push_str(&format!("edit failed: {}\n", failed.iter().filter(|f| f["step"] == "edit").map(|f| format!("{} ({})", f["name"].as_str().unwrap_or(""), f["error"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", ")));
    }
    for x in &notes {
        text.push_str(&format!("skipped {}\n", x));
    }
    let pushed = rb.get("pushed").is_some_and(|p| p.get("head").is_some());
    let mut next = Vec::new();
    if !failed.is_empty() {
        next.push("fix what failed (see its log) and run pkg_update again".to_string());
    }
    if !build.is_empty() && !pushed && failed.is_empty() {
        next.push("pkg_push".to_string());
    }
    if !files.is_empty() {
        next.push(format!("commit {} and push main", files.iter().cloned().collect::<Vec<_>>().join(" ")));
    }
    if !ci_wait.is_empty() {
        next.push(format!("when the pkg workflow has built {}: pkg_fetch, pkg_test, pkg_push", ci_wait.join(" ")));
    }
    if w.is_empty() {
        text.push_str("nothing to update\n");
    } else if !next.is_empty() {
        text.push_str(&format!("next: {}\n", next.join("; then ")));
    }
    json!({ "text": text, "updated": edited, "failed": failed, "ci": ci_wait, "files": files, "rebuild": rb, "skipped": notes })
}
