// 毎日の見はり (aish-pkg report DIR [PREV]): check をまるごと回して、DIR に 2 つ書く。パッケージのほかに、改善の種も:
//   linux: kernel.org の安定版と mainline が、手本 (test/sysdiff/linux.sum の linux) より新しければ (答えあわせのしなおし)
//   ci: main の unix ワークフローの、いちばん新しい終わった run がしくじっていれば
//   todo: リポジトリの todo.json (課題の一覧) の open と doing のもの
//   updates.json: 機械が読むもの。まだ上げていない新しい版 (new)、タグだけで tarball がまだないもの (not_out)、
//     直っていない CVE (vulns)、見られなかったもの (errors)、作りなおすもの (rebuild。repo/aarch64 があるときだけ)、
//     check と同じ text。それぞれに、はじめて見た日 (since)
//   updates.atom: やること (新しい版、直っていない CVE) の Atom の feed。aigpt の fetch_feed が新しく出たものだけを
//     見せる (id はパッケージと版か CVE の番号で決まるので、同じものは次の日も同じ id。版が進めば新しい entry)
// CI (.github/workflows/pkg.yml の report) が毎日動かして、GitHub のリリース pkg-updates に置く (RELEASE)。
// PREV (前の updates.json) があれば、そこにあったものの since をうけつぐ
use crate::{up, vuln};
use aish_plugin::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// 置き場所 (GitHub のリリース pkg-updates)
pub const RELEASE: &str = "https://github.com/syui/aios/releases/download/pkg-updates";
/// パッケージの PKGBUILD を見せるところ
const BLOB: &str = "https://github.com/syui/aios/blob/main";

/// いまの時刻 (UTC、RFC 3339)
fn now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // 1970-01-01 からの日数を暦に (Howard Hinnant の civil_from_days。up.rs の today と同じ)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, rem / 60 % 60, rem % 60)
}

/// XML の文字
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// CVE などの番号のページ
fn advisory(id: &str) -> String {
    if id.starts_with("RUSTSEC-") {
        format!("https://rustsec.org/advisories/{}", id)
    } else if id.starts_with("GHSA-") {
        format!("https://github.com/advisories/{}", id)
    } else if id.starts_with("CVE-") {
        format!("https://nvd.nist.gov/vuln/detail/{}", id)
    } else {
        String::new()
    }
}

/// id の中の空白 (コミットの版 "commit 4291f1c") は - に (Atom の id は IRI)
fn id_of(kind: &str, name: &str, ver: &str) -> String {
    format!("{}:{}:{}", kind, name, ver.replace(' ', "-"))
}

/// feed の 1 つ
struct Entry {
    id: String,
    title: String,
    link: String,
    since: String,
    summary: String,
}

/// 改善の種 (ネットとリポジトリから。gather で集める。テストは空のもの)
pub struct Seeds {
    /// (手本の Linux, [(stable|mainline, 版)])
    pub linux: Result<(String, Vec<(String, String)>), String>,
    /// しくじった main の unix の run (短いコミット, URL)
    pub ci: Result<Option<(String, String)>, String>,
    /// todo.json の open と doing
    pub todo: Result<Vec<Value>, String>,
}

/// 改善の種を集める (kernel.org、GitHub の API、todo.json)
pub fn gather(root: &Path) -> Seeds {
    Seeds { linux: linux_news(root), ci: ci_failure(), todo: todo(root) }
}

