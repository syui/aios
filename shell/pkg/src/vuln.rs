// 脆弱性 (CVE) の見はり (pkg_check の vulns): aios のパッケージの版 (pkgver) を、「この版で直した」の一覧とくらべる
//   Alpine Linux の secdb (https://secdb.alpinelinux.org/edge/ の main.json と community.json。パッケージごとに
//   「この版 (VER-rN) で直した CVE」)。pkg/c のもの (配布元のソースから作るもの) と、Alpine の同じ名前のものと同じものと
//   わかっているもの (ALSO)
//     newer: 直した版のほうが新しい (配布元の新しい版で直っている。pkg_edit で上げる)
//     patch: Alpine が同じ版にパッチをあてて直している (VER-r1 から)。aios に同じパッチがなければ直っていない
//   Mozilla の advisories (MOZILLA の firefox。Alpine は firefox の secfixes を書かず、Arch の security tracker は
//   2023 年からほとんど動いていない (firefox は 2019 年の 67.0.4 まで) ので、配布元のもの):
//   github.com/mozilla/foundation-security-advisories の announce/YEAR/mfsaYEAR-N.yml の fixed_in の「Firefox VER」と
//   その advisories の CVE。git で浅く、ことしと去年のところだけ取る (2026 年だけで 110 ほど、CVE は 2600 ほど)
// PKGBUILD かそのとなりのファイル (パッチなど) に CVE の番号を書いておけば、直した (かあてはまらない) とみて出さない。
// pkg/rust の coreutils (uutils) や tar (aios のもの) は名前が同じでも別のものなので見ない。名前のちがうもの (glib2 は
// glib ...) は ALIAS で。どこも書いていないもの (mesa など) は見られない
// どちらも 1 日に 1 回取ってきて build/aish-pkg/secdb/ に置く。取れなければ前のもので (取りなおすのは 1 時間に 1 回まで)
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

const SECDB: [&str; 2] = ["https://secdb.alpinelinux.org/edge/main.json", "https://secdb.alpinelinux.org/edge/community.json"];
/// aios (Arch の名前) と Alpine で名前のちがうもの
const ALIAS: [(&str, &str); 3] = [("glib2", "glib"), ("gdk-pixbuf2", "gdk-pixbuf"), ("freetype2", "freetype")];
/// pkg/c のほかで、Alpine の同じ名前のものと同じもの
const ALSO: [&str; 1] = ["sudo-rs"];
/// Mozilla の advisories で見るもの: (aios の名前, fixed_in の名前)。Firefox ESR や Firefox for iOS は別のもの
const MOZILLA: [(&str, &str); 1] = [("firefox", "Firefox")];
const MFSA: &str = "https://github.com/mozilla/foundation-security-advisories.git";

const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// Alpine の名前 → (版 VER-rN → CVE)
type Db = HashMap<String, BTreeMap<String, Vec<String>>>;

/// 直っていない CVE のまとまり
#[derive(Debug, PartialEq)]
pub struct Fix {
    /// newer (新しい版で直っている) か patch (Alpine が同じ版にパッチをあてて直している)
    pub how: &'static str,
    /// 直した版 (newer) か、パッチをあてた Alpine の版 VER-rN (patch)
    pub version: String,
    /// どこで知ったか: alpine か、Mozilla の advisory (mfsa2026-106)
    pub source: String,
    pub cves: Vec<String>,
}

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

/// 取れなかったものを使っているときの 1 行: what を N 日前のもので (取れなかったわけ)
fn stale_note(what: &str, old: Duration, why: &str) -> String {
    format!("using the {} from {} days ago (could not fetch it{}{})", what, old.as_secs() / 86400, if why.is_empty() { "" } else { ": " }, why.trim())
}

