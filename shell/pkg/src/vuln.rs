// 脆弱性 (CVE) の見はり (pkg_check の vulns): Alpine Linux の secdb (https://secdb.alpinelinux.org/edge/ の main.json と
// community.json。パッケージごとに「この版 (VER-rN) で直した CVE」) と、aios のパッケージの版 (pkgver) をくらべる
//   newer: 直した版のほうが新しい (配布元の新しい版で直っている。pkg_edit で上げる)
//   patch: Alpine が同じ版にパッチをあてて直している (VER-r1 から)。aios に同じパッチがなければ直っていない
//   PKGBUILD かそのとなりのファイル (パッチなど) に CVE の番号を書いておけば、直した (かあてはまらない) とみて出さない
// 見るのは pkg/c のもの (配布元のソースから作るもの) と、Alpine の同じ名前のものと同じものとわかっているもの (ALSO)。
// pkg/rust の coreutils (uutils) や tar (aios のもの) は名前が同じでも別のものなので見ない。名前のちがうもの (glib2 は
// glib ...) は ALIAS で。Alpine が secfixes を書いていないもの (firefox、mesa など) は見られない
// secdb は 1 日に 1 回取ってきて build/aish-pkg/secdb/ に置く。取れなければ前のもので (取りなおすのは 1 時間に 1 回まで)
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

const SECDB: [&str; 2] = ["https://secdb.alpinelinux.org/edge/main.json", "https://secdb.alpinelinux.org/edge/community.json"];
/// aios (Arch の名前) と Alpine で名前のちがうもの
const ALIAS: [(&str, &str); 3] = [("glib2", "glib"), ("gdk-pixbuf2", "gdk-pixbuf"), ("freetype2", "freetype")];
/// pkg/c のほかで、Alpine の同じ名前のものと同じもの
const ALSO: [&str; 1] = ["sudo-rs"];

/// Alpine の名前 → (版 VER-rN → CVE)
type Db = HashMap<String, BTreeMap<String, Vec<String>>>;
/// (newer か patch, Alpine の版 VER-rN, CVE)
pub type Fix = (&'static str, String, Vec<String>);

/// 1 つのパッケージの直っていない CVE
#[derive(Debug, PartialEq)]
pub struct Vuln {
    pub name: String,
    /// aios の版 (pkgver)
    pub now: String,
    pub fixes: Vec<Fix>,
}

/// テキストの中の CVE-YYYY-N
fn cves_in(text: &str) -> BTreeSet<String> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"CVE-\d{4}-\d{4,}").expect("regex"));
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

/// ファイルが書かれてからどれだけたったか
fn age(p: &Path) -> Option<Duration> {
    fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok())
}

