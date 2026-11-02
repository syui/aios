// 毎日の見はり (aish-pkg report DIR [PREV]): check をまるごと回して、DIR に 2 つ書く
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

/// answer は check の答え (text など)、rs は up::check の 1 つずつ、vulns は直っていないもの
pub fn write(root: &Path, out: &Path, prev: Option<&Path>, answer: &Value, rs: &[Value], vulns: &[vuln::Vuln]) -> Result<Value, String> {
    let at = now();
    // 前の報告の since (id → いつ)
    let old: Value = prev.and_then(|p| fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut since: HashMap<String, String> = HashMap::new();
    for k in ["new", "not_out", "vulns"] {
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

    let mut report = json!({
        "date": at,
        "new": new,
        "not_out": not_out,
        "vulns": vj,
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
    atom.push_str("  <title>aios: packages to update</title>\n");
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let r = write(&dir, &dir.join("out"), Some(&prev), &json!({ "text": "t" }), &rs, &[]).unwrap();
        assert_eq!((r["new"].as_u64(), r["not_out"].as_u64(), r["errors"].as_u64(), r["entries"].as_u64()), (Some(2), Some(1), Some(1), Some(2)));
        let j: Value = serde_json::from_str(&fs::read_to_string(dir.join("out/updates.json")).unwrap()).unwrap();
        assert_eq!(j["new"][0]["since"], "2026-10-01T00:00:00Z");
        let atom = fs::read_to_string(dir.join("out/updates.atom")).unwrap();
        assert!(atom.contains("<title>foo 1.0 → 1.1</title>"));
        assert!(atom.contains("https://github.com/syui/aios/blob/main/pkg/c/foo/PKGBUILD"));
        assert!(atom.contains("<title>pnpm 12.9.1 → 12.16.1 (npm)</title>"));
        assert!(!atom.contains("bar"));
        let _ = fs::remove_dir_all(&dir);
    }
}