/// secdb を読む (1 日たったものは取ってくる)。取れなかったら前のものと、そう書いた 1 行
fn secdb(root: &Path) -> Result<(Db, String), String> {
    let dir = root.join("build/aish-pkg/secdb");
    let _ = fs::create_dir_all(&dir);
    // 取れなかったときに、見るたびに待たないように (ネットのない aios など)。中は取れなかったわけ (取れたら空)
    let tried = dir.join("tried");
    let may = age(&tried).is_none_or(|a| a > HOUR);
    let (mut db, mut note, mut touched) = (Db::new(), String::new(), false);
    for url in SECDB {
        let f = dir.join(url.rsplit('/').next().unwrap_or("secdb.json"));
        if may && age(&f).is_none_or(|a| a > DAY) {
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
    if old > DAY {
        let why = if note.is_empty() { fs::read_to_string(&tried).unwrap_or_default() } else { note };
        note = stale_note("Alpine secdb", old, &why);
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
        let (how, version) = match crate::up::vercmp(now, pre.map_or(ver, |i| &ver[..i])) {
            Less => ("newer", ver.to_string()),
            Equal if rel > 0 && pre.is_none() => ("patch", fix.clone()),
            _ => continue,
        };
        let left: Vec<String> = cves.iter().filter(|c| !done.contains(*c)).cloned().collect();
        if !left.is_empty() {
            out.push(Fix { how, version, source: "alpine".into(), cves: left });
        }
    }
    out
}

/// いまの年 (1970 年 1 月 1 日からの日の数から)
fn year_now() -> i64 {
    let days = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86400) as i64;
    // H. Hinnant の civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    yoe + era * 400 + i64::from(mp >= 10)
}

fn git(args: &[&str]) -> Result<(), String> {
    let o = Command::new("git").args(args).env("GIT_TERMINAL_PROMPT", "0").output().map_err(|e| format!("git: {}", e))?;
    if o.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&o.stderr);
    Err(format!("git {}: {}", args.iter().find(|a| !a.starts_with('-') && **a != "-C").unwrap_or(&""), err.trim().lines().last().unwrap_or("failed")))
}

/// Mozilla の advisories (mfsa の id, YAML) を読む。1 日たっていれば git で取りなおす (はじめは浅く、ファイルの中身は
/// ことしと去年のところだけの clone)。取れなかったら前のものと、そう書いた 1 行
fn mfsa(root: &Path) -> Result<(Vec<(String, String)>, String), String> {
    let base = root.join("build/aish-pkg/secdb");
    let _ = fs::create_dir_all(&base);
    let (repo, ok, tried) = (base.join("mfsa"), base.join("mfsa.fetched"), base.join("mfsa.tried"));
    let y = year_now();
    let years = [format!("announce/{}", y - 1), format!("announce/{}", y)];
    let mut note = String::new();
    if age(&ok).is_none_or(|a| a > DAY) && age(&tried).is_none_or(|a| a > HOUR) {
        let r = repo.to_str().unwrap_or("");
        let got = if repo.join(".git").is_dir() {
            git(&["-C", r, "fetch", "-q", "--depth", "1", "origin", "HEAD"]).and_then(|_| git(&["-C", r, "reset", "-q", "--hard", "FETCH_HEAD"]))
        } else {
            let _ = fs::remove_dir_all(&repo);
            git(&["clone", "-q", "--depth", "1", "--filter=blob:none", "--sparse", MFSA, r])
        }
        .and_then(|_| git(&["-C", r, "sparse-checkout", "set", &years[0], &years[1]]));
        match got {
            Ok(()) => {
                let _ = fs::write(&ok, "");
                let _ = fs::remove_file(&tried);
            }
            Err(e) => {
                let _ = fs::write(&tried, &e);
                note = e;
            }
        }
    }
    let mut out = Vec::new();
    for y in &years {
        for e in fs::read_dir(repo.join(y)).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "yml")
                && let Ok(t) = fs::read_to_string(&p)
            {
                out.push((p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), t));
            }
        }
    }
    if out.is_empty() {
        return Err(format!("no Mozilla advisories in {}{}", repo.display(), if note.is_empty() { String::new() } else { format!(" ({})", note) }));
    }
    match age(&ok) {
        Some(a) if a > DAY => note = stale_note("Mozilla advisories", a, &if note.is_empty() { fs::read_to_string(&tried).unwrap_or_default() } else { note }),
        _ => note.clear(),
    }
    Ok((out, note))
}

/// 1 つの advisory (mfsaYEAR-N.yml): fixed_in の product の版 ("- Firefox 158.0.1"。"- Firefox ESR 140.17" は別のもの) と、
/// advisories のキーの CVE ("  CVE-2026-109704:")
fn mfsa_file(text: &str, product: &str) -> (Vec<String>, Vec<String>) {
    let (mut fixed, mut cves, mut list) = (Vec::new(), Vec::new(), false);
    let item = format!("- {} ", product);
    for l in text.lines() {
        if l.starts_with("fixed_in:") {
            list = true;
            continue;
        }
        if list {
            if l.starts_with("- ") {
                if let Some(v) = l.strip_prefix(&item).map(str::trim).filter(|v| v.starts_with(|c: char| c.is_ascii_digit())) {
                    fixed.push(v.to_string());
                }
                continue;
            }
            list = false;
        }
        if let Some(k) = l.strip_prefix("  ").and_then(|k| k.trim_end().strip_suffix(':')).filter(|k| k.starts_with("CVE-")) {
            cves.push(k.to_string());
        }
    }
    (fixed, cves)
}