/// secdb を読む (1 日たったものは取ってくる)。取れなかったら前のものと、そう書いた 1 行
fn secdb(root: &Path) -> Result<(Db, String), String> {
    let dir = root.join("build/aish-pkg/secdb");
    let _ = fs::create_dir_all(&dir);
    // 取れなかったときに、見るたびに待たないように (ネットのない aios など)。中は取れなかったわけ (取れたら空)
    let tried = dir.join("tried");
    let may = age(&tried).is_none_or(|a| a > Duration::from_secs(3600));
    let (mut db, mut note, mut touched) = (Db::new(), String::new(), false);
    for url in SECDB {
        let f = dir.join(url.rsplit('/').next().unwrap_or("secdb.json"));
        if may && age(&f).is_none_or(|a| a > Duration::from_secs(24 * 3600)) {
            touched = true;
            match crate::up::get(url).and_then(|t| serde_json::from_str::<Value>(&t).map(|_| t).map_err(|e| format!("{}: {}", url, e))) {
                Ok(t) => {
                    let part = f.with_extension("json.part");
                    if fs::write(&part, t).is_ok() {
                        let _ = fs::rename(&part, &f);
                    }
                }
                Err(e) => note = e,
            }
        }
        let text = fs::read_to_string(&f).map_err(|e| format!("{}: {}{}", f.display(), e, if note.is_empty() { String::new() } else { format!(" ({})", note) }))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {}", f.display(), e))?;
        for p in v["packages"].as_array().into_iter().flatten() {
            let Some(name) = p["pkg"]["name"].as_str() else { continue };
            let fixes = db.entry(name.to_string()).or_default();
            for (ver, cves) in p["pkg"]["secfixes"].as_object().into_iter().flatten() {
                // "CVE-2021-27219 GHSL-2021-045" のように、ほかの番号とならんでいるものもある
                fixes.entry(ver.clone()).or_default().extend(cves.as_array().into_iter().flatten().filter_map(|c| c.as_str()).flat_map(cves_in));
            }
        }
    }
    if touched {
        let _ = fs::write(&tried, &note);
    }
    // 1 日より古いものを使っていれば (いま取れなかったか、1 時間のうちに取れなかったので取りに行かなかった)
    let old = SECDB.iter().filter_map(|u| age(&dir.join(u.rsplit('/').next().unwrap_or("")))).max().unwrap_or_default();
    if old > Duration::from_secs(24 * 3600) {
        let why = if note.is_empty() { fs::read_to_string(&tried).unwrap_or_default() } else { note };
        note = format!("using the Alpine secdb from {} days ago (could not fetch it{}{})", old.as_secs() / 86400, if why.is_empty() { "" } else { ": " }, why.trim());
    } else {
        note.clear();
    }
    Ok((db, note))
}

/// 1 つのパッケージ: secfixes (Alpine の版 VER-rN → CVE) のうち、aios の版 now (pkgver) で直っていないもの。done は直したと
/// 書いてある CVE
fn open(now: &str, secfixes: &BTreeMap<String, Vec<String>>, done: &BTreeSet<String>) -> Vec<Fix> {
    use std::cmp::Ordering::*;
    let mut out = Vec::new();
    for (fix, cves) in secfixes {
        // "0" はその CVE が Alpine のものにはあてはまらないという印
        let (ver, rel) = fix.rsplit_once("-r").map_or((fix.as_str(), 0), |(v, r)| (v, r.parse::<u32>().unwrap_or(0)));
        if ver == "0" {
            continue;
        }
        // 1.2.3_rc1 などは出る前のもの: 1.2.3 にはみな入っている
        let pre = ["_rc", "_alpha", "_beta", "_pre"].iter().find_map(|s| ver.find(s));
        let how = match crate::up::vercmp(now, pre.map_or(ver, |i| &ver[..i])) {
            Less => "newer",
            Equal if rel > 0 && pre.is_none() => "patch",
            _ => continue,
        };
        let left: Vec<String> = cves.iter().filter(|c| !done.contains(*c)).cloned().collect();
        if !left.is_empty() {
            out.push((how, fix.clone(), left));
        }
    }
    out
}

/// pkg/c のもの (と ALSO) の直っていない CVE (names があればそれだけ)。答えと、説明の 1 行 (secdb を取れなかったときなど)
pub fn check(root: &Path, names: &[String]) -> Result<(Vec<Vuln>, String), String> {
    let (db, note) = secdb(root)?;
    let mut out = Vec::new();
    for (name, pb) in crate::up::pkgbuilds(&root.join("pkg")) {
        let dir = pb.parent().unwrap_or(Path::new("."));
        let kind = dir.parent().and_then(|d| d.file_name()).map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        if !(names.is_empty() || names.contains(&name)) || !(kind == "c" || ALSO.contains(&name.as_str())) {
            continue;
        }
        let alpine = ALIAS.iter().find(|(a, _)| *a == name).map_or(name.as_str(), |(_, b)| b);
        let Some(secfixes) = db.get(alpine) else { continue };
        // [epoch:]pkgver-pkgrel の pkgver (pkgver() で決めるものは "" なので見ない)
        let ver = crate::up::version(&pb);
        let ver = ver.rsplit(':').next().unwrap_or(&ver);
        let now = ver.rsplit_once('-').map_or(ver, |(v, _)| v);
        if now.is_empty() {
            continue;
        }
        // PKGBUILD とパッチなどに書いてある CVE は直したとみる
        let mut done = BTreeSet::new();
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            if let Ok(t) = fs::read_to_string(e.path()) {
                done.extend(cves_in(&t));
            }
        }
        let fixes = open(now, secfixes, &done);
        if !fixes.is_empty() {
            out.push(Vuln { name, now: now.to_string(), fixes });
        }
    }
    Ok((out, note))
}