/// answer は check の答え (text など)、rs は up::check の 1 つずつ、vulns は直っていないもの、seeds は改善の種
pub fn write(root: &Path, out: &Path, prev: Option<&Path>, answer: &Value, rs: &[Value], vulns: &[vuln::Vuln], seeds: Seeds) -> Result<Value, String> {
    let at = now();
    // 前の報告の since (id → いつ)
    let old: Value = prev.and_then(|p| fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut since: HashMap<String, String> = HashMap::new();
    for k in ["new", "not_out", "vulns", "linux", "ci", "todo"] {
        for x in old[k].as_array().into_iter().flatten() {
            if let (Some(id), Some(s)) = (x["id"].as_str(), x["since"].as_str()) {
                since.insert(id.to_string(), s.to_string());
            }
        }
    }
    let first = |id: &str| since.get(id).cloned().unwrap_or_else(|| at.clone());
    // パッケージの PKGBUILD (リポジトリの中の道)
    let paths: HashMap<String, String> = up::pkgbuilds(&root.join("pkg"))
        .into_iter()
        .map(|(n, p)| (n, p.strip_prefix(root).unwrap_or(&p).display().to_string()))
        .collect();
    let page = |name: &str, npm: bool| -> String {
        if npm {
            format!("https://www.npmjs.com/package/{}", name)
        } else {
            paths.get(name).map_or_else(String::new, |p| format!("{}/{}", BLOB, p))
        }
    };

    let (mut new, mut not_out, mut errors, mut entries) = (vec![], vec![], vec![], vec![]);
    for r in rs {
        let name = r["name"].as_str().unwrap_or("");
        let now = r["pkgver"].as_str().unwrap_or("");
        let npm = r["kind"] == "npm";
        if let Some(e) = r["error"].as_str() {
            errors.push(json!({ "name": name, "now": now, "error": e }));
            continue;
        }
        if r["new"] != true {
            continue;
        }
        let latest = r["latest"].as_str().unwrap_or("");
        if let Some(u) = r["not_out"].as_str() {
            let id = id_of("not_out", name, latest);
            not_out.push(json!({ "id": id, "name": name, "now": now, "latest": latest, "missing": u, "since": first(&id) }));
            continue;
        }
        let id = id_of("new", name, latest);
        let mut j = json!({ "id": id, "name": name, "now": now, "latest": latest, "since": first(&id) });
        if npm {
            j["kind"] = json!("npm");
        }
        if let Some(v) = r.get("via") {
            j["via"] = v.clone();
        }
        entries.push(Entry {
            id: id.clone(),
            title: format!("{} {} → {}{}", name, now, latest, if npm { " (npm)" } else { "" }),
            link: page(name, npm),
            since: first(&id),
            summary: if npm {
                format!("pkg_edit {} (.aios.json の pkg.linux.npm)", name)
            } else {
                format!("pkg_edit {} → pkg_build → pkg_test → pkg_push", name)
            },
        });
        new.push(j);
    }
    let mut vj = vec![];
    for v in vulns {
        let mut ids: Vec<&str> = v.fixes.iter().flat_map(|f| f.cves.iter().map(String::as_str)).collect();
        ids.sort();
        ids.dedup();
        let id = id_of("vuln", &v.name, &ids.join(","));
        let what = vuln::describe(v);
        entries.push(Entry {
            id: id.clone(),
            title: format!("{} {}: {}", v.name, v.now, what),
            link: ids.first().map(|c| advisory(c)).filter(|l| !l.is_empty()).unwrap_or_else(|| page(&v.name, false)),
            since: first(&id),
            summary: what.clone(),
        });
        vj.push(json!({ "id": id, "name": v.name, "now": v.now, "what": what, "since": first(&id) }));
    }

    // 改善の種: Linux の新しい版、CI、課題の一覧
    let (mut lj, mut cj, mut tj) = (vec![], vec![], vec![]);
    match seeds.linux {
        Ok((refv, news)) => {
            for (kind, v) in news {
                let id = id_of("linux", &kind, &v);
                let how = if kind == "stable" { "latest" } else { "mainline" };
                entries.push(Entry {
                    id: id.clone(),
                    title: format!("Linux {} ({}) is out; the reference is {}", v, kind, refv),
                    link: "https://www.kernel.org/".into(),
                    since: first(&id),
                    summary: format!("test/linux-vm.sh build {how}; aish sysdiff base {v}; test/sysdiff-ci.sh golden {how}; the other tests on both Linux (doc/aios.md: Linux 7.4 に合わせる)"),
                });
                lj.push(json!({ "id": id, "kind": kind, "version": v, "reference": refv, "since": first(&id) }));
            }
        }
        Err(e) => errors.push(json!({ "name": "linux", "error": e })),
    }
    match seeds.ci {
        Ok(Some((sha, url))) => {
            let id = id_of("ci", "unix", &sha);
            entries.push(Entry { id: id.clone(), title: format!("CI unix fails on main at {}", sha), link: url.clone(), since: first(&id), summary: "aish ci; read the failing step's log and fix it".into() });
            cj.push(json!({ "id": id, "workflow": "unix", "sha": sha, "url": url, "since": first(&id) }));
        }
        Ok(None) => {}
        Err(e) => errors.push(json!({ "name": "ci", "error": e })),
    }
    match seeds.todo {
        Ok(items) => {
            for t in items {
                let tid = t["id"].as_str().unwrap_or("?").to_string();
                let id = id_of("todo", &tid, t["status"].as_str().unwrap_or("open"));
                entries.push(Entry {
                    id: id.clone(),
                    title: format!("[{}] {}", t["area"].as_str().unwrap_or("?"), t["what"].as_str().unwrap_or("")),
                    link: format!("{}/todo.json", BLOB),
                    since: t["since"].as_str().map(|d| format!("{}T00:00:00Z", d)).unwrap_or_else(|| first(&id)),
                    summary: format!("{} (verify: {})", t["why"].as_str().unwrap_or(""), t["verify"].as_str().unwrap_or("")),
                });
                tj.push(json!({ "id": tid, "area": t["area"], "status": t["status"], "what": t["what"], "since": t["since"] }));
            }
        }
        Err(e) => errors.push(json!({ "name": "todo", "error": e })),
    }

    let mut report = json!({
        "date": at,
        "new": new,
        "not_out": not_out,
        "vulns": vj,
        "linux": lj,
        "ci": cj,
        "todo": tj,
        "errors": errors,
        "text": answer["text"],
        "feed": format!("{}/updates.atom", RELEASE),
    });
    for k in ["rebuild", "unrecorded", "vulns_note", "via_listing"] {
        if let Some(v) = answer.get(k) {
            report[k] = v.clone();
        }
    }

    // feed: 新しく出たものが上 (since の新しい順)
    entries.sort_by(|a, b| b.since.cmp(&a.since).then_with(|| a.id.cmp(&b.id)));
    let mut atom = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<feed xmlns=\"http://www.w3.org/2005/Atom\">\n");
    atom.push_str("  <id>tag:syui.ai,2026:aios-pkg-updates</id>\n");
    atom.push_str("  <title>aios: what to do (packages, Linux, CI, todo)</title>\n");
    atom.push_str(&format!("  <link href=\"{}/updates.json\"/>\n", RELEASE));
    atom.push_str(&format!("  <updated>{}</updated>\n", at));
    atom.push_str("  <author><name>aish-pkg report</name></author>\n");
    for e in &entries {
        atom.push_str(&format!(
            "  <entry>\n    <id>tag:syui.ai,2026:aios-pkg:{}</id>\n    <title>{}</title>\n    <link href=\"{}\"/>\n    <published>{}</published>\n    <updated>{}</updated>\n    <summary>{}</summary>\n  </entry>\n",
            esc(&e.id),
            esc(&e.title),
            esc(&e.link),
            e.since,
            e.since,
            esc(&e.summary)
        ));
    }
    atom.push_str("</feed>\n");

    fs::create_dir_all(out).map_err(|e| format!("{}: {}", out.display(), e))?;
    let json_text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())? + "\n";
    fs::write(out.join("updates.json"), json_text).map_err(|e| e.to_string())?;
    fs::write(out.join("updates.atom"), atom).map_err(|e| e.to_string())?;
    Ok(json!({
        "dir": out.display().to_string(),
        "new": report["new"].as_array().map_or(0, Vec::len),
        "not_out": report["not_out"].as_array().map_or(0, Vec::len),
        "vulns": report["vulns"].as_array().map_or(0, Vec::len),
        "errors": report["errors"].as_array().map_or(0, Vec::len),
        "entries": entries.len(),
    }))
}