/// Mozilla の advisories のうち、product の now (pkgver) で直っていないもの (直した版がいまより新しいもの)
fn open_mfsa(now: &str, product: &str, advs: &[(String, String)], done: &BTreeSet<String>) -> Vec<Fix> {
    let mut out: Vec<Fix> = Vec::new();
    for (id, text) in advs {
        let (fixed, cves) = mfsa_file(text, product);
        // いまより新しい版のうち、いちばん古いもの (いくつもの版の線に入れたもの)
        let Some(v) = fixed.iter().filter(|v| crate::up::vercmp(now, v).is_lt()).min_by(|a, b| crate::up::vercmp(a, b)) else { continue };
        let left: Vec<String> = cves.into_iter().filter(|c| !done.contains(c)).collect();
        if !left.is_empty() {
            out.push(Fix { how: "newer", version: v.clone(), source: id.clone(), cves: left });
        }
    }
    out.sort_by(|a, b| crate::up::vercmp(&a.version, &b.version).then_with(|| a.source.cmp(&b.source)));
    out
}

/// pkg/c のもの (と ALSO) の直っていない CVE (names があればそれだけ)。答えと、説明の 1 行 (取れなかったときなど)
pub fn check(root: &Path, names: &[String]) -> Result<(Vec<Vuln>, String), String> {
    let (db, note) = secdb(root)?;
    let mut notes = vec![note];
    // Mozilla の advisories は、それを見るパッケージがあるときだけ取る
    let mut moz: Option<Vec<(String, String)>> = None;
    let mut out = Vec::new();
    for (name, pb) in crate::up::pkgbuilds(&root.join("pkg")) {
        let dir: PathBuf = pb.parent().unwrap_or(Path::new(".")).to_path_buf();
        let kind = dir.parent().and_then(|d| d.file_name()).map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        if !(names.is_empty() || names.contains(&name)) || !(kind == "c" || ALSO.contains(&name.as_str())) {
            continue;
        }
        let alpine = db.get(ALIAS.iter().find(|(a, _)| *a == name).map_or(name.as_str(), |(_, b)| b));
        let product = MOZILLA.iter().find(|(n, _)| *n == name).map(|(_, p)| *p);
        if alpine.is_none() && product.is_none() {
            continue;
        }
        // [epoch:]pkgver-pkgrel の pkgver (pkgver() で決めるものは "" なので見ない)
        let ver = crate::up::version(&pb);
        let ver = ver.rsplit(':').next().unwrap_or(&ver);
        let now = ver.rsplit_once('-').map_or(ver, |(v, _)| v);
        if now.is_empty() {
            continue;
        }
        // PKGBUILD とパッチなどに書いてある CVE は直したとみる
        let mut done = BTreeSet::new();
        for e in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if let Ok(t) = fs::read_to_string(e.path()) {
                done.extend(cves_in(&t));
            }
        }
        let mut fixes = alpine.map(|s| open(now, s, &done)).unwrap_or_default();
        if let Some(p) = product {
            if moz.is_none() {
                moz = Some(match mfsa(root) {
                    Ok((a, n)) => {
                        notes.push(n);
                        a
                    }
                    Err(e) => {
                        notes.push(e);
                        Vec::new()
                    }
                });
            }
            fixes.extend(open_mfsa(now, p, moz.as_deref().unwrap_or_default(), &done));
        }
        if !fixes.is_empty() {
            out.push(Vuln { name, now: now.to_string(), fixes });
        }
    }
    notes.retain(|n| !n.is_empty());
    Ok((out, notes.join("; ")))
}

/// 1 行で: 2.8.5: CVE-a CVE-b fixed in 2.9.0; CVE-c patched by Alpine in 1.2.6-r1。CVE が多ければ 4 つと残りの数
/// (Mozilla は 1 つのバグに 1 つの CVE なので、1 つの版に何十も)
pub fn describe(v: &Vuln) -> String {
    let list = |c: &[String]| if c.len() > 4 { format!("{} and {} more", c[..4].join(" "), c.len() - 4) } else { c.join(" ") };
    let parts: Vec<String> = v
        .fixes
        .iter()
        .map(|f| match f.how {
            "newer" if f.source == "alpine" => format!("{} fixed in {}", list(&f.cves), f.version),
            "newer" => format!("{} fixed in {} ({})", list(&f.cves), f.version, f.source),
            _ => format!("{} patched by Alpine in {}", list(&f.cves), f.version),
        })
        .collect();
    format!("{}: {}", v.now, parts.join("; "))
}