/// 1 行で: 2.8.5: CVE-a CVE-b fixed in 2.9.0; CVE-c (1.2.6-r1) patched by Alpine
pub fn describe(v: &Vuln) -> String {
    let mut parts = Vec::new();
    for (how, fix, cves) in &v.fixes {
        parts.push(match *how {
            "newer" => format!("{} fixed in {}", cves.join(" "), fix.rsplit_once("-r").map_or(fix.as_str(), |(f, _)| f)),
            _ => format!("{} patched by Alpine in {}", cves.join(" "), fix),
        });
    }
    format!("{}: {}", v.now, parts.join("; "))
}

pub fn to_json(vs: &[Vuln]) -> Value {
    json!(vs.iter().map(|v| json!({ "name": v.name, "now": v.now, "fixes": v.fixes.iter().map(|(h, f, c)| json!({ "how": h, "alpine": f, "cves": c })).collect::<Vec<_>>() })).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_fixes() {
        let mut s: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut add = |k: &str, v: &[&str]| s.insert(k.into(), v.iter().map(|c| c.to_string()).collect());
        add("0", &["CVE-2020-0001"]);
        add("1.2.5-r0", &["CVE-2024-0002"]);
        add("1.2.6-r0", &["CVE-2024-0005"]);
        add("1.2.6-r1", &["CVE-2026-6042"]);
        add("1.2.6-r2", &["CVE-2026-40200", "CVE-2026-0003"]);
        add("1.2.7_rc1-r0", &["CVE-2026-0006"]);
        add("1.2.8-r0", &["CVE-2026-0004"]);
        // 1.2.6: 1.2.5 と 1.2.6-r0 のものは直っている。1.2.6 のパッチのもの (書いてあるものはのぞく) と、その先のものが出る
        let done: BTreeSet<String> = ["CVE-2026-0003".to_string()].into();
        let o = open("1.2.6", &s, &done);
        let f = |h: &'static str, v: &str, c: &str| (h, v.to_string(), vec![c.to_string()]);
        assert_eq!(o, vec![f("patch", "1.2.6-r1", "CVE-2026-6042"), f("patch", "1.2.6-r2", "CVE-2026-40200"), f("newer", "1.2.7_rc1-r0", "CVE-2026-0006"), f("newer", "1.2.8-r0", "CVE-2026-0004")]);
        // 1.2.7 には 1.2.7_rc1 で直したものが入っている
        assert_eq!(open("1.2.7", &s, &BTreeSet::new()), vec![f("newer", "1.2.8-r0", "CVE-2026-0004")]);
        assert!(open("1.2.8", &s, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn cves_in_text() {
        let c = cves_in("# CVE-2025-26519 (iconv) のパッチは 1.2.6 に入った\nCVE-2026-40200.patch CVE-12 CVE-2021-27219 GHSL-2021-045");
        assert_eq!(c.into_iter().collect::<Vec<_>>(), ["CVE-2021-27219", "CVE-2025-26519", "CVE-2026-40200"]);
    }

    #[test]
    fn describe_fixes() {
        let v = Vuln { name: "musl".into(), now: "1.2.6".into(), fixes: vec![("patch", "1.2.6-r1".into(), vec!["CVE-2026-6042".into()]), ("newer", "1.2.7-r0".into(), vec!["CVE-a".into(), "CVE-b".into()])] };
        assert_eq!(describe(&v), "1.2.6: CVE-2026-6042 patched by Alpine in 1.2.6-r1; CVE-a CVE-b fixed in 1.2.7");
    }
}