/// Linux の版の大きさ: X.Y[.Z][-rcN] → (X, Y, Z, rc)。rc でないものは rc = MAX (安定版は同じ X.Y の rc より新しい)
fn linux_key(v: &str) -> Option<(u32, u32, u32, u32)> {
    let (base, rc) = match v.split_once("-rc") {
        Some((b, r)) => (b, r.parse().ok()?),
        None => (v, u32::MAX),
    };
    let mut it = base.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next().map_or(Some(0), |x| x.parse().ok())?;
    Some((a, b, c, rc))
}

/// 手本の Linux (test/sysdiff/linux.sum の「linux X」) と、それより新しい kernel.org のもの ((stable|mainline, 版))
fn linux_news(root: &Path) -> Result<(String, Vec<(String, String)>), String> {
    let sum = fs::read_to_string(root.join("test/sysdiff/linux.sum")).map_err(|e| format!("test/sysdiff/linux.sum: {}", e))?;
    let refv = sum.lines().find_map(|l| l.strip_prefix("linux ")).map(|v| v.trim().to_string()).ok_or("test/sysdiff/linux.sum: no linux line")?;
    let rk = linux_key(&refv).ok_or_else(|| format!("{}: not a Linux version", refv))?;
    let rel: Value = serde_json::from_str(&up::get("https://www.kernel.org/releases.json")?).map_err(|e| format!("releases.json: {}", e))?;
    let stable = rel["latest_stable"]["version"].as_str().unwrap_or("").to_string();
    let mainline = rel["releases"].as_array().into_iter().flatten().find(|r| r["moniker"] == "mainline").and_then(|r| r["version"].as_str()).unwrap_or("").to_string();
    let mut out = vec![];
    if linux_key(&stable).is_some_and(|k| k > rk) {
        out.push(("stable".to_string(), stable.clone()));
    }
    if mainline != stable && linux_key(&mainline).is_some_and(|k| k > rk) {
        out.push(("mainline".to_string(), mainline));
    }
    Ok((refv, out))
}