pub fn to_json(vs: &[Vuln]) -> Value {
    json!(vs.iter().map(|v| json!({ "name": v.name, "now": v.now, "fixes": v.fixes.iter().map(|f| json!({ "how": f.how, "version": f.version, "source": f.source, "cves": f.cves })).collect::<Vec<_>>() })).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(how: &'static str, version: &str, source: &str, cves: &[&str]) -> Fix {
        Fix { how, version: version.into(), source: source.into(), cves: cves.iter().map(|c| c.to_string()).collect() }
    }

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
        let a = |h, v, c| fix(h, v, "alpine", &[c]);
        assert_eq!(o, vec![a("patch", "1.2.6-r1", "CVE-2026-6042"), a("patch", "1.2.6-r2", "CVE-2026-40200"), a("newer", "1.2.7_rc1", "CVE-2026-0006"), a("newer", "1.2.8", "CVE-2026-0004")]);
        // 1.2.7 には 1.2.7_rc1 で直したものが入っている
        assert_eq!(open("1.2.7", &s, &BTreeSet::new()), vec![a("newer", "1.2.8", "CVE-2026-0004")]);
        assert!(open("1.2.8", &s, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn mozilla_advisories() {
        let a = "## mfsa2026-106.yml\nannounced: October 13, 2026\nfixed_in:\n- Firefox 158\n- Firefox ESR 153.5\n- Firefox for iOS 158\ntitle: x\nadvisories:\n  CVE-2026-1001:\n    title: a\n    bugs:\n      - url: 1\n  CVE-2026-1002:\n    title: b\n";
        assert_eq!(mfsa_file(a, "Firefox"), (vec!["158".to_string()], vec!["CVE-2026-1001".to_string(), "CVE-2026-1002".to_string()]));
        let b = "fixed_in:\n- Firefox 157.0.2\n- Firefox 158.0.1\nadvisories:\n  CVE-2026-2001:\n    title: c\n";
        let advs = vec![("mfsa2026-106".to_string(), a.to_string()), ("mfsa2026-112".to_string(), b.to_string())];
        // 158.0 は 158 (の advisory) で直っていて、158.0.1 のものだけ
        assert_eq!(open_mfsa("158.0", "Firefox", &advs, &BTreeSet::new()), vec![fix("newer", "158.0.1", "mfsa2026-112", &["CVE-2026-2001"])]);
        // 157.0.1 にはどちらも。いまより新しいうちのいちばん古い版 (157.0.2)
        assert_eq!(open_mfsa("157.0.1", "Firefox", &advs, &BTreeSet::new()), vec![fix("newer", "157.0.2", "mfsa2026-112", &["CVE-2026-2001"]), fix("newer", "158", "mfsa2026-106", &["CVE-2026-1001", "CVE-2026-1002"])]);
        assert!(open_mfsa("158.0.1", "Firefox", &advs, &BTreeSet::new()).is_empty());
        // ESR だけの advisory は Firefox のものではない
        assert!(mfsa_file("fixed_in:\n- Firefox ESR 140.17\nadvisories:\n  CVE-2026-3001:\n", "Firefox").0.is_empty());
    }

    #[test]
    fn this_year() {
        let y = year_now();
        assert!((2024..2200).contains(&y), "{}", y);
    }

    #[test]
    fn cves_in_text() {
        let c = cves_in("# CVE-2025-26519 (iconv) のパッチは 1.2.6 に入った\nCVE-2026-40200.patch CVE-12 CVE-2021-27219 GHSL-2021-045");
        assert_eq!(c.into_iter().collect::<Vec<_>>(), ["CVE-2021-27219", "CVE-2025-26519", "CVE-2026-40200"]);
    }

    #[test]
    fn describe_fixes() {
        let v = Vuln { name: "musl".into(), now: "1.2.6".into(), fixes: vec![fix("patch", "1.2.6-r1", "alpine", &["CVE-2026-6042"]), fix("newer", "1.2.7", "alpine", &["CVE-a", "CVE-b"])] };
        assert_eq!(describe(&v), "1.2.6: CVE-2026-6042 patched by Alpine in 1.2.6-r1; CVE-a CVE-b fixed in 1.2.7");
        let f = Vuln { name: "firefox".into(), now: "158.0".into(), fixes: vec![fix("newer", "158.0.1", "mfsa2026-112", &["CVE-1", "CVE-2", "CVE-3", "CVE-4", "CVE-5", "CVE-6"])] };
        assert_eq!(describe(&f), "158.0: CVE-1 CVE-2 CVE-3 CVE-4 and 2 more fixed in 158.0.1 (mfsa2026-112)");
    }
}