/// main の unix ワークフローの、いちばん新しい終わった run がしくじっていれば (短いコミット, run の URL)。
/// 次の push に追いこされて止められたもの (cancelled) と飛ばしたもの (skipped) は見ない (しくじりではない。前は
/// それを知らせていた)。GH_TOKEN があれば使う (CI の中。なければ名前なしで: 1 時間に 60 回まで)
fn ci_failure() -> Result<Option<(String, String)>, String> {
    let url = "https://api.github.com/repos/syui/aios/actions/workflows/unix.yml/runs?branch=main&status=completed&per_page=20";
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-fsSL", "-m", "30", "-H", "Accept: application/vnd.github+json"]);
    if let Ok(t) = std::env::var("GH_TOKEN").or_else(|_| std::env::var("GITHUB_TOKEN")) {
        cmd.args(["-H", &format!("Authorization: Bearer {}", t)]);
    }
    let o = cmd.arg(url).output().map_err(|e| format!("curl: {}", e))?;
    if !o.status.success() {
        return Err(format!("{}: {}", url, String::from_utf8_lossy(&o.stderr).trim()));
    }
    let v: Value = serde_json::from_slice(&o.stdout).map_err(|e| e.to_string())?;
    let Some(run) = v["workflow_runs"].as_array().and_then(|a| a.iter().find(|r| !matches!(r["conclusion"].as_str(), Some("cancelled" | "skipped")))) else { return Ok(None) };
    if run["conclusion"] == "success" {
        return Ok(None);
    }
    let sha = run["head_sha"].as_str().unwrap_or("").chars().take(7).collect();
    Ok(Some((sha, run["html_url"].as_str().unwrap_or("").to_string())))
}

/// todo.json (リポジトリの一番上の課題の一覧) の open と doing のもの。id がかさなっていればしくじる
fn todo(root: &Path) -> Result<Vec<Value>, String> {
    let t = fs::read_to_string(root.join("todo.json")).map_err(|e| format!("todo.json: {}", e))?;
    let v: Vec<Value> = serde_json::from_str(&t).map_err(|e| format!("todo.json: {}", e))?;
    let mut seen = std::collections::HashSet::new();
    for x in &v {
        let id = x["id"].as_str().ok_or("todo.json: an item has no id")?;
        if !seen.insert(id.to_string()) {
            return Err(format!("todo.json: {} twice", id));
        }
    }
    Ok(v.into_iter().filter(|x| matches!(x["status"].as_str(), Some("open" | "doing"))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_versions() {
        assert!(linux_key("7.4").unwrap() > linux_key("7.4-rc1").unwrap());
        assert!(linux_key("7.4-rc2").unwrap() > linux_key("7.4-rc1").unwrap());
        assert!(linux_key("7.4-rc1").unwrap() > linux_key("7.3.1").unwrap());
        assert!(linux_key("7.3.1").unwrap() > linux_key("7.3").unwrap());
        assert!(linux_key("6.18.60").unwrap() < linux_key("7.2.15").unwrap());
        assert_eq!(linux_key("x"), None);
    }

    #[test]
    fn time_and_escaping() {
        let t = now();
        assert_eq!(t.len(), 20);
        assert!(t.ends_with('Z') && t.as_bytes()[10] == b'T');
        assert_eq!(esc("a<b & \"c\">"), "a&lt;b &amp; &quot;c&quot;&gt;");
        assert_eq!(advisory("RUSTSEC-2026-0385"), "https://rustsec.org/advisories/RUSTSEC-2026-0385");
        assert_eq!(advisory("GHSA-abcd-efgh-ijkl"), "https://github.com/advisories/GHSA-abcd-efgh-ijkl");
    }

    #[test]
    fn report_keeps_since_and_writes_a_feed() {
        let dir = std::env::temp_dir().join(format!("aish-pkg-report-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("pkg/c/foo")).unwrap();
        fs::write(dir.join("pkg/c/foo/PKGBUILD"), "pkgname=foo\npkgver=1.0\n").unwrap();
        let prev = dir.join("prev.json");
        fs::write(&prev, r#"{"new":[{"id":"new:foo:1.1","since":"2026-10-01T00:00:00Z"}]}"#).unwrap();
        let rs = vec![
            json!({ "name": "foo", "pkgver": "1.0", "latest": "1.1", "new": true }),
            json!({ "name": "pnpm", "pkgver": "12.9.1", "latest": "12.16.1", "new": true, "kind": "npm" }),
            json!({ "name": "bar", "pkgver": "2.0", "latest": "2.1", "new": true, "not_out": "404 https://x/bar-2.1.tar.xz" }),
            json!({ "name": "baz", "pkgver": "3", "error": "no answer" }),
        ];
        let seeds = Seeds {
            linux: Ok(("7.4-rc1".into(), vec![("stable".into(), "7.4".into())])),
            ci: Ok(Some(("abc1234".into(), "https://example.org/run".into()))),
            todo: Ok(vec![json!({ "id": "robust-futex", "area": "kernel", "status": "open", "since": "2026-11-02", "what": "robust futex", "why": "w", "verify": "v" })]),
        };
        let r = write(&dir, &dir.join("out"), Some(&prev), &json!({ "text": "t" }), &rs, &[], seeds).unwrap();
        assert_eq!((r["new"].as_u64(), r["not_out"].as_u64(), r["errors"].as_u64(), r["entries"].as_u64()), (Some(2), Some(1), Some(1), Some(5)));
        let j: Value = serde_json::from_str(&fs::read_to_string(dir.join("out/updates.json")).unwrap()).unwrap();
        assert_eq!(j["new"][0]["since"], "2026-10-01T00:00:00Z");
        let atom = fs::read_to_string(dir.join("out/updates.atom")).unwrap();
        assert!(atom.contains("<title>foo 1.0 → 1.1</title>"));
        assert!(atom.contains("https://github.com/syui/aios/blob/main/pkg/c/foo/PKGBUILD"));
        assert!(atom.contains("<title>pnpm 12.9.1 → 12.16.1 (npm)</title>"));
        assert!(!atom.contains("bar"));
        assert!(atom.contains("<title>Linux 7.4 (stable) is out; the reference is 7.4-rc1</title>"));
        assert!(atom.contains("<title>CI unix fails on main at abc1234</title>"));
        assert!(atom.contains("<title>[kernel] robust futex</title>") && atom.contains("<published>2026-11-02T00:00:00Z</published>"));
        let _ = fs::remove_dir_all(&dir);
    }
}
